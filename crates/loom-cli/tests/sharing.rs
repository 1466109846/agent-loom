//! 分享的公开边界是两个真实 CLI 进程之间的一份文件。只绑定本测试新建的临时根。
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use loom_core::{project::ProjectLocation, util::sha256_hex};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

struct Project(TempDir);

impl Project {
    fn new() -> Self {
        let p = Self(tempdir().unwrap());
        p.ok(&["init"]);
        p
    }
    fn root(&self) -> &Path {
        self.0.path()
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_loom"))
            .current_dir(self.root())
            .arg("--root")
            .arg(self.root())
            .args(args)
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
    fn rejects(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "意外成功：{args:?}");
        assert!(out.stdout.is_empty(), "失败时不得输出半份预览");
        String::from_utf8(out.stderr).unwrap()
    }
    fn memory(&self, title: &str, body: &str) -> String {
        self.ok(&["remember", "GOTCHA", title, body]).trim().into()
    }
    fn goal(&self) -> String {
        let id = self
            .ok(&[
                "intent",
                "propose",
                "goal",
                "保留离线入口",
                "断开网络时仍能查询本项目。",
            ])
            .trim()
            .to_owned();
        self.ok(&["intent", "confirm", &id, "--yes"]);
        id
    }
    fn store(&self, writable: bool) -> loom_core::Store {
        ProjectLocation::select(self.root(), Some(self.root()), None)
            .unwrap()
            .open(writable)
            .unwrap()
    }
    fn db_bytes(&self) -> Vec<u8> {
        fs::read(self.root().join(".loom/loom.db")).unwrap()
    }
}

fn as_json(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}
fn sha(env: &Value) -> &str {
    env["sha256"].as_str().unwrap()
}
// v1 的规范形式：键按 ASCII 顺序，UTF-8 紧凑 JSON；不包含外层摘要和换行。
fn digest(payload: &Value) -> String {
    let mut payload = payload.clone();
    payload.sort_all_objects();
    sha256_hex(&serde_json::to_vec(&payload).unwrap())
}
fn rehash(env: &mut Value) {
    env["sha256"] = digest(&env["payload"]).into();
}
fn snap(root: &Project, id: &str) -> Value {
    as_json(&root.ok(&["share", "preview", "--memory", id]))
}
fn apply(p: &Project, file: &str, env: &Value) -> Value {
    as_json(&p.ok(&["share", "import", file, "--reviewed-sha", sha(env), "--yes"]))
}

#[test]
fn exact_reviewed_payload_is_deterministic_and_does_not_follow_edges() {
    let p = Project::new();
    let goal = p.goal();
    let memory = p
        .ok(&[
            "remember",
            "BUGFIX",
            "重试之前要退避",
            "必须退避，否则重试会放大负载。",
            "--anchor",
            "never-disclose.rs#private_symbol",
        ])
        .trim()
        .to_owned();
    let private = p.memory("旁证不在选择内", "这是另一条记录，不应因为关系而分享。");
    p.ok(&["evidence", "link", &memory, &private, "supports", "--yes"]);
    let before = p.db_bytes();
    let preview = p.ok(&["share", "preview", "--intent", &goal, "--memory", &memory]);
    let again = p.ok(&["share", "preview", "--memory", &memory, "--intent", &goal]);
    assert_eq!(preview, again);
    let env = as_json(&preview);
    assert_eq!(sha(&env), digest(&env["payload"]));
    let origin = fs::read_to_string(p.root().join(".loom/project.id")).unwrap();
    let records = env["payload"]["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .all(|r| r["origin_project_id"] == origin.trim())
    );
    assert!(
        records
            .iter()
            .any(|r| r["origin_record_id"] == goal && r["record_type"] == "intent")
    );
    assert!(
        records
            .iter()
            .any(|r| r["origin_record_id"] == memory
                && r["body"] == "必须退避，否则重试会放大负载。")
    );
    for hidden in [
        &private[..],
        "旁证不在选择内",
        "private_symbol",
        "never-disclose",
        "anchors",
        "edges",
        "tags",
        "confirmed",
        p.root().file_name().unwrap().to_str().unwrap(),
    ] {
        assert!(!preview.contains(hidden), "泄露：{hidden}");
    }
    p.ok(&[
        "share",
        "export",
        "snapshot",
        "--memory",
        &memory,
        "--intent",
        &goal,
        "--reviewed-sha",
        sha(&env),
        "--yes",
    ]);
    assert_eq!(
        fs::read_to_string(p.root().join("snapshot")).unwrap(),
        preview
    );
    assert_eq!(before, p.db_bytes());
}

#[test]
fn import_is_read_only_until_review_then_proposes_and_preserves_origin_idempotently() {
    let (a, b) = (Project::new(), Project::new());
    let g = a.goal();
    let m = a.memory("写入顺序", "先写内容再更新游标，否则读者会丢记录。");
    let env = as_json(&a.ok(&["share", "preview", "--intent", &g, "--memory", &m]));
    let file = b.root().join("received.json");
    let path = file.to_str().unwrap();
    a.ok(&[
        "share",
        "export",
        path,
        "--memory",
        &m,
        "--intent",
        &g,
        "--yes",
        "--reviewed-sha",
        sha(&env),
    ]);
    let before = b.db_bytes();
    let side_files = || {
        ["-wal", "-shm", "-journal"]
            .map(|s| fs::read(b.root().join(format!(".loom/loom.db{s}"))).ok())
    };
    let side_before = side_files();
    let plan = as_json(&b.ok(&["share", "import", path]));
    assert!(
        side_files() == side_before,
        "只读预演不能创建或改写 SQLite 旁文件"
    );
    assert_eq!(plan["sha256"], env["sha256"]);
    assert_eq!(plan["payload"], env["payload"]);
    assert!(
        plan["changes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["status"] == "new")
    );
    b.rejects(&["share", "import", path, "--yes"]);
    b.rejects(&["share", "import", path, "--reviewed-sha", sha(&env)]);
    b.rejects(&[
        "share",
        "import",
        path,
        "--yes",
        "--reviewed-sha",
        &"0".repeat(64),
    ]);
    assert_eq!(before, b.db_bytes());
    let receipt = apply(&b, path, &env);
    let changes = receipt["changes"].as_array().unwrap();
    let local = |id: &str| {
        changes
            .iter()
            .find(|r| r["origin_record_id"] == id)
            .unwrap()["local_id"]
            .as_str()
            .unwrap()
    };
    assert_ne!(local(&g), g);
    assert_ne!(local(&m), m);
    assert!(b.ok(&["intent", "get", local(&g)]).contains("待确认"));
    assert!(!b.ok(&["overview"]).contains("保留离线入口"));
    let accepted = b.store(false).get(local(&m)).unwrap().unwrap();
    assert!(accepted.origin.contains(&m));
    assert!(
        accepted.origin.contains(
            env["payload"]["records"][0]["origin_project_id"]
                .as_str()
                .unwrap()
        )
    );
    assert_eq!(accepted.status, "active");
    assert!(b.store(false).get(&m).unwrap().is_none());
    assert!(b.ok(&["recall", "游标"]).contains("写入顺序"));
    assert_eq!(
        snap(&b, local(&m)),
        snap(&a, &m),
        "再次点选不能把外来经验冒充为本地原创"
    );
    let committed = b.db_bytes();
    let again = apply(&b, path, &env);
    assert!(
        again["changes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["status"] == "duplicate")
    );
    assert_eq!(committed, b.db_bytes());
}

#[test]
fn an_uncheckpointed_archive_cannot_be_exported_from_the_old_main_file() {
    let p = Project::new();
    let id = p.memory("读取前检验", "必须先检查内容，以免在恢复后使用旧资料。");
    let env = snap(&p, &id);
    let writer = p.store(true);
    writer
        .set_status(std::slice::from_ref(&id), "archived")
        .unwrap();
    assert!(
        fs::metadata(p.root().join(".loom/loom.db-wal"))
            .unwrap()
            .len()
            > 0
    );
    p.rejects(&[
        "share",
        "export",
        "unsafe.json",
        "--memory",
        &id,
        "--reviewed-sha",
        sha(&env),
        "--yes",
    ]);
    assert!(!p.root().join("unsafe.json").exists());
    drop(writer);
    p.rejects(&["share", "preview", "--memory", &id]);
}

#[test]
fn explicit_root_beats_cwd_and_external_db_cannot_choose_its_own_root() {
    let (a, b) = (Project::new(), Project::new());
    let g = a.goal();
    let expected = a.ok(&["share", "preview", "--intent", &g]);
    let cli = |cwd: &Path, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_loom"))
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap()
    };
    let r = cli(
        b.root(),
        &[
            "share",
            "preview",
            "--root",
            a.root().to_str().unwrap(),
            "--intent",
            &g,
        ],
    );
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(r.stdout, expected.as_bytes());
    let child = a.root().join("child");
    fs::create_dir(&child).unwrap();
    let r = cli(&child, &["share", "preview", "--intent", &g]);
    assert!(r.status.success());
    assert_eq!(r.stdout, expected.as_bytes());
    let external = a.root().join(".loom/loom.db");
    let err = b.rejects(&[
        "--db",
        external.to_str().unwrap(),
        "share",
        "preview",
        "--intent",
        &g,
    ]);
    assert!(err.contains("身份") && !err.contains("保留离线入口"));
}

#[test]
fn current_selection_and_reviewed_sha_are_both_required_at_export() {
    let (p, other) = (Project::new(), Project::new());
    let goal = p.goal();
    let m = p.memory("检查顺序", "先检查输入再登记结果，避免损坏统计。");
    let reviewed = snap(&p, &m);
    p.rejects(&["share", "preview"]);
    p.rejects(&[
        "share",
        "export",
        "out",
        "--yes",
        "--reviewed-sha",
        sha(&reviewed),
    ]);
    p.rejects(&["share", "export", "out", "--memory", &m, "--yes"]);
    p.rejects(&[
        "share",
        "export",
        "out",
        "--memory",
        &m,
        "--reviewed-sha",
        sha(&reviewed),
    ]);
    p.rejects(&[
        "share",
        "export",
        "out",
        "--intent",
        &goal,
        "--reviewed-sha",
        sha(&reviewed),
        "--yes",
    ]);
    let proposed = p.ok(&[
        "intent",
        "propose",
        "constraint",
        "待审事项",
        "尚未得到人工确认。",
    ]);
    p.rejects(&[
        "share",
        "preview",
        "--intent",
        proposed.trim(),
        "--memory",
        &m,
    ]);
    p.ok(&["intent", "revoke", &goal, "--yes"]);
    p.rejects(&["share", "preview", "--intent", &goal]);
    let foreign = other.goal();
    p.rejects(&["share", "preview", "--intent", &foreign]);
    p.store(true)
        .set_status(std::slice::from_ref(&m), "archived")
        .unwrap();
    p.rejects(&[
        "share",
        "export",
        "out",
        "--memory",
        &m,
        "--reviewed-sha",
        sha(&reviewed),
        "--yes",
    ]);
    assert!(!p.root().join("out").exists());
}

#[test]
fn incoming_conflict_even_after_a_new_record_rolls_back_everything() {
    let (a, b) = (Project::new(), Project::new());
    let new = a.goal();
    let m = a.memory("先提交再通知", "必须等事务提交才发通知，避免读到旧值。");
    let first = snap(&a, &m);
    let path = b.root().join("in.json");
    let file = path.to_str().unwrap();
    fs::write(&path, first.to_string()).unwrap();
    apply(&b, file, &first);
    let before = b.db_bytes();
    let mut changed = as_json(&a.ok(&["share", "preview", "--memory", &m, "--intent", &new]));
    let rows = changed["payload"]["records"].as_array_mut().unwrap();
    let row = rows
        .iter_mut()
        .find(|r| r["origin_record_id"] == m)
        .unwrap();
    row["body"] = json!("另一个结论，即使摘要正确也不能覆盖旧记录。");
    rehash(&mut changed);
    fs::write(&path, changed.to_string()).unwrap();
    let plan = as_json(&b.ok(&["share", "import", file]));
    let changes = plan["changes"].as_array().unwrap();
    assert!(
        changes
            .iter()
            .any(|r| r["status"] == "conflict" && r["origin_record_id"] == m)
    );
    assert!(
        changes
            .iter()
            .any(|r| r["status"] == "new" && r["origin_record_id"] == new)
    );
    let reason = b.rejects(&[
        "share",
        "import",
        file,
        "--reviewed-sha",
        sha(&changed),
        "--yes",
    ]);
    assert!(reason.contains("冲突"));
    assert_eq!(before, b.db_bytes());
}

#[test]
fn rejects_private_text_strict_schema_limits_and_changed_file() {
    let (p, receiver) = (Project::new(), Project::new());
    let m = p.memory("检验输入", "必须先检验内容才使用，避免错误扩散。");
    let env = snap(&p, &m);
    let path = receiver.root().join("input");
    let f = path.to_str().unwrap();
    let before = receiver.db_bytes();
    for text in [
        r"本机位置 C:\Users\Local\private.txt 不外传",
        "缓存位于 /home/private/cache，必须清理",
        "连接 http://127.0.0.1:8123 才能工作",
        "用户偏好：命令完成后不要提醒我。",
        "设置 password=fixture-only 再运行测试。",
    ] {
        let mut e = env.clone();
        e["payload"]["records"][0]["body"] = json!(text);
        rehash(&mut e);
        fs::write(&path, e.to_string()).unwrap();
        let err = receiver.rejects(&["share", "import", f, "--yes", "--reviewed-sha", sha(&e)]);
        assert!(!err.contains(text));
    }
    let unsafe_id = p.memory("工作站目录", r"资料暂存在 D:\private\test 中，不应分享。");
    p.rejects(&["share", "preview", "--memory", &unsafe_id]);
    for (level, field, value) in [
        (0, "signature", json!("untrusted")),
        (1, "anchors", json!([])),
        (2, "private", json!(true)),
        (2, "kind", json!("USER_PREFERENCE")),
        (2, "body", json!("字".repeat(3000))),
        (2, "body", json!("短")),
        (1, "format", json!("future")),
        (2, "origin_record_id", json!("../../other")),
    ] {
        let mut e = env.clone();
        let at = match level {
            0 => &mut e,
            1 => &mut e["payload"],
            _ => &mut e["payload"]["records"][0],
        };
        at[field] = value;
        rehash(&mut e);
        fs::write(&path, e.to_string()).unwrap();
        receiver.rejects(&["share", "import", f]);
    }
    let mut changed = env.clone();
    changed["payload"]["records"][0]["body"] = json!("这段正文没有被先前的摘要承诺过。");
    fs::write(&path, changed.to_string()).unwrap();
    receiver.rejects(&["share", "import", f]);
    rehash(&mut changed);
    fs::write(&path, changed.to_string()).unwrap();
    receiver.rejects(&["share", "import", f, "--reviewed-sha", sha(&env), "--yes"]);
    for count in [0, 2, 65] {
        let mut e = env.clone();
        let mut rows = vec![e["payload"]["records"][0].clone(); count];
        if count == 65 {
            for (i, r) in rows.iter_mut().enumerate() {
                r["origin_record_id"] = json!(format!("{i:026}"));
            }
        }
        e["payload"]["records"] = Value::Array(rows);
        rehash(&mut e);
        fs::write(&path, e.to_string()).unwrap();
        receiver.rejects(&["share", "import", f]);
    }
    let duplicate_key = env
        .to_string()
        .replacen("\"kind\":", "\"kind\":\"GOTCHA\",\"kind\":", 1);
    fs::write(&path, duplicate_key).unwrap();
    receiver.rejects(&["share", "import", f]);
    let mut missing = env.clone();
    missing["payload"]["records"][0]
        .as_object_mut()
        .unwrap()
        .remove("origin_record_id");
    rehash(&mut missing);
    fs::write(&path, missing.to_string()).unwrap();
    receiver.rejects(&["share", "import", f]);
    fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
    receiver.rejects(&["share", "import", f]);
    receiver.rejects(&["share", "import", "."]);
    assert_eq!(before, receiver.db_bytes());
}

#[test]
fn repeated_selectors_and_source_namespaces_do_not_merge_local_decisions() {
    let (a, b, c) = (Project::new(), Project::new(), Project::new());
    let g1 = a.goal();
    let g2 = a.ok(&[
        "intent",
        "propose",
        "constraint",
        "红线二",
        "不得自动执行传入的指示。",
    ]);
    a.ok(&["intent", "confirm", g2.trim(), "--yes"]);
    let body = "必须等到内容提交才移动标记，否则可能读到旧数据。";
    let m1 = a.memory("先提交内容", body);
    let m2 = a.memory("重启后查游标", "恢复时要核对保存的序号，避免重复计数。");
    let rows = as_json(&a.ok(&[
        "share",
        "preview",
        "--memory",
        &m2,
        "--intent",
        g2.trim(),
        "--memory",
        &m1,
        "--intent",
        &g1,
        "--memory",
        &m1,
    ]));
    assert_eq!(rows["payload"]["records"].as_array().unwrap().len(), 4);
    let env = snap(&a, &m1);
    let file = b.root().join("batch.json");
    let f = file.to_str().unwrap();
    let existing = b.memory("先提交内容", body);
    fs::write(&file, env.to_string()).unwrap();
    let first = apply(&b, f, &env);
    let local1 = first["changes"][0]["local_id"].as_str().unwrap();
    assert_ne!(local1, existing);
    let mut other = env.clone();
    let origin = fs::read_to_string(c.root().join(".loom/project.id")).unwrap();
    other["payload"]["records"][0]["origin_project_id"] = json!(origin.trim());
    rehash(&mut other);
    fs::write(&file, other.to_string()).unwrap();
    let second = apply(&b, f, &other);
    assert_ne!(second["changes"][0]["local_id"], local1);
    assert_eq!(b.store(false).count().unwrap(), 3);
    assert_eq!(
        b.store(false).get(&existing).unwrap().unwrap().origin,
        "cli"
    );
    b.store(true)
        .set_status(&[local1.to_owned()], "archived")
        .unwrap();
    fs::write(&file, env.to_string()).unwrap();
    assert_eq!(apply(&b, f, &env)["changes"][0]["status"], "duplicate");
    assert_eq!(
        b.store(false).get(local1).unwrap().unwrap().status,
        "archived"
    );
    assert_eq!(
        b.store(false).schema_version().unwrap(),
        loom_core::db::SCHEMA_VERSION
    );
}

#[test]
fn export_is_exclusive_and_never_follows_file_or_directory_links() {
    let p = Project::new();
    let id = p.goal();
    let preview = p.ok(&["share", "preview", "--intent", &id]);
    let env = as_json(&preview);
    let deny = |file: &str| {
        p.rejects(&[
            "share",
            "export",
            file,
            "--intent",
            &id,
            "--yes",
            "--reviewed-sha",
            sha(&env),
        ])
    };
    let source = p.root().join("lib.rs");
    fs::write(&source, b"// keep this source").unwrap();
    let before = p.db_bytes();
    deny("lib.rs");
    deny(".loom/loom.db");
    deny(".loom/new.json");
    fs::hard_link(&source, p.root().join("alias.json")).unwrap();
    deny("alias.json");
    assert_eq!(fs::read(&source).unwrap(), b"// keep this source");
    let snapshot = p.root().join("snapshot.json");
    fs::write(&snapshot, preview).unwrap();
    fs::hard_link(&snapshot, p.root().join("linked.json")).unwrap();
    p.rejects(&["share", "import", "linked.json"]);
    let other = tempdir().unwrap();
    let link = p.root().join("via-link");
    #[cfg(windows)]
    {
        let out = Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:TEST_LINK -Target $env:TEST_DEST | Out-Null"]).env("TEST_LINK", &link).env("TEST_DEST", other.path()).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        deny("snapshot.json:alternate");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(other.path(), &link).unwrap();
    deny("via-link/new.json");
    assert!(!other.path().join("new.json").exists());
    fs::write(other.path().join("in.json"), env.to_string()).unwrap();
    p.rejects(&["share", "import", "via-link/in.json"]);
    assert_eq!(before, p.db_bytes());
}
