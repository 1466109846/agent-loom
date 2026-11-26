//! Go、Java、Vue 和 C/C++：引用不一定写成路径，这里核对各语言给出的候选和它们的确定程度。
use super::{has_candidate, parse, symbols};
use crate::{Extractor, Parsed, Reference, go_module, language, member};

const APP: &[(&str, Option<&str>)] = &[("", Some("example.com/app"))];

fn go(modules: &[(&str, Option<&str>)], path: &str, source: &str) -> Parsed {
    let mut extractor = Extractor::default();
    extractor.set_go_modules(
        modules
            .iter()
            .map(|(dir, module)| ((*dir).to_owned(), module.map(str::to_owned))),
    );
    let parsed = extractor.extract(path, source).unwrap();
    assert!(!parsed.syntax_error, "合成样例应可解析：{path}");
    parsed
}

fn spelled<'p>(parsed: &'p Parsed, kind: &str) -> Vec<&'p str> {
    parsed
        .refs
        .iter()
        .filter(|r| r.kind == kind)
        .map(|r| r.spelling.as_str())
        .collect()
}

fn named<'p>(parsed: &'p Parsed, spelling: &str) -> &'p Reference {
    parsed
        .refs
        .iter()
        .find(|r| r.spelling == spelling)
        .unwrap_or_else(|| panic!("没有引用 {spelling}"))
}

fn located(parsed: &Parsed) -> Vec<(&str, usize)> {
    parsed
        .refs
        .iter()
        .map(|r| (r.spelling.as_str(), r.line))
        .collect()
}

fn certainty(parsed: &Parsed) -> Vec<(&str, bool)> {
    parsed
        .refs
        .iter()
        .map(|r| (r.spelling.as_str(), r.exact))
        .collect()
}

#[test]
fn every_supported_extension_names_its_language() {
    for (path, lang) in [
        ("a.go", "go"),
        ("A.java", "java"),
        ("A.vue", "vue"),
        ("a.c", "c"),
        ("a.h", "c"),
        ("a.cc", "cpp"),
        ("a.cpp", "cpp"),
        ("a.cxx", "cpp"),
        ("a.hh", "cpp"),
        ("a.hpp", "cpp"),
        ("a.hxx", "cpp"),
    ] {
        assert_eq!(language(path), Some(lang), "{path}");
    }
    for path in ["go.mod", "a.kt", "a.m", "a.inc", "Makefile"] {
        assert_eq!(language(path), None, "{path}");
    }
}

#[test]
fn one_extractor_serves_every_language_in_any_order() {
    let files = [
        ("a.go", "package a\n\nfunc A() {}\n"),
        ("A.java", "public class A {}\n"),
        ("a.vue", "<script>\nexport const a = 1\n</script>\n"),
        ("a.c", "#include \"a.h\"\n"),
        ("a.ts", "export const a = 1;\n"),
        ("a.py", "def a(): pass\n"),
        ("a.rs", "pub fn a() {}\n"),
    ];
    let mut shared = Extractor::default();
    for _ in 0..2 {
        for (path, source) in files.iter().rev() {
            let fresh = Extractor::default().extract(path, source).unwrap();
            assert!(!fresh.syntax_error, "{path}");
            assert_eq!(shared.extract(path, source).unwrap(), fresh, "{path}");
        }
    }
}

#[test]
fn go_module_path_is_read_from_the_module_directive_only() {
    for text in [
        "module example.com/app\n\ngo 1.22\n",
        "// 说明\nmodule example.com/app // 主模块\n",
        "module \"example.com/app\"\n",
        "module `example.com/app`\n",
        "module (\n\texample.com/app\n)\n",
        "\u{feff}module example.com/app\r\n",
    ] {
        assert_eq!(
            go_module(text).as_deref(),
            Some("example.com/app"),
            "{text:?}"
        );
    }
    for text in [
        "",
        "go 1.22\n",
        "module\n",
        "modules example.com/app\n",
        "module ./local\n",
        "module a b\n",
        "require module example.com/x v1\n",
        "module (\n)\n",
    ] {
        assert_eq!(go_module(text), None, "{text:?}");
    }
}

#[test]
fn go_imports_follow_the_module_path_and_members_name_the_declaring_package() {
    let p = go(
        APP,
        "cmd/server/main.go",
        "package main\n\nimport (\n\t\"fmt\"\n\t\"example.com/app/internal/model\"\n\tst \"example.com/app/internal/store\"\n\t_ \"example.com/app/internal/side\"\n\t. \"example.com/app/internal/dot\"\n\t\"example.com/application/other\"\n\t\"github.com/ext/lib\"\n)\n\nfunc main() {\n\tvar it model.Item\n\tfmt.Println(it, st.Open(), model.New().Field, model.unexported, lib.X)\n}\n",
    );
    // 模块路径要整段对上：`example.com/application` 不在 `example.com/app` 之下。
    assert_eq!(
        spelled(&p, "external"),
        ["fmt", "example.com/application/other", "github.com/ext/lib"]
    );
    // `_` 没有可跟踪的包名，只能指向整个包。
    assert_eq!(spelled(&p, "import"), ["example.com/app/internal/side"]);
    assert_eq!(
        named(&p, "example.com/app/internal/side").candidates,
        ["go:example.com/app/internal/side"]
    );
    // `.` 把那个包的导出名放进文件作用域：它们的写法与同包的名字一样，按名字到那个包里去找。
    assert_eq!(p.search, ["go:example.com/app/internal/dot"]);
    assert_eq!(spelled(&p, "use"), ["model.Item", "model.New", "st.Open"]);
    // 没写别名时，被导入包自己声明的包名也要对上；别名直接绑定到目录。
    assert_eq!(
        named(&p, "model.Item").candidates,
        [member("Item", "go:example.com/app/internal/model!model")]
    );
    assert_eq!(
        named(&p, "st.Open").candidates,
        [member("Open", "go:example.com/app/internal/store")]
    );
    assert_eq!(
        (named(&p, "model.Item").line, named(&p, "st.Open").line),
        (14, 15)
    );
    assert!(
        p.refs
            .iter()
            .filter(|r| matches!(r.kind, "use" | "import"))
            .all(|r| r.exact)
    );
    assert!(spelled(&p, "mention").is_empty());
    assert_eq!(p.domain.as_deref(), Some(""));
    assert_eq!(
        p.provides,
        ["gop:cmd/server!main", "go:example.com/app/cmd/server!main"]
    );
}

#[test]
fn go_files_under_build_constraints_keep_their_facts_but_belong_to_no_module() {
    let source = "package store\n\nimport \"example.com/app/model\"\n\nfunc Open() model.Item { return model.Item{} }\n";
    for path in [
        "store/open.go",
        "store/linux.go",
        "store/open_posix.go",
        "store/open_test.go",
        "store/linux_test.go",
    ] {
        assert_eq!(go(APP, path, source).domain.as_deref(), Some(""), "{path}");
    }
    // 文件名带系统或架构后缀的只在某些构建里出现，以下划线开头的根本不参与构建。
    // 事实照常给出；不给所属的根，它声明的和它用到的名字就都证明不了、只算可能。
    for path in [
        "store/open_linux.go",
        "store/open_windows_amd64.go",
        "store/open_arm64_test.go",
        "store/a_test_linux.go",
        "store/_draft.go",
    ] {
        let p = go(APP, path, source);
        assert!(p.domain.is_none(), "{path}");
        assert_eq!(spelled(&p, "use"), ["model.Item"], "{path}");
        assert_eq!(p.provides[0], "gop:store!store", "{path}");
    }
    // package 子句之前的构建注释同样如此；普通注释不是约束。
    for (head, free) in [
        ("//go:build linux && amd64\n\n", false),
        ("// +build ignore\n\n", false),
        ("// 版权说明\n\n//go:build tools\n\n", false),
        ("// 普通注释\n/* go:build 只是文字 */\n", true),
    ] {
        let p = go(APP, "store/open.go", &format!("{head}{source}"));
        assert_eq!(p.domain.is_some(), free, "{head}");
    }
    // 用了 cgo 的文件只在启用 cgo 时参与构建。
    let p = go(
        APP,
        "store/bridge.go",
        "package store\n\nimport \"C\"\n\nfunc Puts() { C.puts(nil) }\n",
    );
    assert!(p.domain.is_none());
    assert_eq!(spelled(&p, "external"), ["C"]);
    assert_eq!(p.refs.len(), 1);
}

#[test]
fn go_member_access_through_a_redeclared_name_is_only_trusted_in_type_position() {
    let p = go(
        APP,
        "a/a.go",
        "package a\n\nimport \"example.com/app/model\"\n\nfunc f(x model.Item) {\n\tmodel := load()\n\t_ = model.Count\n}\n",
    );
    assert_eq!(spelled(&p, "use"), ["model.Item"]);
    assert!(spelled(&p, "import").is_empty());
    // 只在表达式里用到时，这条导入没有可信的成员，退回到整个包。
    let p = go(
        APP,
        "a/a.go",
        "package a\n\nimport \"example.com/app/model\"\n\nfunc f() {\n\tmodel := load()\n\t_ = model.Count\n}\n",
    );
    assert!(spelled(&p, "use").is_empty());
    assert_eq!(spelled(&p, "import"), ["example.com/app/model"]);
}

#[test]
fn go_imports_cannot_be_placed_without_the_enclosing_module() {
    let source = "package a\n\nimport (\n\t\"fmt\"\n\t\"example.com/app/model\"\n)\n\nvar _ = model.Zero\nvar _ = fmt.Sprint\n";
    // 项目里没有 go.mod：连标准库和本地包都分不出来。
    let p = go(&[], "a/a.go", source);
    assert_eq!(spelled(&p, "unsupported"), ["fmt", "example.com/app/model"]);
    assert_eq!(p.refs.len(), 2);
    assert!(p.domain.is_none());
    assert_eq!(p.provides, ["gop:a!a"]);
    // 最近的 go.mod 读不出模块路径：它仍是边界，不退到外层模块。
    let p = go(
        &[("", Some("example.com/app")), ("a", None)],
        "a/a.go",
        source,
    );
    assert_eq!(spelled(&p, "unsupported"), ["fmt", "example.com/app/model"]);
    // 别处有一个读不出模块路径的 go.mod：对不上的导入也可能属于它，不能断定是外部的。
    let p = go(
        &[("", Some("example.com/app")), ("vendored", None)],
        "cmd/main.go",
        "package main\n\nimport (\n\t\"fmt\"\n\t\"example.com/app/model\"\n)\n\nvar _ = fmt.Sprint(model.Zero)\n",
    );
    assert_eq!(spelled(&p, "unsupported"), ["fmt"]);
    assert!(spelled(&p, "external").is_empty());
    assert_eq!(spelled(&p, "use"), ["model.Zero"]);
    // 嵌套模块里的文件按自己的模块给出键；跨模块的导入照样按导入路径去找。
    let p = go(
        &[
            ("", Some("example.com/app")),
            ("tools", Some("example.com/tools")),
        ],
        "tools/gen/gen.go",
        source,
    );
    assert_eq!(p.domain.as_deref(), Some("tools"));
    assert_eq!(
        p.provides,
        ["gop:tools/gen!a", "go:example.com/tools/gen!a"]
    );
    assert_eq!(spelled(&p, "external"), ["fmt"]);
    assert_eq!(
        named(&p, "model.Zero").candidates,
        [member("Zero", "go:example.com/app/model!model")]
    );
}

#[test]
fn go_unaliased_imports_are_matched_by_the_conventional_package_name() {
    let p = go(
        APP,
        "a/a.go",
        "package a\n\nimport (\n\t\"example.com/app/api/v2\"\n\t\"example.com/app/go-kit\"\n\t\"example.com/app/yaml.v3\"\n\t\"example.com/app/odd\"\n)\n\nvar _ = []any{api.A, kit.B, yaml.C, weird.D}\n",
    );
    for (spelling, key) in [
        ("api.A", "go:example.com/app/api/v2!api"),
        ("kit.B", "go:example.com/app/go-kit!kit"),
        ("yaml.C", "go:example.com/app/yaml.v3!yaml"),
    ] {
        let item = spelling.rsplit('.').next().unwrap();
        assert_eq!(named(&p, spelling).candidates, [member(item, key)]);
    }
    // 包名与路径对不上、又没写别名：成员归不到这条导入上，它退回到整个包。
    // 真实包名写在声明键里，所以推测错了只会对不上，不会连到别的包。
    assert_eq!(spelled(&p, "import"), ["example.com/app/odd"]);
    assert_eq!(spelled(&p, "mention"), ["weird"]);
}

#[test]
fn go_names_not_declared_in_the_file_are_looked_up_in_the_package() {
    let p = go(
        APP,
        "store/use.go",
        "package store\n\nimport \"example.com/app/raw\"\n\ntype pair struct {\n\tleft raw.Item\n\tEmbedded\n}\n\nfunc use[T any](ch chan int, items []int) (named error) {\n\ts := Server{addr: addr, limit: 3}\n\tfor i, v := range items {\n\t\t_ = i + v + offset\n\t}\n\tswitch t := named.(type) {\n\tcase nil:\n\t\t_ = t\n\t}\n\tselect {\n\tcase got := <-ch:\n\t\t_ = got\n\t}\n\tfn := func(inner int) int { return inner }\n\t_, _ = string(buf), fn\n\tvar local T = T(defaultClient.Timeout)\nouter:\n\tfor {\n\t\tbreak outer\n\t}\n\t_, _ = s, local\n\treturn helper(len(items))\n}\n\nfunc (p *pair) method() {}\n",
    );
    // 参数、返回值、类型参数、`:=`、range、类型分支、select、标签、字段名和字面量的键
    // 都不是对包级名字的引用；预声明的类型、nil，以及 len 这类没有人重新声明的内建函数也不是。
    assert_eq!(
        spelled(&p, "mention"),
        [
            "Embedded",
            "Server",
            "addr",
            "offset",
            "buf",
            "defaultClient",
            "helper"
        ]
    );
    // 同包的文件一起编译：对上唯一的声明就是确定的，所以这些候选在语法上算确定。
    for r in p.refs.iter().filter(|r| r.kind == "mention") {
        assert_eq!(r.candidates, [member(&r.spelling, "gop:store!store")]);
        assert!(r.exact && r.fallbacks.is_empty() && r.substitutes.is_empty());
    }
    assert_eq!(
        (named(&p, "Embedded").line, named(&p, "helper").line),
        (7, 31)
    );
    assert_eq!(spelled(&p, "use"), ["raw.Item"]);
    assert!(p.search.is_empty());
    // 包里自己声明 min、max、new、copy 的并不少见：这几个内建函数照常按名字去找。
    let p = go(
        APP,
        "store/m.go",
        "package store\n\nfunc f(a, b int) int { return min(a, max(b, cap(make([]int, 0)))) }\n",
    );
    assert_eq!(spelled(&p, "mention"), ["min", "max"]);
}

#[test]
fn go_local_names_only_shadow_uses_inside_function_bodies() {
    let p = go(
        APP,
        "store/scope.go",
        "package store\n\nimport \"example.com/app/keys\"\n\nvar AKey = keys.New(\"A\")\n\ntype set[T any] struct{ items []T }\n\nfunc (s *set[K]) add(v K, w Other) {}\n\nfunc (h) Serve(conn Conn) {}\n\nfunc run(name string) {\n\tkeys := load()\n\th := 1\n\t_ = keys.Count\n\t_, _ = h, name\n\tvar local = func(inner Shape) { _ = inner }\n\t_ = local\n}\n",
    );
    // 函数体里声明的局部变量遮不住包级位置上的使用：`keys.New` 写在包级变量的初始值里，
    // `h` 写在接收者类型上。函数体里的 `keys.Count` 可能是字段访问，不算。
    assert_eq!(spelled(&p, "use"), ["keys.New"]);
    assert_eq!(named(&p, "keys.New").line, 5);
    assert!(spelled(&p, "import").is_empty());
    // 接收者类型里的 K 是在声明类型参数，参数名和类型参数都不是对包级名字的使用。
    assert_eq!(
        spelled(&p, "mention"),
        ["Other", "h", "Conn", "load", "Shape"]
    );
    assert_eq!((named(&p, "h").line, named(&p, "load").line), (11, 14));
    // 类型参数在签名里遮住同名的包级名字。
    let p = go(
        APP,
        "store/generic.go",
        "package store\n\nfunc Map[T, Item any](in []T, f func(T) Item) []Item { return nil }\n\nvar _ Item\n",
    );
    assert!(spelled(&p, "mention").is_empty());
}

#[test]
fn go_package_level_names_are_symbols_and_unexported_ones_are_not_shown() {
    let p = go(
        APP,
        "store/decl.go",
        "package store\n\nconst (\n\tLimit = iota\n\tfloor\n)\n\nvar (\n\tDefault, other = 1, 2\n)\n\ntype (\n\tDB struct{}\n\tAlias = DB\n\thidden int\n)\n\nfunc Open() {}\nfunc helper() {}\nfunc init() {}\nfunc (d *DB) Close() {}\nvar _ = 1\n",
    );
    let shown: Vec<_> = p
        .symbols
        .iter()
        .filter(|s| s.exported)
        .map(|s| (s.name.as_str(), s.kind, s.line))
        .collect();
    assert_eq!(
        shown,
        [
            ("Limit", "constant", 4),
            ("Default", "variable", 9),
            ("DB", "type", 13),
            ("Alias", "type", 14),
            ("Open", "function", 18)
        ]
    );
    // 方法不在包作用域里；init 不能被引用。
    let hidden: Vec<_> = p
        .symbols
        .iter()
        .filter(|s| !s.exported)
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(hidden, ["floor", "other", "hidden", "helper"]);
}

#[test]
fn go_test_files_are_not_part_of_the_importable_package() {
    let mut extractor = Extractor::default();
    extractor.set_go_modules([(String::new(), Some("example.com/app".to_owned()))]);
    assert_eq!(
        extractor.path_keys("store/open.go"),
        ["go:example.com/app/store"]
    );
    assert_eq!(extractor.path_keys("main.go"), ["go:example.com/app"]);
    assert!(extractor.path_keys("store/open_test.go").is_empty());
    assert!(Extractor::default().path_keys("store/open.go").is_empty());

    let inner = go(
        APP,
        "store/export_test.go",
        "package store\n\nvar Exported = newDB\n",
    );
    assert_eq!(
        inner.provides,
        ["gop:store!store", "gotest:example.com/app/store!store"]
    );
    let outer = go(
        APP,
        "store/open_test.go",
        "package store_test\n\nimport (\n\t\"testing\"\n\n\t\"example.com/app/model\"\n\t\"example.com/app/store\"\n)\n\nfunc TestOpen(t *testing.T) { _, _ = store.Exported, model.Zero }\n",
    );
    assert_eq!(outer.provides, ["gop:store!store_test"]);
    // 外部测试包与被测包的包内测试文件一起编译，用得到后者导出的名字；别的包用不到。
    assert_eq!(
        named(&outer, "store.Exported").candidates,
        [
            member("Exported", "go:example.com/app/store!store"),
            member("Exported", "gotest:example.com/app/store!store")
        ]
    );
    assert_eq!(
        named(&outer, "model.Zero").candidates,
        [member("Zero", "go:example.com/app/model!model")]
    );
    assert!(outer.search.is_empty());
    // 外部测试包常把被测包点导入：名字到被测包和它的包内测试文件里去找。
    let dotted = go(
        APP,
        "store/dot_test.go",
        "package store_test\n\nimport . \"example.com/app/store\"\n\nvar _ = Open\n",
    );
    assert_eq!(
        dotted.search,
        [
            "go:example.com/app/store",
            "gotest:example.com/app/store!store"
        ]
    );
    assert_eq!(spelled(&dotted, "mention"), ["Open"]);
    assert!(spelled(&dotted, "import").is_empty());
}

#[test]
fn go_source_needs_a_package_clause_but_not_a_final_newline() {
    let p = go(
        APP,
        "m/item.go",
        "package m\n\ntype Item struct{ Name string }",
    );
    assert_eq!(symbols(&p), [("Item", 3)]);
    let broken = Extractor::default()
        .extract("m/x.go", "func f() {}\n")
        .unwrap();
    assert!(broken.syntax_error && broken.symbols.is_empty() && broken.provides.is_empty());
}

#[test]
fn java_imports_name_a_type_in_a_package_and_only_platform_packages_are_external() {
    let p = parse(
        "src/main/java/com/acme/web/Handler.java",
        "package com.acme.web;\n\nimport com.acme.model.Item;\nimport com.acme.model.Outer.Inner;\nimport static com.acme.util.Texts.trim;\nimport static com.acme.util.Texts.*;\nimport com.acme.spi.*;\nimport java.util.List;\nimport java.util.*;\nimport org.ext.Thing;\n\npublic class Handler {}\n",
    );
    assert_eq!(
        spelled(&p, "import"),
        [
            "com.acme.model.Item",
            "com.acme.model.Outer.Inner",
            "com.acme.util.Texts.trim",
            "com.acme.util.Texts.*",
            "org.ext.Thing"
        ]
    );
    // 包在哪一段结束要看别处的声明，这里按命名约定来拆：第一个大写开头的段是顶层类型。
    assert_eq!(
        named(&p, "com.acme.model.Item").candidates,
        [member("Item", "java:com.acme.model")]
    );
    // 后面还有一段时它是嵌套类型；不合约定的名字再按“最后一段是类型”试一次。
    assert_eq!(
        named(&p, "com.acme.model.Outer.Inner").candidates,
        [
            member("Outer", "java:com.acme.model"),
            member("Inner", "java:com.acme.model.Outer")
        ]
    );
    // 静态导入的最后一段是成员，类型是它前面的部分。
    let texts = [member("Texts", "java:com.acme.util")];
    assert_eq!(named(&p, "com.acme.util.Texts.trim").candidates, texts);
    assert_eq!(named(&p, "com.acme.util.Texts.*").candidates, texts);
    assert!(
        p.refs
            .iter()
            .filter(|r| r.kind == "import")
            .all(|r| r.exact)
    );
    // `java.` 开头的包只能由平台提供；其余第三方包无从证明，留给索引去对。
    assert_eq!(spelled(&p, "external"), ["java.util.List", "java.util.*"]);
    // 按需导入本身不指向哪个文件：包记作搜索键，另留一处“它也可能是某个类型”的提及。
    assert_eq!(p.search, ["java:com.acme.spi"]);
    assert_eq!(spelled(&p, "mention"), ["com.acme.spi.*"]);
    let demand = named(&p, "com.acme.spi.*");
    assert_eq!(demand.candidates, [member("spi", "java:com.acme")]);
    assert!(!demand.exact);
    assert_eq!(p.provides, ["java:com.acme.web"]);
    // 类型名不合约定（小写开头）时，靠“最后一段是类型”这一种拆法。
    let p = parse(
        "x/A.java",
        "package x;\nimport com.acme.model.item;\nimport com.acme.Outer.Deep.Leaf;\nclass A {}\n",
    );
    assert_eq!(
        named(&p, "com.acme.model.item").candidates,
        [member("item", "java:com.acme.model")]
    );
    assert_eq!(
        named(&p, "com.acme.Outer.Deep.Leaf").candidates,
        [
            member("Outer", "java:com.acme"),
            member("Leaf", "java:com.acme.Outer.Deep")
        ]
    );

    // 无名包里的类型不能被导入。
    let p = parse(
        "A.java",
        "import Foo;\nimport static Bar.baz;\nclass A {}\n",
    );
    assert_eq!(spelled(&p, "unsupported"), ["Foo", "Bar.baz"]);
}

#[test]
fn java_source_root_is_the_directory_minus_the_package() {
    let source = "package com.acme.web;\nclass A {}\n";
    for (path, root) in [
        ("src/main/java/com/acme/web/A.java", Some("src/main/java")),
        ("com/acme/web/A.java", Some("")),
        ("src/acme/web/A.java", None),
        ("xcom/acme/web/A.java", None),
        ("A.java", None),
    ] {
        let p = parse(path, source);
        assert_eq!(p.domain.as_deref(), root, "{path}");
        assert_eq!(p.provides, ["java:com.acme.web"], "{path}");
    }
    // 无名包没有名字可对：只有同一个目录里的文件算同包。
    let p = parse("scripts/Tool.java", "class Tool { Other o; }\n");
    assert_eq!(p.domain.as_deref(), Some("scripts"));
    assert_eq!(p.provides, ["javadir:scripts"]);
    assert_eq!(
        named(&p, "Other").candidates,
        [member("Other", "javadir:scripts")]
    );
    assert_eq!(parse("Tool.java", "class Tool {}\n").provides, ["javadir:"]);
}

#[test]
fn java_module_declarations_name_the_service_types_they_use_and_provide() {
    let p = parse(
        "src/module-info.java",
        "module com.acme.app {\n    requires java.logging;\n    exports com.acme.api;\n    uses com.acme.spi.Plugin;\n    uses java.sql.Driver;\n    provides com.acme.spi.Plugin with com.acme.impl.Default, com.acme.impl.Extra;\n}\n",
    );
    let got: Vec<_> = p
        .refs
        .iter()
        .map(|r| (r.spelling.as_str(), r.kind, r.line, r.exact))
        .collect();
    // requires 写的是模块名，exports 写的是包名，都不指向哪个文件。
    assert_eq!(
        got,
        [
            ("com.acme.spi.Plugin", "import", 4, true),
            ("java.sql.Driver", "external", 5, false),
            ("com.acme.spi.Plugin", "import", 6, true),
            ("com.acme.impl.Default", "import", 6, true),
            ("com.acme.impl.Extra", "import", 6, true),
        ]
    );
    assert_eq!(
        named(&p, "com.acme.impl.Default").candidates,
        [member("Default", "java:com.acme.impl")]
    );
    // 模块声明放在源根上，所以它和模块里的类同属一个根。
    assert_eq!(p.domain.as_deref(), Some("src"));
    assert!(p.symbols.is_empty() && p.search.is_empty());
}

#[test]
fn java_top_level_types_are_symbols_and_package_private_ones_are_not_shown() {
    let p = parse(
        "p/A.java",
        "package p;\n\n@Deprecated\npublic final class A { public static class Nested {} }\nclass B {}\ninterface C {}\npublic enum D { X }\nrecord E(int x) {}\n@interface F {}\n",
    );
    let all: Vec<_> = p
        .symbols
        .iter()
        .map(|s| (s.name.as_str(), s.kind, s.line, s.exported))
        .collect();
    assert_eq!(
        all,
        [
            ("A", "class", 4, true),
            ("B", "class", 5, false),
            ("C", "interface", 6, false),
            ("D", "enum", 7, true),
            ("E", "record", 8, false),
            ("F", "annotation", 9, false)
        ]
    );
}

#[test]
fn java_names_are_looked_up_in_the_same_package_before_on_demand_imports() {
    let p = parse(
        "src/com/acme/web/Handler.java",
        "package com.acme.web;\n\nimport com.acme.model.Item;\nimport com.acme.spi.*;\nimport static com.acme.util.Texts.trim;\n\n@Service\npublic class Handler<T extends Base> extends Parent implements Shape.Round {\n    private Helper helper;\n    private Map.Entry<Item, T> entry;\n    void run(Store store) {\n        var local = Limits.MAX;\n        Worker.start(trim(\"x\"));\n        helper.go(Handler.class, local);\n        store.save(com.acme.other.Qualified.build(), java.util.List.of());\n        Runnable r = Tasks::run;\n        java.io.File f = null;\n        Nested n = null;\n        log.info(\"x\");\n        String s = this.config.name.value;\n        com.acme.wide.Typed t = (com.acme.wide.Typed) null;\n    }\n    static class Nested {}\n}\n",
    );
    // 不去找的名字：文件里声明的类型和类型参数、单个导入绑定的名字、声明过的变量、
    // 表达式里小写开头的名字（变量或继承来的字段）、`类型.成员` 里的成员、`var`、
    // 以 `java.` 开头的限定名，以及 String、Runnable 这类没有人重新声明的平台类型。
    assert_eq!(
        spelled(&p, "mention"),
        [
            "com.acme.spi.*",
            "Service",
            "Base",
            "Parent",
            "Shape",
            "Helper",
            "Map",
            "Store",
            "Limits",
            "Worker",
            "com.acme.other.Qualified",
            "Tasks",
            "com.acme.wide.Typed"
        ]
    );
    // 同包的类型遮住按需导入的同名类型：候选里只写同包，按需导入的包记作搜索键，
    // 由索引在同包落空时再去找。
    let helper = named(&p, "Helper");
    assert_eq!(helper.candidates, [member("Helper", "java:com.acme.web")]);
    assert!(helper.fallbacks.is_empty() && helper.substitutes.is_empty());
    assert_eq!((helper.line, helper.exact), (9, false));
    assert_eq!(p.search, ["java:com.acme.spi"]);
    // 表达式里直接写的限定名：至少两段小写之后接一段大写才认，取到类型为止。
    let qualified = named(&p, "com.acme.other.Qualified");
    assert_eq!(
        qualified.candidates,
        [member("Qualified", "java:com.acme.other")]
    );
    assert_eq!((qualified.line, qualified.exact), (15, false));
    // 类型位置上的限定名只出现一次（声明和强制转换各写了一遍）。
    assert_eq!(
        named(&p, "com.acme.wide.Typed").candidates,
        [member("Typed", "java:com.acme.wide")]
    );
    assert!(p.refs.iter().all(|r| r.kind != "mention" || !r.exact));
}

#[test]
fn vue_scripts_are_read_with_their_own_language_and_keep_file_line_numbers() {
    let p = parse(
        "web/src/App.vue",
        "<template>\n  <div><Child /><template v-if=\"ok\"><span>a < b</span></template></div>\n</template>\n\n<script>\nimport { helper } from '../helper'\nexport default { name: 'App' }\n</script>\n\n<script setup lang=\"ts\" generic=\"T extends Record<string, unknown>\">\nimport Child from './components/Child.vue'\nimport { useStore } from './store.js'\nimport axios from 'axios'\n</script>\n\n<style scoped>\n.a > .b { color: red; }\n</style>\n",
    );
    let got: Vec<_> = p
        .refs
        .iter()
        .map(|r| (r.spelling.as_str(), r.kind, r.line, r.exact))
        .collect();
    assert_eq!(
        got,
        [
            ("../helper", "import", 6, false),
            ("./components/Child.vue", "import", 11, true),
            ("./store.js", "import", 12, true),
            ("axios", "unsupported", 13, false)
        ]
    );
    assert!(has_candidate(&p.refs[0], "web/helper.js"));
    // 省略了扩展名：脚本扩展名都落空才看组件。
    assert_eq!(
        p.refs[0].fallbacks,
        ["web/helper.vue", "web/helper/index.vue"]
    );
    assert_eq!(p.refs[1].candidates, ["web/src/components/Child.vue"]);
    // 只有声明为 TypeScript 的块才把 `./store.js` 也当作 `store.ts`。
    assert_eq!(
        p.refs[2].substitutes,
        ["web/src/store.ts", "web/src/store.tsx"]
    );
    assert!(p.refs[0].substitutes.is_empty());
    assert_eq!(symbols(&p), [("default", 7)]);
}

#[test]
fn vue_only_root_level_script_blocks_count() {
    // 只有模板和样式的组件没有事实，也不是错误。
    let p = parse(
        "a/Plain.vue",
        "<template><p>hi</p></template>\n<style>p{}</style>\n",
    );
    assert!(p.refs.is_empty() && p.symbols.is_empty());
    // 注释里、模板里和自定义块里的 script 都不是根级脚本块。
    let p = parse(
        "a/Doc.vue",
        "<!-- <script>import './ghost'</script> -->\n<template>\n  <pre>&lt;script&gt;</pre>\n  <component is=\"script\" />\n</template>\n<docs>\n<script>import './ghost'</script>\n</docs>\n",
    );
    assert!(p.refs.is_empty());
    // 写了 src 的块由外部文件提供内容，按脚本里的导入同样的规则去找；别的语言的块读不了。
    let p = parse(
        "a/b/Ext.vue",
        "<template><p/></template>\n<script src=\"./ext.ts\" lang=\"ts\"></script>\n<script src=\"pkg/ext.js\" />\n<script lang=\"coffee\">\nx = require './y'\n</script>\n<script src></script>\n",
    );
    let got: Vec<_> = p
        .refs
        .iter()
        .map(|r| (r.spelling.as_str(), r.kind, r.line, r.exact))
        .collect();
    assert_eq!(
        got,
        [
            ("./ext.ts", "import", 2, true),
            ("pkg/ext.js", "unsupported", 3, false),
            ("script", "unsupported", 4, false),
            ("script", "unsupported", 7, false)
        ]
    );
    assert_eq!(p.refs[0].candidates, ["a/b/ext.ts"]);
}

#[test]
fn vue_files_with_an_unfinished_block_or_broken_script_are_not_covered() {
    let mut extractor = Extractor::default();
    for source in [
        "<script>\nimport './a'\n",
        "<template><p>x</p>\n<script>import './a'</script>\n",
        "<script lang=\"ts\">\nimport './a'\nexport function f( {\n</script>\n",
        "<script setup\nimport './a'\n",
        "<!-- 没有结束的注释\n<script>import './a'</script>\n",
    ] {
        let p = extractor.extract("a/X.vue", source).unwrap();
        assert!(
            p.syntax_error && p.refs.is_empty() && p.symbols.is_empty(),
            "{source}"
        );
    }
    let ok = extractor
        .extract("a/X.vue", "<script>import './a.js'</script>")
        .unwrap();
    assert!(!ok.syntax_error && ok.refs[0].candidates == ["a/a.js"]);
}

#[test]
fn c_quoted_includes_look_beside_the_file_first_and_then_by_path_suffix() {
    let p = parse(
        "native/src/main.c",
        "#include <stdio.h>\n#include \"util.h\"\n#include \"core/log.h\"\n#include <core/log.h>\n#include \"../shared/x.h\"\n#include \"../../../out.h\"\n#  include   \"spaced.h\"   // 注释\n#include \"win\\api.h\"\n",
    );
    assert!(p.refs.iter().all(|r| r.kind == "include"));
    assert!(p.symbols.is_empty() && p.provides.is_empty());
    let got: Vec<_> = p
        .refs
        .iter()
        .map(|r| {
            (
                r.spelling.as_str(),
                r.line,
                r.candidates.join(","),
                r.fallbacks.join(","),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            // 尖括号里只有文件名：多半是系统头，不猜。
            ("stdio.h", 1, String::new(), String::new()),
            ("util.h", 2, "native/src/util.h".into(), "inc:util.h".into()),
            (
                "core/log.h",
                3,
                "native/src/core/log.h".into(),
                "inc:core/log.h".into()
            ),
            ("core/log.h", 4, String::new(), "inc:core/log.h".into()),
            // 写了上级目录的只按相对位置找；越出项目根的没有候选。
            (
                "../shared/x.h",
                5,
                "native/shared/x.h".into(),
                String::new()
            ),
            ("../../../out.h", 6, String::new(), String::new()),
            (
                "spaced.h",
                7,
                "native/src/spaced.h".into(),
                "inc:spaced.h".into()
            ),
            (
                "win/api.h",
                8,
                "native/src/win/api.h".into(),
                "inc:win/api.h".into()
            ),
        ]
    );
    // 只有引号形式、且就在使用方旁边的那个候选可以算确定；按包含路径找到的不算。
    assert!(p.refs[1].exact && !p.refs[3].exact && !p.refs[0].exact);
    assert!(p.refs.iter().all(|r| r.substitutes.is_empty()));
}

#[test]
fn c_includes_under_conditions_are_never_certain_but_header_guards_do_not_count() {
    let p = parse(
        "src/util.h",
        "#ifndef UTIL_H\n#define UTIL_H\n#include \"types.h\"\n#ifdef _WIN32\n#include \"win.h\"\n#else\n#include \"posix.h\"\n#endif\n#endif // UTIL_H\n",
    );
    assert_eq!(
        certainty(&p),
        [("types.h", true), ("win.h", false), ("posix.h", false)]
    );
    // `#pragma once` 之后再写保护、`#if !defined(...)` 的写法同样成立。
    for head in [
        "#pragma once\n#ifndef UTIL_H\n#define UTIL_H 1\n",
        "/* 许可 */\n#if !defined(UTIL_H)\n#define UTIL_H\n",
        "#if !defined UTIL_H\n# define UTIL_H\n",
    ] {
        let p = parse(
            "src/util.h",
            &format!("{head}#include \"types.h\"\n#endif\n"),
        );
        assert_eq!(certainty(&p), [("types.h", true)], "{head}");
    }
    // 不是完整的保护：有 #else、保护之外有代码、定义的不是同一个宏、保护之后另有指令，
    // 或者根本是别的条件。不求值，所以 `#if 0` 里的也照样列出、只是不算确定。
    for source in [
        "#ifndef A\n#define A\n#include \"t.h\"\n#else\n#endif\n",
        "int x;\n#ifndef A\n#define A\n#include \"t.h\"\n#endif\n",
        "#ifndef A\n#define B\n#include \"t.h\"\n#endif\n",
        "#ifndef A\n#define A\n#include \"t.h\"\n#endif\n#include \"late.h\"\n",
        "#ifdef A\n#include \"t.h\"\n#endif\n",
        "#if 0\n#include \"t.h\"\n#endif\n",
    ] {
        let p = parse("src/u.h", source);
        assert_eq!(
            (p.refs[0].spelling.as_str(), p.refs[0].exact),
            ("t.h", false),
            "{source}"
        );
    }
    assert_eq!(
        certainty(&parse("src/a.c", "#include \"a.h\"\nint x;\n")),
        [("a.h", true)]
    );
}

#[test]
fn c_scanning_skips_comments_strings_and_joins_continued_lines() {
    let p = parse(
        "src/a.c",
        "/* #include \"in_block.h\" */\n// #include \"in_line.h\" \\\n#include \"still_comment.h\"\nconst char *a = \"#include \\\"in_string.h\\\"\";\nconst char *b = \"/* not a comment\";\n#include \\\n  \"joined.h\"\nint n = 1'000; char q = '\"';\n#include \"after.h\"\n#define TEXT \"x\" /* open\n#include \"hidden.h\"\n*/\n#include <a/b.h> // tail\n",
    );
    assert_eq!(
        located(&p),
        [("joined.h", 6), ("after.h", 9), ("a/b.h", 13)]
    );
    // 行号是指令自己所在的行，不是它前面那段注释开始的行。
    let p = parse(
        "src/b.c",
        "/* 许可\n   说明 */ #include \"a.h\"\n\n  /* x */\n#include \"b.h\"\n",
    );
    assert_eq!(located(&p), [("a.h", 2), ("b.h", 5)]);
    // C++ 的原始字符串可以跨行，里面的 #include 只是文本。
    let p = parse(
        "src/doc.cpp",
        "#include \"real.hpp\"\nconst char *doc = R\"x(\n#include \"fake.h\"\n)x\";\n#include \"tail.hpp\"\n",
    );
    assert_eq!(located(&p), [("real.hpp", 1), ("tail.hpp", 5)]);
}

#[test]
fn c_includes_named_by_macros_or_by_search_order_cannot_be_placed() {
    let p = parse(
        "src/a.c",
        "#include HEADER\n#include_next <stdlib.h>\n#include \"sp ace.h\"\n#include \"/abs/x.h\"\n#include \"dir/\"\n",
    );
    assert_eq!(p.refs.len(), 5);
    assert!(
        p.refs
            .iter()
            .all(|r| r.kind == "unsupported" && r.candidates.is_empty() && r.fallbacks.is_empty())
    );
}

#[test]
fn c_unbalanced_conditionals_and_open_comments_leave_no_facts() {
    let mut extractor = Extractor::default();
    for source in [
        "#include \"a.h\"\n#if X\n",
        "#include \"a.h\"\n#endif\n",
        "#else\n",
        "#include \"a.h\"\n/* 没有结束\n",
    ] {
        let p = extractor.extract("src/a.c", source).unwrap();
        assert!(p.syntax_error && p.refs.is_empty(), "{source}");
    }
}

#[test]
fn c_headers_answer_to_every_suffix_of_their_path() {
    let extractor = Extractor::default();
    assert_eq!(
        extractor.path_keys("native/include/core/log.h"),
        [
            "inc:native/include/core/log.h",
            "inc:include/core/log.h",
            "inc:core/log.h",
            "inc:log.h"
        ]
    );
    assert_eq!(extractor.path_keys("log.hpp"), ["inc:log.hpp"]);
    assert_eq!(extractor.path_keys("a\\b.hh"), ["inc:a/b.hh", "inc:b.hh"]);
    // 包含路径上找的是头文件；源文件只能按相对位置被包含。
    for path in ["src/main.c", "src/main.cpp", "src/main.ts", "README.md"] {
        assert!(extractor.path_keys(path).is_empty(), "{path}");
    }
}
