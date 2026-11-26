//! 通过真实 CLI、独立项目库及两次以上显式索引验证声明证据，而不是断言解析器候选。
use std::{fs, path::Path, process::Command};

use serde_json::Value;
use tempfile::TempDir;

fn command(root: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new(env!("CARGO_BIN_EXE_loom"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn json(root: &Path, args: &[&str]) -> Value {
    serde_json::from_slice(&command(root, args)).unwrap()
}

fn write(root: &Path, path: &str, src: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, src).unwrap();
}

fn project(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    write(
        r,
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    for (path, text) in files {
        write(r, path, text);
    }
    command(r, &["--root", ".", "init"]);
    dir
}

fn index(r: &Path) -> Value {
    json(r, &["index", "--json"])
}

fn outgoing(r: &Path, src: &str) -> Value {
    json(r, &["explain", src, "--json"])
}

fn edges(v: &Value) -> &[Value] {
    v["relations"].as_array().unwrap()
}

fn check_import(r: &Path, src: &str, line: u64, target: &str, proved: bool) {
    let out = outgoing(r, src);
    let incoming = json(r, &["impact", target, "--json"]);
    for report in [&out, &incoming] {
        let found: Vec<_> = edges(report)
            .iter()
            .filter(|edge| edge["source"] == src && edge["line"] == line)
            .collect();
        assert_eq!(
            found.len(),
            usize::from(proved),
            "{src}:{line} → {target}: {report}"
        );
        if proved {
            assert_eq!(found[0]["target"], target);
            assert_eq!(found[0]["kind"], "import");
            assert_eq!(found[0]["confidence"], "exact");
        }
    }
    assert_eq!(
        out["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["line"] == line),
        !proved
    );
}

#[test]
fn only_the_final_provider_counts_and_editing_a_middle_declaration_retracts_both_directions() {
    let d = project(&[
        ("src/lib.rs", "mod consumer;\nmod domain;\n"),
        (
            "src/consumer.rs",
            "// 证据在下一行\nuse crate::domain::api::{Job, run};\n",
        ),
        ("src/domain.rs", "pub(crate) mod api;\n"),
        (
            "src/domain/api.rs",
            "pub(crate) struct Job;\npub fn run() {}\n",
        ),
        ("unrelated/api.rs", "pub struct Job;\npub fn run() {}\n"),
    ]);
    let r = d.path();
    let check = |yes| check_import(r, "src/consumer.rs", 2, "src/domain/api.rs", yes);
    assert_eq!(index(r)["parsed"], 5);
    check(true);
    assert_eq!(edges(&outgoing(r, "src/consumer.rs")).len(), 1);
    let idle = index(r);
    assert_eq!(idle["parsed"], 0);
    assert_eq!(idle["relinked"], 0);

    for declaration in [
        "// 不再声明 api\n",
        "#[cfg(test)] pub mod api;\n",
        "mod api;\n",
        "pub mod api { pub struct Job; }\n",
    ] {
        write(r, "src/domain.rs", declaration);
        check(true); // 未显式索引时仍是旧快照。
        let changed = index(r);
        assert_eq!(changed["parsed"], 1);
        assert_eq!(changed["unchanged"], 4);
        check(false);
        write(r, "src/domain.rs", "pub(crate) mod api;\n");
        index(r);
        check(true);
    }
    // 仅组内一个末端消失也不能沿用前缀或只证明另一半。
    write(r, "src/domain/api.rs", "pub struct Job;\n");
    index(r);
    check(false);
}

#[test]
fn a_path_prefix_and_an_exported_name_are_not_proof_of_the_final_item() {
    let d = project(&[
        (
            "src/lib.rs",
            "mod api;\nmod consumer;\nmod origin;\nuse crate::api::Missing;\n",
        ),
        (
            "src/api.rs",
            "pub struct Good;\npub use crate::origin::Forwarded;\npub use crate::origin::Original as Renamed;\n#[cfg(test)] pub struct Gated;\npub mod inner { pub struct Nested; }\npub struct Twice;\n#[cfg(test)] pub struct Twice;\nfn private() {}\n",
        ),
        (
            "src/origin.rs",
            "pub struct Forwarded;\npub struct Original;\npub struct Missing;\n",
        ),
        (
            "src/consumer.rs",
            "use crate::api::Forwarded;\nuse crate::api::Renamed;\nuse crate::api::Gated;\nuse crate::api::Nested;\nuse crate::api::inner::Nested;\nuse crate::api::Twice;\nuse crate::api::private;\nuse crate::api::Good as Alias;\n#[cfg(test)] use crate::api::Good;\nmod local { use crate::api::Good; }\nfn local_use() { use crate::api::Good; }\nuse crate::api::{Good, Missing};\n",
        ),
        ("src/orphan.rs", "use crate::api::Good;\n"),
    ]);
    let r = d.path();
    index(r);
    check_import(r, "src/lib.rs", 4, "src/api.rs", false);
    for path in ["src/consumer.rs", "src/orphan.rs"] {
        let report = outgoing(r, path);
        assert!(edges(&report).is_empty(), "{report}");
        let issues = report["issues"].as_array().unwrap();
        assert_eq!(issues.len(), if path == "src/consumer.rs" { 12 } else { 1 });
        assert!(issues.iter().all(|i| i["state"] != "external"));
    }
}

#[test]
fn ambiguous_layout_or_second_owner_never_becomes_a_unique_crate() {
    let d = project(&[
        ("src/lib.rs", "mod consumer;\nmod api;\n"),
        ("src/consumer.rs", "use crate::api::Job;\n"),
        ("src/api/mod.rs", "pub struct Job;\n"),
        ("src/main.rs", "fn main() {}\n"),
    ]);
    let r = d.path();
    let check = |yes| check_import(r, "src/consumer.rs", 1, "src/api/mod.rs", yes);
    index(r);
    check(true); // 同目录有 lib/main，不等于同一文件有两个声明所有者。
    for second in [
        "mod consumer;",
        "#[cfg(test)] mod consumer;",
        "#[path = \"consumer.rs\"] mod shared;",
        "#[cfg_attr(test, path = \"consumer.rs\")] mod other;",
    ] {
        write(r, "src/main.rs", &format!("{second}\nfn main() {{}}\n"));
        index(r);
        check(false);
        write(r, "src/main.rs", "fn main() {}\n");
        index(r);
        check(true);
    }
    write(r, "src/api.rs", "fn broken("); // 存在但未覆盖的另一种布局也参与消歧。
    assert_eq!(index(r)["uncovered"], 1);
    check(false);
    fs::remove_file(r.join("src/api.rs")).unwrap();
    index(r);
    check(true);
}

#[test]
fn a_target_override_or_a_hidden_alternative_cannot_prove_ownership() {
    let d = project(&[
        ("src/lib.rs", "mod api;\nmod consumer;\n"),
        ("src/api.rs", "pub struct Job;\n"),
        ("src/consumer.rs", "use crate::api::Job;\n"),
    ]);
    let r = d.path();
    let check = |yes| check_import(r, "src/consumer.rs", 1, "src/api.rs", yes);
    let manifest = fs::read_to_string(r.join("Cargo.toml")).unwrap();
    index(r);
    check(true);
    write(
        r,
        "Cargo.toml",
        &format!("{manifest}\n[[bin]]\nname = \"also_consumer\"\npath = \"src/consumer.rs\"\n"),
    );
    assert_eq!(index(r)["parsed"], 0);
    check(false);
    write(r, "Cargo.toml", &manifest);
    index(r);
    check(true);
    write(r, ".loom/ignore", "/src/api/mod.rs\n");
    write(r, "src/api/mod.rs", "pub struct Job;\n");
    let ignored = index(r);
    assert_eq!(ignored["seen"], 3);
    assert_eq!(ignored["parsed"], 0);
    check(false);
    fs::remove_file(r.join("src/api/mod.rs")).unwrap();
    index(r);
    check(true);
}

#[test]
fn a_familiar_attribute_name_does_not_prove_an_item_survives_expansion() {
    let d = project(&[
        ("src/lib.rs", "mod consumer;\nmod api;\n"),
        ("src/consumer.rs", "use crate::api::{Tag, Record};\n"),
        (
            "src/api.rs",
            "#[derive(serde::Serialize)]\n#[serde(rename_all = \"snake_case\")]\npub enum Tag { On }\npub struct Record;\n",
        ),
    ]);
    let r = d.path();
    let check = |yes| check_import(r, "src/consumer.rs", 1, "src/api.rs", yes);
    index(r);
    // 不展开过程属性，也未证明宏身份；包括疑似 serde 辅助属性在内都不能只按名字放行。
    check(false);
    for attrs in [
        "#[serde(rename_all = \"snake_case\")]",
        "#[derive(Debug)] #[serde(rename_all = \"snake_case\")]",
        "#[derive(serde::Serialize)] #[cfg_attr(test, derive(Debug))]",
        "#[derive(serde::Serialize)] #[other]",
    ] {
        write(
            r,
            "src/api.rs",
            &format!("{attrs}\npub enum Tag {{ On }}\npub struct Record;"),
        );
        index(r);
        check(false);
    }
    write(
        r,
        "src/api.rs",
        "#[derive(Debug)] pub enum Tag { On }\npub struct Record;",
    );
    index(r);
    check(true);
    write(
        r,
        "src/consumer.rs",
        "use crate::api::Tag;\nuse other::Type as Tag;\n",
    );
    index(r);
    check(false);
}
