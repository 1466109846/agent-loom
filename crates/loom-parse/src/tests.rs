use super::{Declaration, Extractor, Parsed, Reference, language};

mod aliases;
mod languages;

fn parse(path: &str, source: &str) -> Parsed {
    let parsed = Extractor::default().extract(path, source).unwrap();
    assert!(!parsed.syntax_error, "合成样例应可解析：{path}");
    parsed
}

fn refs<'p>(parsed: &'p Parsed, kind: &str) -> Vec<&'p Reference> {
    parsed.refs.iter().filter(|r| r.kind == kind).collect()
}

fn has_candidate(reference: &Reference, candidate: &str) -> bool {
    reference.candidates.iter().any(|p| p == candidate)
}

fn symbols(parsed: &Parsed) -> Vec<(&str, usize)> {
    parsed
        .symbols
        .iter()
        .map(|Declaration { name, line, .. }| (name.as_str(), *line))
        .collect()
}

#[test]
fn shared_extractor_switches_grammars_and_does_not_carry_source_state() {
    let mut extractor = Extractor::default();
    for (path, lang, src) in [
        ("lib.rs", "rust", "pub fn save() {}"),
        (
            "u.jsx",
            "javascript",
            "export function save(){ return <p/> }",
        ),
        ("u.js", "javascript", "export function save(){}"),
        (
            "u.tsx",
            "typescript",
            "export function save(): JSX.Element { return <p/> }",
        ),
        ("u.ts", "typescript", "export function save<T>(): void {}"),
        ("u.py", "python", "def save(): pass"),
    ] {
        assert_eq!(language(path), Some(lang));
        let p = extractor.extract(path, src).unwrap();
        assert!(!p.syntax_error);
        assert_eq!(symbols(&p), [("save", 1)]);
        assert!(p.refs.is_empty());
    }
    assert_eq!(language("no-parser.txt"), None);
    assert!(extractor.extract("no-parser.txt", "").is_err());
}

#[test]
fn rust_file_modules_keep_both_layouts_and_public_names_only() {
    let p = parse(
        "lib.rs",
        "// mod wrong;\npub mod writer;\nmod private;\npub mod inline { pub fn inner() {} }\npub(crate) fn restricted() {}\npub fn run() {}\n",
    );
    let modules = refs(&p, "module");
    assert_eq!(modules.len(), 2);
    assert_eq!(modules[0].line, 2);
    assert!(modules[0].exact);
    assert_eq!(modules[0].candidates, ["writer.rs", "writer/mod.rs"]);
    assert_eq!(symbols(&p), [("writer", 2), ("inline", 4), ("run", 6)]);
    for (path, candidate) in [
        ("src/entry.rs", "src/entry/item.rs"),
        ("src/pkg/mod.rs", "src/pkg/item.rs"),
        ("src/main.rs", "src/item.rs"),
    ] {
        assert!(has_candidate(&parse(path, "mod item;").refs[0], candidate));
    }
}

#[test]
fn rust_uses_bind_to_declared_modules_not_to_aliases_or_inferred_item_files() {
    let p = parse(
        "src/lib.rs",
        "pub use crate::writer::save as store;\nuse self::writer::{a,b};\nuse store::x;\nuse missing::x;\nuse crate::inline::x;\nmod inline {}\nmod writer;\nextern crate alloc as memory;\n",
    );
    let r = &p.refs;
    assert_eq!(r[0].kind, "unsupported"); // 别名仍可展示，不当成终端声明。
    assert_eq!(r[0].line, 1);
    assert!(r[0].rust_paths.is_empty());
    assert_eq!(r[1].rust_paths, ["self::writer::a", "self::writer::b"]);
    assert!(r[..5].iter().all(|x| !x.exact && x.candidates.is_empty()));
    assert!(r[2..4].iter().all(|x| x.rust_paths.is_empty()));
    assert!(p.rust_modules.iter().all(|m| m.name != "inline"));
    assert_eq!(refs(&p, "external")[0].spelling, "alloc");
    assert!(symbols(&p).contains(&("store", 1)));
}

#[test]
fn rust_path_overrides_cfg_and_macro_arguments_do_not_invent_default_edges() {
    let p = parse(
        "src/host.rs",
        "#[path = \"../shared/real.rs\"] // keep attribute attached\npub mod mapped;\n#[cfg_attr(test, path = \"test.rs\")]\nmod conditional;\n#[cfg(unix)] mod only;\n#[path = \"../../escape.rs\"] mod escape;\n#[custom] mod rewritten;\nuse self::conditional::x;\ninclude!(concat!(\"PRIVATE_BODY\", env!(\"USER\")));\n",
    );
    let known: Vec<_> = p.refs.iter().filter(|r| r.exact).collect();
    assert_eq!(known.len(), 1);
    assert_eq!(known[0].candidates, ["shared/real.rs"]);
    assert_eq!(known[0].line, 2);
    assert!(p.refs.iter().all(|r| !r.spelling.contains("PRIVATE_BODY")));
    assert_eq!(symbols(&p), [("mapped", 2)]);
    let gated = parse(
        "src/lib.rs",
        "#![cfg(feature = \"x\")]\npub mod a;\npub use self::a::f;",
    );
    assert!(gated.symbols.is_empty() && gated.refs.iter().all(|r| !r.exact));
}

#[test]
fn js_relative_imports_and_reexports_keep_all_candidates_without_files() {
    let p = parse(
        "src/entry.ts",
        "// import './bogus';\nimport {save} from './writer';\nexport {save as keep} from './sub/../store';\nimport './store.js';\nexport function save(){}\nfunction hidden(){}\n",
    );
    assert_eq!(p.refs.len(), 3);
    assert_eq!((p.refs[0].kind, p.refs[0].line), ("import", 2));
    assert!(has_candidate(&p.refs[0], "src/writer.ts"));
    let r = &p.refs[1];
    assert_eq!((r.kind, r.line, r.exact), ("reexport", 3, false));
    for name in ["src/store.ts", "src/store.js", "src/store/index.tsx"] {
        assert!(has_candidate(r, name));
    }
    assert!(
        r.candidates
            .iter()
            .all(|s| !s.contains("..") && !s.contains('\\'))
    );
    assert_eq!(p.refs[2].candidates, ["src/store.js"]);
    assert_eq!(symbols(&p), [("keep", 3), ("save", 5)]);
}

#[test]
fn javascript_loader_expressions_and_payloads_never_become_local_edges() {
    let p = parse(
        "src/entry.tsx",
        r#"import('node:fs');
import '#path';
import '@/writer';
import 'app/writer';
require('./secret?PRIVATE_BODY');
if (ok) import(`./${PRIVATE_BODY}`);
require(/* comment */ PRIVATE_BODY, './not-the-first');
import('./' + PRIVATE_BODY);
import(`./\x50RIVATE_BODY`);
require('./\x50RIVATE_BODY');
const note = "import('./PRIVATE_BODY')";
require.resolve('./x'); require.context('./x'); module.require('./x');
export function View(){ return <p/> }
"#,
    );
    assert_eq!(
        (p.refs[0].kind, p.refs[0].spelling.as_str()),
        ("external", "node:fs")
    );
    assert_eq!(p.refs.len(), 10);
    assert!(p.refs[1..].iter().all(|r| r.kind == "unsupported"));
    // 读不成模块名的说明符不回显，只留下它是哪一种加载。
    let spellings: Vec<_> = p.refs.iter().map(|r| r.spelling.as_str()).collect();
    assert_eq!(
        spellings,
        [
            "node:fs",
            "#path",
            "@/writer",
            "app/writer",
            "require",
            "import",
            "require",
            "import",
            "import",
            "require"
        ]
    );
    assert!(p.refs.iter().all(|r| !r.exact
        && r.candidates.is_empty()
        && r.substitutes.is_empty()
        && r.fallbacks.is_empty()
        && !r.spelling.contains("PRIVATE_BODY")));
    assert_eq!(symbols(&p), [("View", 13)]);
}

#[test]
fn literal_loaders_share_static_candidates_but_are_never_exact() {
    let mut ex = Extractor::default();
    ex.set_ecma_configs([(
        "tsconfig.json".into(),
        Some(r#"{"compilerOptions":{"paths":{"@/*":["./src/*"]}}}"#.into()),
    )]);
    let p = ex
        .extract(
            "src/main.ts",
            r#"import './x';
import(/* webpackChunkName: "PRIVATE_BODY" */ './x', { with: { type: 'json' } });
require(`./x`);
import X = require('./x');
import '@/views/x';
import('@/views/x');
import './a.js';
require('./a.js');
"#,
        )
        .unwrap();
    assert!(!p.syntax_error);
    assert_eq!(p.refs.len(), 8);
    for (i, baseline, kind) in [
        (1, 0, "import"),
        (2, 0, "require"),
        (3, 0, "require"),
        (5, 4, "import"),
        (7, 6, "require"),
    ] {
        let (r, static_ref) = (&p.refs[i], &p.refs[baseline]);
        assert_eq!((r.kind, r.line, r.exact), (kind, i + 1, false));
        assert_eq!(r.spelling, static_ref.spelling);
        assert_eq!(r.candidates, static_ref.candidates);
        assert_eq!(r.substitutes, static_ref.substitutes);
        assert_eq!(r.fallbacks, static_ref.fallbacks);
    }
    assert!(has_candidate(&p.refs[1], "src/x/index.ts"));
    assert!(has_candidate(&p.refs[5], "src/views/x.ts"));
    assert!(p.refs[6].exact);
}

#[test]
fn reserved_words_as_jsx_attribute_names_do_not_make_a_script_unreadable() {
    // Vue、Solid、Preact 的 JSX 直接写 `class`、`for`；React 的组件也有 `in`、`default` 这样的属性。
    let source = "import Icon from './icon'\nexport function Item({ on }) {\n  return <label class=\"a\" for=\"b\"><Icon in={on} default /></label>\n}\n";
    for path in ["src/Item.js", "src/Item.jsx", "src/Item.tsx"] {
        let p = parse(path, source);
        assert_eq!(p.refs.len(), 1, "{path}");
        assert!(has_candidate(&p.refs[0], "src/icon.js"), "{path}");
        assert_eq!(symbols(&p), [("Item", 2)], "{path}");
    }
    // 借 TSX 语法读通的 JavaScript 文件仍是 JavaScript 使用方，候选不套 TypeScript 的约定。
    let p = parse("src/Item.jsx", source);
    assert_eq!(p.refs[0].fallbacks, ["src/icon.vue", "src/icon/index.vue"]);
    let p = parse(
        "src/Item.vue",
        "<script>\nimport Icon from './Icon.vue'\nexport default {\n  render() { return <i class={['a']}><Icon /></i> }\n}\n</script>\n",
    );
    assert_eq!(p.refs[0].candidates, ["src/Icon.vue"]);
    assert_eq!((p.refs[0].line, p.refs[0].exact), (2, true));
    // 真正写坏的脚本换一套语法也读不通。
    let broken = Extractor::default()
        .extract(
            "src/Item.jsx",
            "import './icon'\nexport function Item( {\n  return <i class=\"a\" />\n",
        )
        .unwrap();
    assert!(broken.syntax_error && broken.refs.is_empty() && broken.symbols.is_empty());
}

#[test]
fn python_relative_base_is_the_module_not_the_imported_member_or_alias() {
    let p = parse(
        "pkg/entry.py",
        "# from .other import x\nfrom .writer import (\n    flush as renamed,\n)\n@decorator\ndef flush(): pass\nclass Writer: pass\ndef _private(): pass\n",
    );
    assert_eq!(p.refs.len(), 1);
    let r = &p.refs[0];
    assert_eq!((r.kind, r.line, r.exact), ("import", 2, true));
    assert_eq!(r.candidates, ["pkg/writer.py", "pkg/writer/__init__.py"]);
    assert_eq!(symbols(&p), [("flush", 6), ("Writer", 7)]);
    assert_eq!(
        parse("pkg/sub/__init__.py", "from ..io.writer import f").refs[0].candidates,
        ["pkg/io/writer.py", "pkg/io/writer/__init__.py"]
    );
}

#[test]
fn python_absolute_imports_and_package_attributes_keep_hints_not_certainty() {
    let p = parse(
        "pkg/entry.py",
        "import app.writer as w, thirdparty\nfrom app.writer import flush\nfrom . import child as alias\nif flag:\n    from .only import x\ntry:\n    import optional\nexcept ImportError: pass\nmod = __import__(PRIVATE_BODY)\n",
    );
    assert!(
        p.refs
            .iter()
            .all(|r| !r.exact && !r.spelling.contains("PRIVATE_BODY"))
    );
    assert!(has_candidate(&p.refs[0], "app/writer.py"));
    assert!(has_candidate(&p.refs[1], "thirdparty.py"));
    assert!(has_candidate(&p.refs[2], "app/writer/__init__.py"));
    assert!(has_candidate(&p.refs[3], "pkg/child.py"));
    assert!(has_candidate(&p.refs[3], "pkg/__init__.py"));
    assert_eq!(p.refs[3].line, 3);
    // 分支和 try 里的导入照常给出候选；相对导入在这里也不升为确定（见上面的整体断言）。
    assert_eq!((p.refs[4].kind, p.refs[4].line), ("import", 5));
    assert_eq!(
        p.refs[4].candidates,
        ["pkg/only.py", "pkg/only/__init__.py"]
    );
    assert_eq!(
        (p.refs[5].spelling.as_str(), p.refs[5].kind, p.refs[5].line),
        ("optional", "import", 7)
    );
    assert!(has_candidate(&p.refs[5], "optional.py"));
    // 运行时表达式仍然无法静态确定。
    assert_eq!(p.refs.len(), 7);
    assert_eq!((p.refs[6].kind, p.refs[6].line), ("unsupported", 9));
    assert!(p.refs[6].candidates.is_empty());
}

#[test]
fn python_imports_below_module_level_keep_candidates_but_are_never_certain() {
    let p = parse(
        "pkg/mod.py",
        "from .base import Base\nif TYPE_CHECKING:\n    from .models import Model\ntry:\n    from ._fast import run\nexcept ImportError:\n    from ._slow import run\nelse:\n    import pkg.extra\nfinally:\n    pass\ndef load():\n    from .lazy import value\n    import json, app.tools\n    return value\nclass Holder:\n    from . import sibling\nwith ctx:\n    from .shared import util\n",
    );
    let seen: Vec<_> = p
        .refs
        .iter()
        .map(|r| (r.spelling.as_str(), r.line, r.exact))
        .collect();
    // 只有模块顶层的相对导入是无条件的；分支、函数体、类体里的同一种写法只算“可能”。
    assert_eq!(
        seen,
        [
            (".base", 1, true),
            (".models", 3, false),
            ("._fast", 5, false),
            ("._slow", 7, false),
            ("pkg.extra", 9, false),
            (".lazy", 13, false),
            ("json", 14, false),
            ("app.tools", 14, false),
            ("sibling", 17, false),
            (".shared", 19, false),
        ]
    );
    assert!(p.refs.iter().all(|r| r.kind == "import"));
    assert_eq!(
        p.refs[1].candidates,
        ["pkg/models.py", "pkg/models/__init__.py"]
    );
    assert!(has_candidate(&p.refs[4], "pkg/extra.py"));
    assert!(has_candidate(&p.refs[8], "pkg/sibling.py"));
    // 越出项目根的相对导入和运行时加载仍然没有候选。
    let p = parse(
        "pkg/mod.py",
        "def f():\n    from ...far import x\n    return importlib.import_module(name)\n",
    );
    assert_eq!(p.refs.len(), 2);
    assert!(
        p.refs
            .iter()
            .all(|r| r.kind == "unsupported" && r.candidates.is_empty() && !r.exact)
    );
}

#[test]
fn typescript_output_extensions_offer_sources_only_as_substitutes() {
    let ts = parse(
        "pkg/src/api.mts",
        "import {a} from './config.js';\nexport * from '../lib/x.cjs';\nimport './view.jsx';\nimport './data.json';\nimport './types.ts';\nimport './plain';\n",
    );
    // 书写的文件仍是唯一的确定候选；源码扩展名只作替代候选，由索引按存在情况裁决。
    assert_eq!(ts.refs[0].candidates, ["pkg/src/config.js"]);
    assert_eq!(
        ts.refs[0].substitutes,
        ["pkg/src/config.ts", "pkg/src/config.tsx"]
    );
    assert_eq!(ts.refs[1].candidates, ["pkg/lib/x.cjs"]);
    assert_eq!(ts.refs[1].substitutes, ["pkg/lib/x.cts"]);
    assert_eq!(
        ts.refs[2].substitutes,
        ["pkg/src/view.ts", "pkg/src/view.tsx"]
    );
    assert!(ts.refs[..5].iter().all(|r| r.exact));
    // 非 JS 产物扩展名和省略扩展名的写法没有替代候选。
    assert!(ts.refs[3..].iter().all(|r| r.substitutes.is_empty()));
    assert!(!ts.refs[5].exact);
    // JavaScript 使用方写 `.js` 就是该文件，不做替换。
    let js = parse("pkg/src/api.mjs", "import {a} from './config.js';\n");
    assert_eq!(js.refs[0].candidates, ["pkg/src/config.js"]);
    assert!(js.refs[0].exact && js.refs[0].substitutes.is_empty());
}

#[test]
fn python_absolute_imports_offer_src_layout_and_top_package_roots_without_certainty() {
    let p = parse(
        "src/app/api/routes.py",
        "from app.core import run\nimport app\n",
    );
    // src 树里的使用方只有两个根：项目根和它所在的 src。候选数不随目录深度增长。
    assert_eq!(
        p.refs[0].candidates,
        [
            "app/core.py",
            "app/core/__init__.py",
            "src/app/core.py",
            "src/app/core/__init__.py"
        ]
    );
    assert_eq!(
        p.refs[1].candidates,
        [
            "app.py",
            "app/__init__.py",
            "src/app.py",
            "src/app/__init__.py"
        ]
    );
    // src 树之外的使用方：各级祖先下的 src/ 都是候选根。
    let p = parse("pkgs/foo/tests/test_x.py", "import foo.x\n");
    for name in [
        "foo/x.py",
        "src/foo/x.py",
        "pkgs/src/foo/x.py",
        "pkgs/foo/src/foo/x.py",
    ] {
        assert!(has_candidate(&p.refs[0], name), "{name}");
    }
    // 使用方在嵌套的 src 里：它所在的每一层 src 都保留。
    let p = parse("src/tools/src/pkg/x.py", "import pkg.y\n");
    for name in ["pkg/y.py", "src/pkg/y.py", "src/tools/src/pkg/y.py"] {
        assert!(has_candidate(&p.refs[0], name), "{name}");
    }
    assert_eq!(p.refs[0].candidates.len(), 6);
    // 已知取舍：src 树里另有一个不是使用方祖先的 src/ 时，不再猜它。
    let p = parse("src/svc/tests/test_x.py", "import svc_pkg.mod\n");
    assert!(!has_candidate(&p.refs[0], "src/svc/src/svc_pkg/mod.py"));
    assert_eq!(p.refs[0].candidates.len(), 4);
    // 多段导入的首段与某级祖先目录同名时，该目录的上一级也是候选根。
    let p = parse("tools/handlers/admin.py", "from handlers.utils import f\n");
    assert!(has_candidate(&p.refs[0], "tools/handlers/utils.py"));
    assert!(p.refs.iter().all(|r| !r.exact && r.kind == "import"));
    // 单段导入不凭同名目录连到本地：包装外部库的目录常与该库同名。
    let p = parse(
        "app/providers/openai/client.py",
        "import openai\nfrom openai import OpenAI\n",
    );
    assert_eq!(p.refs.len(), 2);
    for r in &p.refs {
        assert!(!has_candidate(r, "app/providers/openai/__init__.py"));
        assert!(!has_candidate(r, "app/providers/openai.py"));
        assert!(has_candidate(r, "openai/__init__.py") && !r.exact);
    }
    // 候选仍只由词法构造，不会越出项目根，也不会重复。
    let r = &parse("a/b/c.py", "import x.y\n").refs[0];
    assert!(
        r.candidates
            .iter()
            .all(|c| !c.contains("..") && !c.starts_with('/'))
    );
    let mut unique = r.candidates.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), r.candidates.len());
}

#[test]
fn python_package_root_depends_on_the_managed_package_directories() {
    let mut ex = Extractor::default();
    let path = "tools/handlers/api/run.py";
    let source = "from config import value\n";
    let unknown = ex.extract(path, source).unwrap();
    assert!(!has_candidate(
        &unknown.refs[0],
        "tools/handlers/api/config.py"
    ));
    ex.set_python_packages([]);
    let script = ex.extract(path, source).unwrap();
    assert!(has_candidate(
        &script.refs[0],
        "tools/handlers/api/config.py"
    ));
    ex.set_python_packages(["tools/handlers/api".into(), "tools/handlers".into()]);
    let package = ex.extract(path, source).unwrap();
    let r = &package.refs[0];
    assert!(has_candidate(r, "tools/config.py") && !r.exact);
    assert!(!has_candidate(r, "tools/handlers/api/config.py"));
    assert!(!has_candidate(r, "tools/handlers/config.py"));
    // 包一直延伸到项目根，也不能越出项目或重复已有候选。
    ex.set_python_packages([
        "tools/handlers/api".into(),
        "tools/handlers".into(),
        "tools".into(),
        "".into(),
    ]);
    assert_eq!(ex.extract(path, source).unwrap(), unknown);
}

#[test]
fn path_normalization_never_escapes_the_project_or_treats_literals_as_paths() {
    let mut extractor = Extractor::default();
    for path in [
        "../outside.ts",
        "src/../../escape.ts",
        "/root.ts",
        "D:\\root.ts",
        "\\\\host\\share\\x.ts",
    ] {
        assert!(extractor.extract(path, "").is_err(), "{path}");
    }
    let p = parse(
        "src\\inner\\entry.ts",
        "import '../x/../writer.ts';\nimport '../../../bad';\nimport './C:/PRIVATE_BODY';\nimport './%2e%2e/x';\nimport './a\\\\b';",
    );
    assert_eq!(p.refs[0].candidates, ["src/writer.ts"]);
    assert!(
        p.refs[1..]
            .iter()
            .all(|r| !r.exact && r.candidates.is_empty())
    );
    let r = parse("pkg/u.py", "from ...writer import f");
    assert!(r.refs.iter().all(|r| !r.exact && r.candidates.is_empty()));
}

#[test]
fn syntax_errors_discard_even_preceding_valid_facts_and_recovery_is_clean() {
    let mut extractor = Extractor::default();
    for (path, source) in [
        ("lib.rs", "pub mod writer;\nfn broken("),
        ("src/u.py", "from .writer import f\ndef broken(:"),
        ("src/u.ts", "import './writer';\nexport function f( {"),
    ] {
        let p = extractor.extract(path, source).unwrap();
        assert!(p.syntax_error && p.symbols.is_empty() && p.refs.is_empty());
    }
    let ok = extractor.extract("src/u.ts", "import './writer'").unwrap();
    assert!(!ok.syntax_error && has_candidate(&ok.refs[0], "src/writer.ts"));
}

#[test]
fn rust_stems_cannot_introduce_parent_traversal_or_turn_nested_lib_into_a_crate_root() {
    for path in ["...rs", "pkg/...rs", "pkg/..rs", "pkg/name..rs"] {
        let p = parse(path, "pub mod child;");
        assert!(
            !p.refs[0].exact && p.refs[0].candidates.is_empty(),
            "{path}"
        );
    }
    let p = parse("src/feature/lib.rs", "mod child;\nuse crate::child::run;");
    assert!(has_candidate(&p.refs[0], "src/feature/lib/child.rs"));
    assert!(!p.refs[1].exact);
}

#[test]
fn rust_name_collision_keeps_explicit_mod_hints_but_cannot_certify_a_use() {
    let p = parse(
        "lib.rs",
        "mod writer;\nuse other::file as writer;\nuse self::writer::save;\n",
    );
    assert!(has_candidate(&p.refs[0], "writer.rs"));
    assert!(p.refs.iter().all(|r| !r.exact));
    let p = parse(
        "lib.rs",
        "mod writer { mod child; }\nuse other::file as writer;\n",
    );
    assert!(p.refs.iter().all(|r| !r.exact));
    let p = parse(
        "lib.rs",
        "mod writer;\nfn writer() {}\nuse self::writer::flush;\n",
    );
    assert!(p.refs[0].exact); // 显式 mod 的文件候选不变。
    assert!(!p.refs[1].exact);
    assert_eq!(p.refs[1].rust_paths, ["self::writer::flush"]);
    // 声明归因采用更保守的跨命名空间边界，不因找到了前缀而承诺最终项。
    assert!(p.rust_modules.iter().all(|m| !m.usable));
}

#[test]
fn rust_raw_path_grouped_exports_and_inline_modules_preserve_their_scope() {
    let p = parse(
        "src/mod.rs",
        "#[path = r#\"io/actual.rs\"#] mod r#type;\npub use self::r#type::{flush as write, Open};\npub mod inner { mod leaf; use self::leaf::run; }\n#[cfg(test)] mod gated { mod hidden; }\n",
    );
    assert_eq!(symbols(&p), [("write", 2), ("Open", 2), ("inner", 3)]);
    assert_eq!(p.refs[0].candidates, ["src/io/actual.rs"]);
    assert!(p.refs[1].candidates.is_empty() && p.refs[1].rust_paths.is_empty());
    assert!(
        refs(&p, "module")
            .iter()
            .any(|r| r.line == 3 && has_candidate(r, "src/inner/leaf.rs"))
    );
    assert!(
        p.refs
            .iter()
            .any(|r| r.line == 3 && r.kind == "unsupported")
    );
    assert!(p.rust_modules.iter().all(|m| !m.usable));
    assert!(p.refs.iter().filter(|r| r.line == 4).all(|r| !r.exact));
}

#[test]
fn ecma_exports_name_public_bindings_and_directory_imports_only_offer_indexes() {
    let p = parse(
        "src/u.ts",
        "import './store/';\nexport const a=1, b=2;\nexport interface Shape {}\nexport { Shape as S };\nexport * as io from './io.mjs';\nexport default function local() {}\nimport 'react';\n",
    );
    assert_eq!(
        symbols(&p),
        [
            ("a", 2),
            ("b", 2),
            ("Shape", 3),
            ("S", 4),
            ("io", 5),
            ("default", 6)
        ]
    );
    assert!(
        p.refs[0]
            .candidates
            .iter()
            .all(|f| f.starts_with("src/store/index."))
    );
    assert_eq!(p.refs[1].candidates, ["src/io.mjs"]);
    assert_eq!(
        (p.refs[2].spelling.as_str(), p.refs[2].kind, p.refs[2].exact),
        ("react", "unsupported", false)
    );
}

#[test]
fn python_exports_honor_literal_all_and_do_not_infer_a_dynamic_export_list() {
    let p = parse(
        "pkg/lib.py",
        "__all__ = ('_visible', 'VALUE')\ndef _visible(): pass\nVALUE: int = 2\ndef hidden(): pass\n",
    );
    assert_eq!(symbols(&p), [("_visible", 2), ("VALUE", 3)]);
    let p = parse(
        "pkg/lib.py",
        "__all__ = ['run']\n__all__.append(config.name)\ndef run(): pass\n",
    );
    assert!(p.symbols.is_empty());
}

#[test]
fn unseen_and_previously_parsed_files_produce_identical_candidate_sets() {
    let mut extractor = Extractor::default();
    let source = "import './store';\nexport function useStore() {}\n";
    let before = extractor.extract("src/u.ts", source).unwrap();
    for path in ["src/store.ts", "src/store.js", "other/store.ts"] {
        extractor.extract(path, "").unwrap();
    }
    assert_eq!(before, extractor.extract("src/u.ts", source).unwrap());
    assert!(
        has_candidate(&before.refs[0], "src/store.ts")
            && has_candidate(&before.refs[0], "src/store.js")
    );
    assert!(!has_candidate(&before.refs[0], "other/store.ts"));
}

#[test]
fn python_empty_suites_and_unfinished_try_discard_preceding_facts() {
    let mut extractor = Extractor::default();
    for suffix in [
        "def bad():\n",
        "if ready:\n    # no statement\n",
        "try: pass\n",
    ] {
        let src = format!("from .writer import flush\nclass Writer: pass\n{suffix}");
        let p = extractor.extract("pkg/u.py", &src).unwrap();
        assert!(
            p.syntax_error && p.symbols.is_empty() && p.refs.is_empty(),
            "{suffix}"
        );
    }
    let src = "try:\n    pass\nexcept* ValueError:\n    pass\nfinally:\n    pass\n";
    assert!(!extractor.extract("pkg/u.py", src).unwrap().syntax_error);
}
