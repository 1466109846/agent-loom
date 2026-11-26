//! Go、Java、Vue、C/C++ 和 tsconfig / jsconfig 别名经命令行走完整条路径：
//! 索引、双向查询、文本与 JSON 输出。
use serde_json::Value;
use std::{fs, path::Path, process::Command};

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_loom"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}
fn json(root: &Path, args: &[&str]) -> Value {
    let out = run(root, args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("结构结果可用作机器输入")
}
fn text(root: &Path, args: &[&str]) -> String {
    let out = run(root, args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
fn init(root: &Path) {
    let out = run(root, &["--root", ".", "init"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn write(root: &Path, file: &str, text: &str) {
    let p = root.join(file);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}
fn index(root: &Path) -> Value {
    json(root, &["index", "--json"])
}
fn explain(root: &Path, p: &str) -> Value {
    json(root, &["explain", p, "--json"])
}
fn impact(root: &Path, p: &str) -> Value {
    json(root, &["impact", p, "--json"])
}
/// 每条关系的另一端、种类、确定程度和使用方行号。
fn edges<'v>(v: &'v Value, end: &str) -> Vec<(&'v str, &'v str, &'v str, u64)> {
    v["relations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e[end].as_str().unwrap(),
                e["kind"].as_str().unwrap(),
                e["confidence"].as_str().unwrap(),
                e["line"].as_u64().unwrap(),
            )
        })
        .collect()
}
fn gaps(v: &Value) -> Vec<(&str, &str)> {
    v["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["spelling"].as_str().unwrap(),
                i["state"].as_str().unwrap(),
            )
        })
        .collect()
}
fn names(v: &Value) -> Vec<&str> {
    v["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect()
}

const APP_VUE: &str = "<template>\n  <Child />\n</template>\n\n<script setup lang=\"ts\">\nimport Child from './components/Child.vue'\nimport { useStore } from './store.js'\nimport axios from 'axios'\n</script>\n";

#[test]
fn vue_components_link_to_scripts_and_to_each_other_in_both_directions() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    init(r);
    write(r, "web/src/main.ts", "import App from './App.vue'\n");
    write(r, "web/src/App.vue", APP_VUE);
    write(
        r,
        "web/src/components/Child.vue",
        "<template><p>child</p></template>\n",
    );
    write(r, "web/src/store.ts", "export function useStore() {}\n");
    let first = index(r);
    assert_eq!(first["seen"], 4);
    assert_eq!(first["uncovered"], 0);
    assert_eq!(first["relations"], 3);
    assert_eq!(first["unresolved"], 1);

    // 行号是整份 .vue 文件里的行号，不是脚本块内的。
    let app = explain(r, "web/src/App.vue");
    assert_eq!(app["status"], "indexed");
    assert_eq!(
        edges(&app, "target"),
        [
            ("web/src/components/Child.vue", "import", "exact", 6),
            ("web/src/store.ts", "import", "likely", 7),
        ]
    );
    assert_eq!(gaps(&app), [("axios", "unsupported")]);
    assert_eq!(
        edges(&impact(r, "web/src/components/Child.vue"), "source"),
        [("web/src/App.vue", "import", "exact", 6)]
    );
    assert_eq!(
        edges(&impact(r, "web/src/App.vue"), "source"),
        [("web/src/main.ts", "import", "exact", 1)]
    );
    let two = json(r, &["impact", "web/src/store.ts", "--depth", "2", "--json"]);
    assert_eq!(
        edges(&two, "source"),
        [
            ("web/src/App.vue", "import", "likely", 7),
            ("web/src/main.ts", "import", "exact", 1),
        ]
    );
    // 只有模板的组件是已分析的文件，只是没有引用别的文件。
    let child = explain(r, "web/src/components/Child.vue");
    assert_eq!(child["status"], "indexed");
    assert!(edges(&child, "target").is_empty());
    assert!(
        text(r, &["explain", "web/src/components/Child.vue"])
            .contains("本次快照的受管范围内没有查到它依赖的本地文件")
    );

    // 脚本块没写完：整份组件算未覆盖，指向它的导入不能再当作已确认的关系。
    write(
        r,
        "web/src/App.vue",
        "<script setup lang=\"ts\">\nimport Child from './components/Child.vue'\n",
    );
    let broken = index(r);
    assert_eq!(broken["parsed"], 1);
    assert_eq!(broken["uncovered"], 1);
    assert_eq!(explain(r, "web/src/App.vue")["status"], "syntax_error");
    assert!(edges(&impact(r, "web/src/components/Child.vue"), "source").is_empty());
    let main = explain(r, "web/src/main.ts");
    assert!(edges(&main, "target").is_empty());
    assert_eq!(gaps(&main), [("./App.vue", "uncovered_target")]);
    assert_eq!(
        main["issues"][0]["candidates"],
        serde_json::json!(["web/src/App.vue"])
    );

    write(r, "web/src/App.vue", APP_VUE);
    let fixed = index(r);
    assert_eq!(fixed["parsed"], 1);
    assert_eq!(fixed["relations"], 3);

    // 省略了扩展名的导入：脚本扩展名都落空才认组件和声明文件，都只算可能。
    write(
        r,
        "web/src/pages.ts",
        "import Child from './components/Child'\nimport type { Kind } from './kinds'\n",
    );
    write(r, "web/src/kinds.d.ts", "export type Kind = 'a'\n");
    index(r);
    assert_eq!(
        edges(&explain(r, "web/src/pages.ts"), "target"),
        [
            ("web/src/components/Child.vue", "import", "likely", 1),
            ("web/src/kinds.d.ts", "import", "likely", 2),
        ]
    );
    write(r, "web/src/components/Child.ts", "export default {}\n");
    index(r);
    assert_eq!(
        edges(&explain(r, "web/src/pages.ts"), "target")[0],
        ("web/src/components/Child.ts", "import", "likely", 1)
    );
}

#[test]
fn go_java_and_c_files_answer_what_they_use_and_who_uses_them() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    init(r);
    write(r, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        r,
        "cmd/main.go",
        "package main\n\nimport (\n\t\"fmt\"\n\n\t\"example.com/app/store\"\n)\n\nfunc main() {\n\tfmt.Println(store.Open())\n\tstore.Close()\n}\n",
    );
    write(
        r,
        "store/open.go",
        "package store\n\nfunc Open() int { return prepare() }\n",
    );
    write(
        r,
        "store/close.go",
        "package store\n\nfunc Close() {}\nfunc prepare() int { return 1 }\n",
    );
    write(
        r,
        "java/com/acme/web/Handler.java",
        "package com.acme.web;\n\nimport com.acme.model.Item;\nimport java.util.List;\n\npublic class Handler {\n    Helper helper;\n    List<Item> items;\n}\n",
    );
    write(
        r,
        "java/com/acme/web/Helper.java",
        "package com.acme.web;\n\nclass Helper {}\n",
    );
    write(
        r,
        "java/com/acme/model/Item.java",
        "package com.acme.model;\n\npublic class Item {}\nclass Internal {}\n",
    );
    write(
        r,
        "native/src/main.c",
        "#include <stdio.h>\n#include \"util.h\"\n#include \"core/log.h\"\n",
    );
    write(
        r,
        "native/src/util.h",
        "#ifndef UTIL_H\n#define UTIL_H\n#include \"core/log.h\"\n#endif\n",
    );
    write(r, "native/include/core/log.h", "#pragma once\n");
    let first = index(r);
    // go.mod 只是解析的输入，不算受管源码。
    assert_eq!(first["seen"], 9);
    assert_eq!(first["uncovered"], 0);

    // Go：经包名用到的导出名落到声明它的文件上，同包的名字按名字去找。
    let main = explain(r, "cmd/main.go");
    assert_eq!(
        edges(&main, "target"),
        [
            ("store/close.go", "use", "exact", 11),
            ("store/open.go", "use", "exact", 10),
        ]
    );
    assert_eq!(gaps(&main), [("fmt", "external")]);
    assert_eq!(
        edges(&explain(r, "store/open.go"), "target"),
        [("store/close.go", "mention", "exact", 3)]
    );
    let close = impact(r, "store/close.go");
    assert_eq!(
        edges(&close, "source"),
        [
            ("cmd/main.go", "use", "exact", 11),
            ("store/open.go", "mention", "exact", 3),
        ]
    );
    // 没有导出的顶层名字不作为符号展示。
    assert_eq!(names(&explain(r, "store/close.go")), ["Close"]);
    let shown = text(r, &["explain", "cmd/main.go"]);
    assert!(
        shown.contains("cmd/main.go → store/open.go · 第 10 行 · 静态确定 · 第 1 跳"),
        "{shown}"
    );
    assert!(shown.contains("cmd/main.go:4 · fmt · 外部引用"), "{shown}");

    // Java：导入按“包 + 类型”找到文件，同包的类型不经导入也连得上。
    let handler = explain(r, "java/com/acme/web/Handler.java");
    assert_eq!(
        edges(&handler, "target"),
        [
            ("java/com/acme/model/Item.java", "import", "exact", 3),
            ("java/com/acme/web/Helper.java", "mention", "likely", 7),
        ]
    );
    assert_eq!(gaps(&handler), [("java.util.List", "external")]);
    assert_eq!(names(&handler), ["Handler"]);
    assert_eq!(
        names(&explain(r, "java/com/acme/model/Item.java")),
        ["Item"]
    );
    assert_eq!(
        edges(&impact(r, "java/com/acme/web/Helper.java"), "source"),
        [("java/com/acme/web/Handler.java", "mention", "likely", 7)]
    );

    // C：旁边的头文件是确定的，靠包含路径找到的只算可能，系统头没有候选。
    let c = explain(r, "native/src/main.c");
    assert_eq!(
        edges(&c, "target"),
        [
            ("native/include/core/log.h", "include", "likely", 3),
            ("native/src/util.h", "include", "exact", 2),
        ]
    );
    assert_eq!(gaps(&c), [("stdio.h", "missing")]);
    // 两跳之内还看得到：main.c 也经 util.h 间接用到它。
    let log = json(
        r,
        &[
            "impact",
            "native/include/core/log.h",
            "--depth",
            "2",
            "--json",
        ],
    );
    assert_eq!(
        edges(&log, "source"),
        [
            ("native/src/main.c", "include", "likely", 3),
            ("native/src/util.h", "include", "likely", 3),
            ("native/src/main.c", "include", "exact", 2),
        ]
    );
    assert_eq!(log["relations"][2]["hop"], 2);

    // 声明换了文件：使用方一个字没动，结论跟着声明走。
    write(r, "store/close.go", "package store\n\nfunc Close() {}\n");
    write(
        r,
        "store/prepare.go",
        "package store\n\nfunc prepare() int { return 1 }\n",
    );
    let moved = index(r);
    assert_eq!(moved["parsed"], 2);
    assert_eq!(
        edges(&explain(r, "store/open.go"), "target"),
        [("store/prepare.go", "mention", "exact", 3)]
    );
    assert_eq!(
        edges(&impact(r, "store/close.go"), "source"),
        [("cmd/main.go", "use", "exact", 11)]
    );
}

#[test]
fn imports_through_jsconfig_aliases_reach_scripts_and_components_as_likely() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    init(r);
    write(
        r,
        "jsconfig.json",
        "{\n  \"compilerOptions\": {\n    \"baseUrl\": \"./\",\n    \"paths\": { \"@/*\": [\"src/*\"] }\n  },\n  \"exclude\": [\"node_modules\", \"dist\"]\n}\n",
    );
    write(
        r,
        "src/main.js",
        "import App from './App.vue'\nimport router from '@/router'\n",
    );
    write(
        r,
        "src/App.vue",
        "<template><Layout /></template>\n<script>\nimport Layout from '@/layout'\nimport { getToken } from '@/utils/auth'\nimport Vue from 'vue'\nconst load = () => import(/* webpackChunkName: 'router' */ '@/router')\nexport default { components: { Layout } }\n</script>\n",
    );
    write(r, "src/router/index.js", "export default {}\n");
    write(r, "src/layout/index.vue", "<template><div /></template>\n");
    write(r, "src/utils/auth.js", "export function getToken() {}\n");
    let first = index(r);
    // jsconfig.json 只是解析的输入，不算受管源码。
    assert_eq!(first["seen"], 5);
    assert_eq!(first["relations"], 5);
    assert_eq!(first["unresolved"], 1);

    // 经别名找到的目标只算可能；省略扩展名时脚本都落空才认组件。
    let app = explain(r, "src/App.vue");
    assert_eq!(
        edges(&app, "target"),
        [
            ("src/layout/index.vue", "import", "likely", 3),
            ("src/router/index.js", "import", "likely", 6),
            ("src/utils/auth.js", "import", "likely", 4),
        ]
    );
    assert_eq!(gaps(&app), [("vue", "unsupported")]);
    assert_eq!(
        edges(&explain(r, "src/main.js"), "target"),
        [
            ("src/App.vue", "import", "exact", 1),
            ("src/router/index.js", "import", "likely", 2),
        ]
    );
    assert_eq!(
        edges(&impact(r, "src/utils/auth.js"), "source"),
        [("src/App.vue", "import", "likely", 4)]
    );
    let shown = text(r, &["explain", "src/App.vue"]);
    assert!(
        shown.contains("src/App.vue → src/utils/auth.js · 第 4 行 · 可能 · 第 1 跳"),
        "{shown}"
    );

    // 去掉别名：同一份组件不再连得上，裸名回到无法静态确定。
    write(r, "jsconfig.json", "{ \"compilerOptions\": {} }\n");
    let bare = index(r);
    assert_eq!(bare["parsed"], 5);
    assert_eq!(bare["relations"], 1);
    let app = explain(r, "src/App.vue");
    assert!(edges(&app, "target").is_empty());
    assert_eq!(
        gaps(&app),
        [
            ("@/layout", "unsupported"),
            ("@/utils/auth", "unsupported"),
            ("vue", "unsupported"),
            ("@/router", "unsupported"),
        ]
    );
}
