//! tsconfig / jsconfig 的 `paths` 别名：哪份配置管着一个文件、模式怎样对上写法、
//! 替换目标落在哪里，以及经别名找到的导入怎样记成引用。
use super::has_candidate;
use crate::{Extractor, Parsed, Reference, ecma_config, tsconfig::Configs};

fn configs(list: &[(&str, &str)]) -> Configs {
    Configs::new(
        list.iter()
            .map(|(path, text)| ((*path).to_owned(), Some((*text).to_owned()))),
    )
}

/// 写在 `path` 里的 `spec` 经别名指向的路径；以 `/` 结尾的表示目标写成了目录。
fn mapped(c: &Configs, path: &str, spec: &str) -> Vec<String> {
    c.aliases(path)
        .targets(spec)
        .into_iter()
        .map(|(target, directory)| {
            if directory {
                format!("{target}/")
            } else {
                target
            }
        })
        .collect()
}

fn extract(list: &[(&str, &str)], path: &str, source: &str) -> Parsed {
    let mut extractor = Extractor::default();
    extractor.set_ecma_configs(
        list.iter()
            .map(|(path, text)| ((*path).to_owned(), Some((*text).to_owned()))),
    );
    let parsed = extractor.extract(path, source).unwrap();
    assert!(!parsed.syntax_error, "合成样例应可解析：{path}");
    parsed
}

fn named<'p>(parsed: &'p Parsed, spelling: &str) -> &'p Reference {
    parsed
        .refs
        .iter()
        .find(|r| r.spelling == spelling)
        .unwrap_or_else(|| panic!("没有引用 {spelling}"))
}

#[test]
fn configs_are_recognised_by_file_name() {
    for name in [
        "tsconfig.json",
        "jsconfig.json",
        "tsconfig.app.json",
        "tsconfig.base.json",
        "jsconfig.paths.json",
    ] {
        assert!(ecma_config(name), "{name}");
    }
    for name in [
        "package.json",
        "tsconfig",
        "tsconfig.json5",
        "mytsconfig.json",
        "tsconfig.json.bak",
    ] {
        assert!(!ecma_config(name), "{name}");
    }
}

#[test]
fn patterns_are_matched_the_way_typescript_matches_them() {
    let c = configs(&[(
        "tsconfig.json",
        r#"{
  // 注释和末尾多余的逗号都允许。
  "compilerOptions": {
    "paths": {
      "*": ["types/*"],
      "@/*b": ["second/*"],
      "@/*": ["src/*"],
      "@/components/*": ["ui/*", "./legacy/ui/*"],
      "@/x": ["exact/x.ts"],
      "~": ["./src/index.ts"],
      "lib/*": ["src/lib/*/"],
      "bad/*/*": ["never/*"],
      "dup/*": ["one/*", "./one/*"],
      "out/*": ["../outside/*", "/abs/*", "ok/*"],
    },
  },
}"#,
    )]);
    let at = |spec| mapped(&c, "src/app.ts", spec);
    assert_eq!(at("@/util"), ["src/util"]);
    // 前缀最长的模式胜出；一个模式的几个替换目标按书写的顺序都给出。
    assert_eq!(at("@/components/Button"), ["ui/Button", "legacy/ui/Button"]);
    // 没有 `*` 的模式整个相等就用它，比任何通配都优先。
    assert_eq!(at("@/x"), ["exact/x.ts"]);
    // 前缀一样长时取写在前面的。
    assert_eq!(at("@/yb"), ["second/y"]);
    assert_eq!(at("~"), ["src/index.ts"]);
    // 只写 `*` 的模式什么都对得上，包名也一样。
    assert_eq!(at("react"), ["types/react"]);
    assert_eq!(at("lib/a"), ["src/lib/a/"]);
    // 写了两个 `*` 的模式不合法，由其余的模式接手。
    assert_eq!(at("bad/x/y"), ["types/bad/x/y"]);
    assert_eq!(at("dup/a"), ["one/a"]);
    // 越出项目根的和绝对路径的目标不要，其余照常。
    assert_eq!(at("out/a"), ["ok/a"]);
    // 以 `/` 开头的写法不经别名；`*` 代表的一段为空时目标照写的用，带着 `*` 对不上文件。
    assert!(at("/abs/a").is_empty());
    assert!(at("@/").is_empty());
}

#[test]
fn the_base_directory_comes_from_base_url_or_from_the_config_that_writes_paths() {
    // 建索引时只读得到名为 tsconfig*.json、jsconfig*.json 的配置，样例也这样起名。
    let c = configs(&[
        (
            "configs/tsconfig.base.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["../src/*"] } } }"#,
        ),
        // 省略 `.json` 的继承。
        (
            "web/tsconfig.json",
            r#"{ "extends": "../configs/tsconfig.base" }"#,
        ),
        (
            "app/tsconfig.json",
            r#"{ "extends": "../configs/tsconfig.base.json", "compilerOptions": { "baseUrl": "./lib" } }"#,
        ),
        (
            "pkg/tsconfig.json",
            r##"{ "extends": "../configs/tsconfig.base.json", "compilerOptions": { "paths": { "#/*": ["./*"] } } }"##,
        ),
        (
            "multi/tsconfig.json",
            r#"{ "extends": ["../configs/tsconfig.base.json", "./tsconfig.more.json"] }"#,
        ),
        (
            "multi/tsconfig.more.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["./m/*"] } } }"#,
        ),
        (
            "deep/tsconfig.json",
            r#"{ "extends": "./tsconfig.mid.json", "compilerOptions": { "paths": { "@/*": ["*"] } } }"#,
        ),
        (
            "deep/tsconfig.mid.json",
            r#"{ "compilerOptions": { "baseUrl": "./src" } }"#,
        ),
        // 包里的预设和没给出文本的配置都读不到，只看自己写的。
        (
            "preset/tsconfig.json",
            r#"{ "extends": ["@vue/tsconfig/tsconfig.dom.json", "./tsconfig.gone.json"], "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#,
        ),
        (
            "loop/tsconfig.json",
            r#"{ "extends": "./tsconfig.a.json" }"#,
        ),
        (
            "loop/tsconfig.a.json",
            r#"{ "extends": "./tsconfig.json" }"#,
        ),
        (
            "empty/tsconfig.json",
            r#"{ "compilerOptions": { "baseUrl": "", "paths": { "@/*": ["src/*"] } } }"#,
        ),
        (
            "far/tsconfig.json",
            r#"{ "compilerOptions": { "baseUrl": "../..", "paths": { "@/*": ["src/*"] } } }"#,
        ),
        // 公共配置里的 `${configDir}` 指继承它的那份配置所在的目录。
        (
            "configs/tsconfig.shared.json",
            r#"{ "compilerOptions": { "paths": { "~/*": ["${configDir}/src/*"], "~": ["${configDir}"] } } }"#,
        ),
        (
            "apps/one/tsconfig.json",
            r#"{ "extends": "../../configs/tsconfig.shared.json" }"#,
        ),
        (
            "configs/tsconfig.rooted.json",
            r#"{ "compilerOptions": { "baseUrl": "${configDir}/lib", "paths": { "@/*": ["*"] } } }"#,
        ),
        (
            "apps/two/tsconfig.json",
            r#"{ "extends": "../../configs/tsconfig.rooted.json" }"#,
        ),
    ]);
    let at = |dir: &str, spec| mapped(&c, &format!("{dir}/main.ts"), spec);
    // 没写 `baseUrl`：以写 `paths` 的那份配置所在的目录为基准，继承来的也一样。
    assert_eq!(at("web", "@/x"), ["src/x"]);
    // 写了 `baseUrl`：以它为基准，它以写它的那份配置所在的目录解释。
    assert_eq!(at("app", "@/x"), ["app/src/x"]);
    assert_eq!(at("deep", "@/x"), ["deep/src/x"]);
    // 子配置的 `paths` 整体取代父配置的，不逐项合并。
    assert!(at("pkg", "@/x").is_empty());
    assert_eq!(at("pkg", "#/y"), ["pkg/y"]);
    // 继承几份时，后面的盖过前面的。
    assert_eq!(at("multi", "@/x"), ["multi/m/x"]);
    assert_eq!(at("preset", "@/x"), ["preset/src/x"]);
    // 继承成环也会停下。
    assert!(at("loop", "@/x").is_empty());
    // 空的 `baseUrl` 指配置所在的目录；落在项目之外时别名指不到项目里的文件。
    assert_eq!(at("empty", "@/x"), ["empty/src/x"]);
    assert!(at("far", "@/x").is_empty());
    assert_eq!(at("apps/one", "~/x"), ["apps/one/src/x"]);
    assert_eq!(at("apps/one", "~"), ["apps/one"]);
    assert_eq!(at("apps/two", "@/x"), ["apps/two/lib/x"]);
}

#[test]
fn the_nearest_config_decides_and_references_lend_their_aliases() {
    let mut list = vec![
        (
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#,
        ),
        (
            "jsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["./js/*"] } } }"#,
        ),
        (
            "packages/ui/tsconfig.json",
            r#"{ "compilerOptions": { "strict": true } }"#,
        ),
        (
            "packages/old/jsconfig.json",
            r#"{ "compilerOptions": { "paths": { "~/*": ["./*"] } } }"#,
        ),
        // 只起汇总作用的配置：别名写在被引用的配置里。
        (
            "apps/web/tsconfig.json",
            r#"{ "files": [], "references": [{ "path": "./tsconfig.app.json" }, { "path": "./tsconfig.node.json" }] }"#,
        ),
        (
            "apps/web/tsconfig.app.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#,
        ),
        (
            "apps/web/tsconfig.node.json",
            r#"{ "compilerOptions": { "types": ["node"] } }"#,
        ),
        // 引用可以写成目录；几份说法不一时都算上。
        (
            "apps/admin/tsconfig.json",
            r#"{ "references": [{ "path": "../web" }, { "path": "./tsconfig.lib.json" }] }"#,
        ),
        (
            "apps/admin/tsconfig.lib.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["./lib/*"] } } }"#,
        ),
        ("broken/tsconfig.json", "{ not json"),
    ];
    let c = configs(&list);
    // 同一个目录里两份都有时先认 tsconfig.json；更深的目录没有配置时用上层的。
    assert_eq!(mapped(&c, "src/a.ts", "@/x"), ["src/x"]);
    assert_eq!(mapped(&c, "src/deep/b.js", "@/x"), ["src/x"]);
    assert_eq!(mapped(&c, "top.ts", "@/x"), ["src/x"]);
    // 最近的配置没写别名，就没有别名，不再向上找。
    assert!(mapped(&c, "packages/ui/src/c.ts", "@/x").is_empty());
    assert_eq!(mapped(&c, "packages/old/d.js", "~/y"), ["packages/old/y"]);
    assert_eq!(mapped(&c, "apps/web/src/e.ts", "@/x"), ["apps/web/src/x"]);
    assert_eq!(
        mapped(&c, "apps/admin/f.ts", "@/x"),
        ["apps/web/src/x", "apps/admin/lib/x"]
    );
    // 读不成对象的配置照样管着它的目录。
    assert!(mapped(&c, "broken/g.ts", "@/x").is_empty());
    // 读不出文本的配置也一样。
    list.retain(|(path, _)| *path != "broken/tsconfig.json");
    let c = Configs::new(
        list.iter()
            .map(|(path, text)| ((*path).to_owned(), Some((*text).to_owned())))
            .chain([("broken/tsconfig.json".to_owned(), None)]),
    );
    assert!(mapped(&c, "broken/g.ts", "@/x").is_empty());
    assert_eq!(mapped(&c, "src/a.ts", "@/x"), ["src/x"]);
    // 没有任何配置时什么都不改写。
    assert!(mapped(&Configs::default(), "src/a.ts", "@/x").is_empty());
}

#[test]
fn aliased_imports_keep_every_candidate_but_are_never_certain() {
    let p = extract(
        &[(
            "tsconfig.json",
            r##"{ "compilerOptions": { "baseUrl": ".", "paths": { "@/*": ["src/*"], "#lib": ["lib/index.ts"], "@legacy/*": ["old/*", "src/*"] } } }"##,
        )],
        "src/app.ts",
        "import { a } from '@/util'\nimport b from '#lib'\nimport c from '@legacy/c'\nexport * from '@/models/'\nimport d from './rel'\nimport e from 'react'\nimport f from '@/x.js'\n",
    );
    let got: Vec<_> = p
        .refs
        .iter()
        .map(|r| (r.spelling.as_str(), r.kind, r.line, r.exact))
        .collect();
    assert_eq!(
        got,
        [
            ("@/util", "import", 1, false),
            ("#lib", "import", 2, false),
            ("@legacy/c", "import", 3, false),
            ("@/models/", "reexport", 4, false),
            ("./rel", "import", 5, false),
            ("react", "unsupported", 6, false),
            ("@/x.js", "import", 7, false),
        ]
    );
    // 省略扩展名的目标与相对路径一样补全扩展名和 index，组件与声明文件排在最后。
    let util = named(&p, "@/util");
    for path in ["src/util.ts", "src/util/index.ts", "src/util.js"] {
        assert!(has_candidate(util, path), "{path}");
    }
    assert_eq!(
        util.fallbacks,
        [
            "src/util.vue",
            "src/util/index.vue",
            "src/util.d.ts",
            "src/util/index.d.ts"
        ]
    );
    // 写明了文件也只是“可能”：别名要靠配置与打包器一致才成立。
    assert_eq!(named(&p, "#lib").candidates, ["lib/index.ts"]);
    let legacy = named(&p, "@legacy/c");
    assert!(has_candidate(legacy, "old/c.ts") && has_candidate(legacy, "src/c.ts"));
    // 写成目录的只找 index。
    let models = named(&p, "@/models/");
    assert!(has_candidate(models, "src/models/index.ts"));
    assert!(!has_candidate(models, "src/models.ts"));
    let x = named(&p, "@/x.js");
    assert_eq!(x.candidates, ["src/x.js"]);
    assert_eq!(x.substitutes, ["src/x.ts", "src/x.tsx"]);
    assert!(named(&p, "react").candidates.is_empty());
}

#[test]
fn vue_scripts_and_src_attributes_use_the_aliases_of_their_file() {
    let p = extract(
        &[(
            "web/jsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#,
        )],
        "web/src/views/Home.vue",
        "<template><Card /></template>\n<script src=\"@/views/home.js\"></script>\n<script setup>\nimport Card from '@/components/Card.vue'\nimport { api } from '@/api'\n</script>\n",
    );
    let got: Vec<_> = p
        .refs
        .iter()
        .map(|r| (r.spelling.as_str(), r.kind, r.line, r.exact))
        .collect();
    assert_eq!(
        got,
        [
            ("@/views/home.js", "import", 2, false),
            ("@/components/Card.vue", "import", 4, false),
            ("@/api", "import", 5, false),
        ]
    );
    let src = named(&p, "@/views/home.js");
    assert_eq!(src.candidates, ["web/src/views/home.js"]);
    assert!(src.substitutes.is_empty());
    assert_eq!(
        named(&p, "@/components/Card.vue").candidates,
        ["web/src/components/Card.vue"]
    );
    // JavaScript 的块不找声明文件。
    let api = named(&p, "@/api");
    assert!(has_candidate(api, "web/src/api.js") && has_candidate(api, "web/src/api/index.js"));
    assert_eq!(api.fallbacks, ["web/src/api.vue", "web/src/api/index.vue"]);
}
