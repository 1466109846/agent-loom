use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use serde_json::{Value, json};

#[path = "project_identity/user_config.rs"]
mod user_config;

fn loom(cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom"));
    cmd.current_dir(cwd).stdin(Stdio::null());
    cmd
}

fn ok(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn refused(cmd: &mut Command, message: &str) -> Output {
    let out = cmd.output().unwrap();
    assert!(!out.status.success(), "不应成功：{cmd:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(message),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn db(root: &Path) -> PathBuf {
    root.join(".loom/loom.db")
}

fn pointed(root: &Path) -> PathBuf {
    PathBuf::from(
        fs::read_to_string(root.join(".loom/database.path"))
            .unwrap()
            .trim_end(),
    )
}

#[test]
fn move_preview_and_round_trip_preserve_knowledge_structure_and_sharing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir(&root).unwrap();
    let run = |args: &[&str]| ok(loom(&root).args(args));
    run(&["--root", ".", "init"]);
    let source = "import { v } from './value';\nexport const use = v;\n";
    fs::write(root.join("use.ts"), source).unwrap();
    fs::write(root.join("value.ts"), "export const v = 1;\n").unwrap();
    let memory = run(&[
        "remember",
        "DECISION",
        "迁移需要核对行数",
        "逐表核对才能保证历史记录不丢失。",
    ]);
    let intent = run(&[
        "intent",
        "propose",
        "goal",
        "保留历史",
        "搬库不能丢失已确认的目标。",
    ]);
    run(&["intent", "confirm", intent.trim(), "--yes"]);
    run(&["index"]);
    run(&[
        "note",
        "set",
        "use.ts",
        &loom_core::util::sha256_hex(source.as_bytes()),
        "读取模块常量",
    ]);
    let check = || {
        assert!(run(&["stats"]).contains("合计 1"));
        assert!(run(&["recall", "核对行数"]).contains(memory.trim()));
        assert!(run(&["intent", "get", intent.trim()]).contains("保留历史"));
        assert!(run(&["note", "get", "use.ts"]).contains("读取模块常量"));
        assert!(run(&["explain", "use.ts"]).contains("value.ts"));
    };
    check();
    let original = fs::read(db(&root)).unwrap();
    let preview = run(&["db", "move", "../archive/"]);
    assert!(preview.contains("KiB") && preview.contains("加 --yes 执行"));
    // 还不存在、又没写库文件扩展名的目标按目录算；显示的是规整后的路径。
    let named = run(&["db", "move", "../archive"]);
    assert_eq!(named, preview);
    assert!(named.contains("loom.db") && !named.contains("..") && !named.contains(r"\\?"));
    assert!(run(&["db", "move", "../archive.db"]).contains("archive.db\n"));
    assert_eq!(fs::read(db(&root)).unwrap(), original);
    assert!(!root.join(".loom/database.path").exists() && !dir.path().join("archive").exists());
    run(&["db", "move", "../archive/", "--yes"]);
    let id = fs::read_to_string(root.join(".loom/project.id")).unwrap();
    let external = dir.path().join("archive").join(id.trim()).join("loom.db");
    assert_eq!(
        pointed(&root).canonicalize().unwrap(),
        external.canonicalize().unwrap()
    );
    assert!(!db(&root).exists());
    check();
    let snapshot: Value =
        serde_json::from_str(&run(&["share", "preview", "--memory", memory.trim()])).unwrap();
    let sha = snapshot["sha256"].as_str().unwrap();
    run(&[
        "share",
        "export",
        "snapshot.json",
        "--memory",
        memory.trim(),
        "--reviewed-sha",
        sha,
        "--yes",
    ]);
    // 分享拒绝把自己的快照重新收作外部来件。接收方也使用指针定位的外置库。
    let recipient = dir.path().join("recipient");
    fs::create_dir(&recipient).unwrap();
    ok(loom(&recipient)
        .args(["--root", ".", "init", "--db"])
        .arg(dir.path().join("recipient.db")));
    let snapshot = root.join("snapshot.json");
    ok(loom(&recipient).args(["share", "import"]).arg(&snapshot));
    ok(loom(&recipient)
        .args(["share", "import"])
        .arg(&snapshot)
        .args(["--reviewed-sha", sha, "--yes"]));
    run(&["db", "move", ".loom/loom.db", "--yes"]);
    assert!(!external.exists() && !root.join(".loom/database.path").exists());
    check();
}

#[test]
fn remembered_db_serves_subdirectories_and_relinks_after_manual_move() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("工程");
    let sub = root.join("src");
    fs::create_dir_all(&sub).unwrap();
    let external = dir.path().join("外置 #1%.db");
    let header = ok(loom(&root)
        .args(["--root", ".", "init", "--db"])
        .arg(&external));
    assert_eq!(pointed(&root), external);
    assert!(ok(loom(&sub).arg("stats")).starts_with(&header));
    assert!(seed_through_session(&sub, "外置库保留历史").starts_with(header.trim_end()));
    let moved = dir.path().join("手工移动.db");
    fs::rename(&external, &moved).unwrap();
    for cmd in [vec!["stats"], vec!["--root", ".", "init"]] {
        let out = refused(loom(&root).args(cmd), "init --db");
        assert!(String::from_utf8_lossy(&out.stderr).contains(external.to_str().unwrap()));
    }
    assert!(!external.exists() && !db(&root).exists());
    assert_eq!(
        ok(loom(&root)
            .args(["--root", ".", "init", "--db"])
            .arg(&moved)),
        header
    );
    assert_eq!(pointed(&root), moved);
    assert!(ok(loom(&sub).args(["recall", "保留历史"])).contains("外置库保留历史"));
    let id = fs::read_to_string(root.join(".loom/project.id")).unwrap();
    let new_root = dir.path().join("新项目地址");
    fs::rename(&root, &new_root).unwrap();
    refused(loom(&new_root).arg("stats"), "项目身份不符");
    ok(loom(&new_root).args(["--root", ".", "init", "--relocate", id.trim()]));
    assert_eq!(pointed(&new_root), moved);
    assert!(ok(loom(&new_root).args(["recall", "保留历史"])).contains("外置库保留历史"));
}

#[test]
fn data_dir_is_only_for_new_projects_and_stats_reports_the_remembered_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir(&root).unwrap();
    let config = dir.path().join("user/config.toml");
    let data = dir.path().join("Loom Data");
    ok(loom(&root)
        .env("LOOM_CONFIG", &config)
        .args(["config", "set", "storage.data_dir"])
        .arg(&data));
    let header = ok(loom(&root)
        .env("LOOM_CONFIG", &config)
        .args(["--root", ".", "init"]));
    let id = fs::read_to_string(root.join(".loom/project.id")).unwrap();
    let expected = data.join(id.trim()).join("loom.db");
    assert!(expected.is_file());
    assert_eq!(pointed(&root), expected);
    assert_eq!(fs::read_dir(root.join(".loom")).unwrap().count(), 3);
    let stats = ok(loom(&root).arg("stats"));
    assert_eq!(stats.lines().next(), Some(header.trim_end()));
    let second = stats.lines().nth(1).unwrap();
    assert!(
        second.starts_with(&format!("库：{} · ", expected.display())) && second.ends_with("KiB")
    );
    fs::write(&config, "[broken").unwrap();
    assert_eq!(
        ok(loom(&root)
            .env("LOOM_CONFIG", &config)
            .args(["--root", ".", "init"])),
        header
    );
    assert!(!db(&root).exists());
}

#[test]
fn foreign_pointer_never_authorizes_content_and_explicit_db_wins() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    for root in [&a, &b] {
        fs::create_dir(root).unwrap();
        ok(loom(root).args(["--root", ".", "init"]));
    }
    seed_through_session(&a, "只属于甲的决策");
    fs::write(b.join(".loom/database.path"), db(&a).to_str().unwrap()).unwrap();
    let out = refused(loom(&b).args(["recall", "只属于甲"]), "项目身份不符");
    assert!(
        out.stdout.is_empty() && !String::from_utf8_lossy(&out.stderr).contains("只属于甲的决策")
    );
    assert!(ok(loom(&b).arg("--db").arg(db(&b)).arg("stats")).contains("合计 0"));
}

#[test]
fn ordinary_lookup_never_creates_a_database_or_parent_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    refused(loom(root).arg("stats"), "未找到");
    refused(
        loom(root)
            .arg("--root")
            .arg(root)
            .args(["recall", "尚未绑定"]),
        "未初始化",
    );
    assert!(!root.join(".loom").exists());
}

#[test]
fn nearest_project_is_found_and_explicit_root_wins_over_working_directory() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("outer");
    let b = a.join("nested");
    let sub = b.join("src");
    fs::create_dir_all(&sub).unwrap();
    let first = ok(loom(&a).arg("--root").arg(&a).arg("init"));
    let second = ok(loom(&b).arg("--root").arg(&b).arg("init"));
    assert_ne!(first, second);
    assert!(ok(loom(&sub).arg("stats")).starts_with(second.trim_end()));
    assert!(ok(loom(&sub).arg("--root").arg(&a).arg("stats")).starts_with(first.trim_end()));
    assert!(!sub.join(".loom").exists());
}

/// 标准会话协议测试客户端，仅连接子进程，不打开端口。
struct Session(Child);
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn seed_through_session(root: &Path, title: &str) -> String {
    let mut child = Session(
        loom(root)
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let stdout = child.0.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let mut exchange = |request: Value| -> Value {
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
        if request.get("id").is_none() {
            return Value::Null;
        }
        let line = receive
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("会话响应超时或子进程已退出");
        let result: Value = serde_json::from_str(&line).expect("期望收到会话响应");
        assert_eq!(result["id"], request["id"]);
        assert!(
            result.get("error").is_none() && result["result"]["isError"] != true,
            "{result}"
        );
        result["result"].clone()
    };
    exchange(
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"loom-test","version":"1"}}}),
    );
    exchange(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    exchange(
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"remember","arguments":{"type":"DECISION","title":title,"body":"必须保留修订原因，以便将来核对设计结论。"}}}),
    );
    let response = exchange(
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"resume","arguments":{}}}),
    );
    // 正常结束会话，把最后的 WAL 提交合并到主文件，才能手工移动单个库文件。
    drop(input);
    assert!(child.0.wait().unwrap().success());
    response["content"][0]["text"].as_str().unwrap().to_owned()
}

#[test]
fn same_name_projects_reject_a_mismatched_database_at_every_entry() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("one/demo");
    let b = dir.path().join("two/demo");
    for root in [&a, &b] {
        fs::create_dir_all(root).unwrap();
        let header = ok(loom(root).arg("--root").arg(root).arg("init"));
        let title = if root == &a {
            "甲方私有决定"
        } else {
            "乙方私有决定"
        };
        let resume = seed_through_session(root, title);
        assert!(resume.starts_with(header.trim_end()));
        assert!(resume.contains(title));
    }
    let bytes_a = fs::read(db(&a)).unwrap();
    let bytes_b = fs::read(db(&b)).unwrap();
    fs::create_dir(b.join("pages")).unwrap();
    for args in [
        vec!["stats"],
        vec!["index"],
        vec!["explain", "src/entry.ts"],
        vec!["impact", "src/entry.ts"],
        vec!["recall", "私有决定"],
        vec!["serve"],
        vec!["import", "pages"],
        vec!["import", "pages", "--apply"],
        vec!["init", "--adopt-legacy"],
    ] {
        let out = refused(
            loom(&b)
                .arg("--root")
                .arg(&b)
                .arg("--db")
                .arg(db(&a))
                .args(args),
            "项目身份不符",
        );
        assert!(out.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&out.stderr).contains("甲方私有决定"));
    }
    assert_eq!(fs::read(db(&a)).unwrap(), bytes_a);
    assert_eq!(fs::read(db(&b)).unwrap(), bytes_b);
    assert!(ok(loom(&a).args(["recall", "私有决定"])).contains("甲方私有决定"));
    assert!(!ok(loom(&b).args(["recall", "私有决定"])).contains("甲方私有决定"));
}
