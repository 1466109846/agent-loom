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
fn init(root: &Path) {
    let out = run(root, &["--root", ".", "init"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn write(root: &Path, file: &str, text: &[u8]) {
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
fn rel(v: &Value) -> &[Value] {
    v["relations"].as_array().unwrap()
}

#[test]
fn directions_evidence_and_incremental_retraction() {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    init(r);
    write(
        r,
        "src/entry.ts",
        b"// entry\nimport { save } from './writer';\nexport function run() { save(); }\n",
    );
    write(r, "src/writer.ts", b"export function save() {}\n");
    write(r, "src/unrelated.ts", b"export const other = 1;\n");
    assert_eq!(explain(r, "src/entry.ts")["status"], "not_indexed");
    assert_eq!(index(r)["parsed"], 3);
    let out = explain(r, "src/entry.ts");
    let incoming = impact(r, "src/writer.ts");
    assert_eq!(rel(&out).len(), 1);
    assert_eq!(rel(&incoming).len(), 1);
    for item in [&rel(&out)[0], &rel(&incoming)[0]] {
        assert_eq!(item["source"], "src/entry.ts");
        assert_eq!(item["target"], "src/writer.ts");
        assert_eq!(item["line"], 2);
        assert_eq!(item["confidence"], "likely");
    }
    assert!(
        explain(r, "src/writer.ts")["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "save")
    );
    let idle = index(r);
    assert_eq!(idle["parsed"], 0);
    assert_eq!(idle["unchanged"], 3);
    write(r, "src/entry.ts", b"export function run() {}\n");
    let changed = index(r);
    assert_eq!(changed["parsed"], 1);
    assert_eq!(changed["unchanged"], 2);
    assert!(rel(&impact(r, "src/writer.ts")).is_empty());
    assert!(rel(&explain(r, "src/entry.ts")).is_empty());
}

#[test]
fn graph_relinks_unchanged_importer_without_guessing_or_using_old_symbols() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    init(r);
    write(r, "a.ts", b"import './store';\n");
    write(r, "store.ts", b"export const oldName=1;\n");
    index(r);
    write(r, "store.js", b"export const alt=1;\n");
    assert_eq!(index(r)["parsed"], 1);
    let out = explain(r, "a.ts");
    assert!(rel(&out).is_empty());
    assert!(
        out["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["state"] == "ambiguous")
    );
    assert_eq!(
        out["issues"][0]["candidates"],
        serde_json::json!(["store.js", "store.ts"])
    );
    assert!(rel(&impact(r, "store.ts")).is_empty());
    fs::remove_file(r.join("store.ts")).unwrap();
    let change = index(r);
    assert_eq!(change["removed"], 1);
    assert_eq!(change["parsed"], 0);
    let now = explain(r, "a.ts");
    assert_eq!(rel(&now)[0]["target"], "store.js");
    assert_eq!(explain(r, "store.ts")["status"], "absent");
    fs::rename(r.join("store.js"), r.join("renamed.js")).unwrap();
    index(r);
    assert!(rel(&explain(r, "a.ts")).is_empty());
    assert!(
        explain(r, "a.ts")["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["state"] == "missing")
    );
    assert!(rel(&impact(r, "renamed.js")).is_empty());
}

#[test]
fn ignored_and_uncovered_files_are_not_definite_evidence() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    init(r);
    write(r, ".gitignore", b"/build-area/\n");
    write(r, "build-area/ignored.ts", b"export const ignored=1;");
    write(r, ".loom/ignore", b"local-only/\n");
    write(r, "local-only/file.ts", b"export const local=1;");
    write(r, "node_modules/vendor/private.ts", b"export const fake=1;");
    write(r, ".env.ts", b"export const mode=1;");
    write(r, "large.py", &vec![b'#'; 1_048_577]);
    write(r, "bad.ts", &[0xff, 0x80]);
    write(r, "good.ts", b"import './bad';\n");
    let report = index(r);
    assert_eq!(report["scope"], "git");
    assert_eq!(report["uncovered"], 2);
    for p in [
        "build-area/ignored.ts",
        "local-only/file.ts",
        "node_modules/vendor/private.ts",
    ] {
        assert_eq!(explain(r, p)["status"], "absent", "{p}");
    }
    assert_eq!(explain(r, ".env.ts")["status"], "indexed");
    let text = run(r, &["index"]);
    assert!(
        String::from_utf8(text.stdout)
            .unwrap()
            .trim_end()
            .ends_with("范围：以 Git 忽略规则为准。")
    );
    assert_eq!(explain(r, "large.py")["status"], "too_large");
    assert_eq!(explain(r, "bad.ts")["status"], "invalid_utf8");
    assert!(
        impact(r, "good.ts")["coverage"]["uncovered_files"]
            .as_u64()
            .unwrap()
            >= 2
    );
    write(r, "good.ts", b"import {\n");
    index(r);
    assert_eq!(explain(r, "good.ts")["status"], "syntax_error");
    assert!(rel(&impact(r, "bad.ts")).is_empty());

    // 此系统临时目录不在任何 Git 工作区里；移除根规则后才使用内置名单。
    fs::remove_file(r.join(".gitignore")).unwrap();
    assert_eq!(index(r)["scope"], "builtin");
    assert_eq!(explain(r, ".env.ts")["status"], "absent");
    assert_eq!(explain(r, "build-area/ignored.ts")["status"], "indexed");
    let text = run(r, &["index"]);
    assert!(
        String::from_utf8(text.stdout)
            .unwrap()
            .trim_end()
            .ends_with("范围：没有 Git 忽略规则，按内置名单排除构建产物、依赖和私密目录。")
    );
}

#[test]
fn failed_refresh_does_not_publish_partial_changes_and_nested_roots_are_excluded() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    init(r);
    write(r, "entry.ts", b"import './leaf';\n");
    write(r, "leaf.ts", b"export const leaf=1;\n");
    let nested = r.join("other");
    fs::create_dir(&nested).unwrap();
    init(&nested);
    write(&nested, "hidden.ts", b"export const privateDecision=1;\n");
    assert_eq!(index(r)["seen"], 2);
    assert_eq!(explain(r, "other/hidden.ts")["status"], "absent");
    let before = impact(r, "leaf.ts");
    assert_eq!(rel(&before).len(), 1);
    write(r, "entry.ts", b"export const entry=1;\n");
    write(r, ".loom/ignore", &[0xfe, 0xff]);
    assert!(!run(r, &["index"]).status.success());
    let after = impact(r, "leaf.ts");
    for (key, val) in before.as_object().unwrap() {
        if key != "verification" && key != "conclusion" {
            assert_eq!(&after[key], val, "{key}");
        }
    }
    assert_eq!(after["conclusion"]["verdict"], "found");
    assert!(
        after["verification"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["state"] == "unverifiable")
    );
    write(r, ".loom/ignore", b"# usable again\n");
    assert_eq!(index(r)["parsed"], 1);
    assert!(rel(&impact(r, "leaf.ts")).is_empty());
}

#[test]
fn a_large_dependency_list_has_an_explicit_total_text_limit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init(root);
    let mut imports = String::new();
    for i in 0..70 {
        let name = format!("模块{}{:02}.ts", "验".repeat(36), i);
        imports.push_str(&format!("import './{name}';\n"));
        write(root, &name, b"export const a=1;\n");
    }
    write(root, "root.ts", imports.as_bytes());
    index(root);
    let structured = explain(root, "root.ts");
    assert_eq!(structured["truncated"], true);
    assert_eq!(rel(&structured).len(), 64);
    assert_eq!(
        structured["verification"]["files"]
            .as_array()
            .unwrap()
            .len(),
        32
    );
    assert_eq!(structured["verification"]["skipped"], 33);
    assert_eq!(
        structured["conclusion"]["gaps"],
        serde_json::json!(["truncated"])
    );
    let result = run(root, &["explain", "root.ts"]);
    assert!(result.status.success());
    let text = String::from_utf8(result.stdout).unwrap();
    assert!(text.len() <= 6144);
    assert!(text.contains("已截短"));
    assert!(text.contains("关系 64") && text.contains("预览省略") && text.contains("不能判断"));
}

#[test]
fn a_custom_project_database_is_never_indexed_as_source() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    assert!(
        run(r, &["--root", ".", "--db", "local.py", "init"])
            .status
            .success()
    );
    write(r, "module.py", b"def go():\n    pass\n");
    let report = json(r, &["--db", "local.py", "index", "--json"]);
    assert_eq!(report["seen"], 1);
    assert_eq!(
        json(r, &["--db", "local.py", "explain", "local.py", "--json"])["status"],
        "absent"
    );
}

#[test]
#[cfg(windows)]
fn an_unreadable_module_is_not_reported_as_having_no_impact() {
    use std::os::windows::fs::OpenOptionsExt;
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    init(r);
    write(r, "a.ts", b"import './writer';\n");
    write(r, "writer.ts", b"export const a=1;\n");
    index(r);
    let locked = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(r.join("writer.ts"))
        .unwrap();
    let failed = index(r);
    assert_eq!(failed["uncovered"], 1);
    assert_eq!(failed["coverage"]["unreadable"], 1);
    let out = impact(r, "writer.ts");
    assert_eq!(out["status"], "unreadable");
    assert_eq!(out["coverage"]["uncovered"]["unreadable"], 1);
    assert_eq!(out["verification"]["files"][0]["state"], "unverifiable");
    assert_eq!(out["conclusion"]["verdict"], "gap");
    let pending = explain(r, "a.ts");
    assert!(rel(&pending).is_empty());
    assert!(
        pending["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["state"] == "uncovered_target")
    );
    drop(locked);
    let fixed = index(r);
    assert_eq!(fixed["parsed"], 1);
    assert_eq!(fixed["unchanged"], 1);
    assert_eq!(rel(&impact(r, "writer.ts")).len(), 1);
}

#[test]
fn convention_based_targets_stay_likely_and_never_outrank_the_written_file() {
    fn edges(v: &Value) -> Vec<(&str, &str)> {
        rel(v)
            .iter()
            .map(|e| {
                (
                    e["target"].as_str().unwrap(),
                    e["confidence"].as_str().unwrap(),
                )
            })
            .collect()
    }
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    init(r);
    write(
        r,
        "web/a.ts",
        b"import './store.js';\nimport './tool.mjs';\n",
    );
    write(r, "web/tool.mjs", b"export const t=1;\n");
    write(r, "src/app/core.py", b"def run():\n    pass\n");
    write(
        r,
        "src/app/api.py",
        b"from app.core import run\nimport openai\n",
    );
    write(r, "tests/test_api.py", b"from app.api import run\n");
    index(r);
    // 书写的文件存在时是确定目标；只有源码替代候选而文件未出现时没有边。
    let out = explain(r, "web/a.ts");
    assert_eq!(edges(&out), [("web/tool.mjs", "exact")]);
    assert_eq!(out["issues"][0]["state"], "missing");
    // 源码出现后，未改动的使用方不重解析，只得到“可能”。
    write(r, "web/store.ts", b"export const s=1;\n");
    assert_eq!(index(r)["parsed"], 1);
    assert_eq!(
        edges(&explain(r, "web/a.ts")),
        [("web/store.ts", "likely"), ("web/tool.mjs", "exact")]
    );
    assert_eq!(rel(&impact(r, "web/store.ts"))[0]["line"], 1);
    // 产物与源码并存时不择一。
    write(r, "web/store.js", b"export const s=2;\n");
    index(r);
    let both = explain(r, "web/a.ts");
    assert_eq!(edges(&both), [("web/tool.mjs", "exact")]);
    assert_eq!(both["issues"][0]["state"], "ambiguous");
    assert_eq!(
        both["issues"][0]["candidates"],
        serde_json::json!(["web/store.js", "web/store.ts"])
    );
    fs::remove_file(r.join("web/store.ts")).unwrap();
    index(r);
    assert_eq!(
        edges(&explain(r, "web/a.ts")),
        [("web/store.js", "exact"), ("web/tool.mjs", "exact")]
    );
    // Python 的 src/ 包布局：包内和包外的绝对导入都只是“可能”，外部包不连到本地。
    for (target, source) in [
        ("src/app/core.py", "src/app/api.py"),
        ("src/app/api.py", "tests/test_api.py"),
    ] {
        let users = impact(r, target);
        assert_eq!(rel(&users).len(), 1, "{target}");
        assert_eq!(rel(&users)[0]["source"], source);
        assert_eq!(rel(&users)[0]["confidence"], "likely");
        assert_eq!(rel(&users)[0]["line"], 1);
    }
    let api = explain(r, "src/app/api.py");
    assert_eq!(edges(&api), [("src/app/core.py", "likely")]);
    assert_eq!(api["issues"][0]["spelling"], "openai");
    assert_eq!(api["issues"][0]["state"], "missing");
}

#[test]
fn imports_inside_branches_and_functions_are_possible_but_never_certain() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    init(r);
    write(r, "pkg/__init__.py", b"");
    write(r, "pkg/base.py", b"def base():\n    pass\n");
    write(r, "pkg/lazy.py", b"def lazy():\n    pass\n");
    write(
        r,
        "pkg/entry.py",
        b"from .base import base\nif flag:\n    import pkg.base\ndef run():\n    from .lazy import lazy\n    import json\n    return lazy\n",
    );
    index(r);
    let out = explain(r, "pkg/entry.py");
    let seen: Vec<_> = rel(&out)
        .iter()
        .map(|e| {
            (
                e["target"].as_str().unwrap(),
                e["line"].as_u64().unwrap(),
                e["confidence"].as_str().unwrap(),
            )
        })
        .collect();
    // 同一种相对写法：顶层是确定的，函数体里只算“可能”。
    assert_eq!(
        seen,
        [
            ("pkg/base.py", 1, "exact"),
            ("pkg/base.py", 3, "likely"),
            ("pkg/lazy.py", 5, "likely"),
        ]
    );
    assert_eq!(out["issues"][0]["spelling"], "json");
    assert_eq!(out["issues"][0]["state"], "missing");
    let users = impact(r, "pkg/lazy.py");
    assert_eq!(rel(&users).len(), 1);
    assert_eq!(rel(&users)[0]["source"], "pkg/entry.py");
    assert_eq!(rel(&users)[0]["confidence"], "likely");
    // 去掉函数里的导入后，这条“可能”关系在两个方向都撤回。
    write(r, "pkg/entry.py", b"from .base import base\n");
    assert_eq!(index(r)["parsed"], 1);
    assert!(rel(&impact(r, "pkg/lazy.py")).is_empty());
    assert_eq!(rel(&explain(r, "pkg/entry.py")).len(), 1);
}

#[test]
fn bounded_multihop_handles_cycles_and_finds_local_rust_and_python_modules() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    init(r);
    write(r, "lib.rs", b"pub mod writer;\n");
    write(r, "writer.rs", b"pub fn flush() {}\n");
    write(r, "pkg/__init__.py", b"");
    write(r, "pkg/entry.py", b"from .writer import flush\n");
    write(r, "pkg/writer.py", b"def flush():\n    pass\n");
    write(r, "a.ts", b"import './b';\n");
    write(r, "b.ts", b"import './c';\n");
    write(r, "c.ts", b"import './a';\n");
    index(r);
    assert_eq!(rel(&explain(r, "lib.rs"))[0]["target"], "writer.rs");
    assert_eq!(
        rel(&impact(r, "pkg/writer.py"))[0]["source"],
        "pkg/entry.py"
    );
    let two = json(r, &["impact", "c.ts", "--depth", "2", "--json"]);
    assert!(
        rel(&two)
            .iter()
            .any(|v| v["source"] == "a.ts" && v["hop"] == 2)
    );
    let three = json(r, &["impact", "c.ts", "--depth", "3", "--json"]);
    assert!(rel(&three).len() <= 3);
    assert!(!run(r, &["impact", "a.ts", "--depth", "4"]).status.success());
}
