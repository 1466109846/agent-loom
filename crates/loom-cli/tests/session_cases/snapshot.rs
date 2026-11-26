use super::*;

#[path = "binding.rs"]
mod binding;
#[path = "context.rs"]
mod context;

fn cli(root: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_loom"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}
fn connect(root: &Path, bound: Option<&str>) -> (Session, String) {
    let mut s = Session::start(root);
    let info = initialize(&mut s, bound);
    (s, info["instructions"].as_str().unwrap_or("").to_owned())
}
fn initialize(s: &mut Session, bound: Option<&str>) -> Value {
    let caps = match bound {
        None => json!({}),
        Some(value) => json!({"experimental":{"loom/overview":{"textBudget":value}}}),
    };
    let info=s.request("initialize", json!({"protocolVersion":"2025-06-18","capabilities":caps,"clientInfo":{"name":"loom-e2e","version":"1"}}));
    s.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    info
}
fn snap(text: &str) -> &str {
    text.lines()
        .find_map(|line| line.strip_prefix("快照："))
        .expect(text)
}
fn project(root: &Path, title: &str) -> String {
    cli(root, &["--root", ".", "init"]);
    let id = cli(
        root,
        &[
            "intent",
            "propose",
            "constraint",
            title,
            "不因实现便利而破坏这项约束。",
        ],
    );
    cli(root, &["intent", "confirm", &id, "--yes"]);
    id
}

#[test]
fn opt_in_initialize_and_refresh_do_not_reinject_or_elevate_proposals() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a/同名");
    let b = tmp.path().join("b/同名");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    let old = project(&a, "离线才能开工");
    project(&b, "另一个项目的私有约束");
    let (mut session, opening) = connect(&a, Some("utf8-byte-upper-bound"));
    assert!(
        opening.starts_with("项目：")
            && opening.contains("离线才能开工")
            && !opening.contains("私有约束")
            && opening.len() <= 800,
        "{opening}"
    );
    let old_snap = snap(&opening);
    let repeated = session.call("overview", json!({}));
    assert_eq!(snap(&repeated), old_snap);
    assert_eq!(repeated.lines().count(), 1);
    let new=session.call("intent",json!({"action":"propose","kind":"constraint","title":"仅允许本地存储","body":"不能使用外部存储。"}));
    assert!(
        session
            .call("intent", json!({"action":"get","id":new}))
            .contains("本地存储")
    );
    assert!(!session.call("overview", json!({})).contains("本地存储"));
    cli(&a, &["intent", "replace", &old, new.trim(), "--yes"]);
    assert_eq!(snap(&session.call("overview", json!({}))), old_snap);
    let refreshed = session.call("refresh", json!({}));
    assert_ne!(snap(&refreshed), old_snap);
    assert!(
        refreshed.contains("仅允许本地存储")
            && !refreshed.contains("离线才能开工")
            && refreshed.len() <= 2000,
        "{refreshed}"
    );
    let (other, other_open) = connect(&b, Some("utf8-byte-upper-bound"));
    assert!(other_open.contains("私有约束") && !other_open.contains("仅允许本地存储"));
    assert_ne!(snap(&other_open), snap(&refreshed));
    other.process.finish();
    session.process.finish();
}

#[test]
fn unknown_host_gets_no_automatic_knowledge_and_long_rule_set_warns() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    project(root, "不可自动注入的约束");
    for i in 0..18 {
        let title = format!("第{i:02}条必须保留省略警告的不可省略约束");
        let id = cli(
            root,
            &[
                "intent",
                "propose",
                "constraint",
                &title,
                "需人工确认的测试约束。",
            ],
        );
        cli(root, &["intent", "confirm", &id, "--yes"]);
    }
    for meter in [None, Some("unknown")] {
        let (mut s, opening) = connect(root, meter);
        assert!(
            !opening.contains("约束") && !opening.contains("项目：") && !opening.contains("快照："),
            "{opening}"
        );
        let manual = s.call("overview", json!({}));
        assert!(manual.starts_with("项目：") && manual.len() <= 2000);
        assert_eq!(snap(&manual), snap(&s.call("overview", json!({}))));
        s.process.finish();
    }
    let (mut s, opening) = connect(root, Some("utf8-byte-upper-bound"));
    assert!(
        opening.len() <= 800 && opening.contains("还有约束未展示"),
        "{opening}"
    );
    let list = s.request("tools/list", json!({}));
    for name in [
        "overview", "refresh", "intent", "note", "canvas", "evidence", "rerank", "extract",
    ] {
        assert!(
            list["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["name"] == name),
            "缺少 {name}"
        );
    }
    s.process.finish();
}

#[test]
fn real_session_resumes_pending_task_and_rechecks_the_source() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    project(root, "仅在项目内记录");
    fs::write(root.join("entry.ts"), "export const entry = 1;\n").unwrap();
    let (mut s, _) = connect(root, None);
    s.call("index", json!({}));
    let empty = s.call("note", json!({"path":"entry.ts"}));
    let sha = empty
        .lines()
        .find_map(|v| v.strip_prefix("当前指纹："))
        .unwrap();
    assert!(s.call("note",json!({"action":"set","path":"entry.ts","expected_sha":sha,"role":"负责显示离线校验结果"})).contains("离线校验"));
    let task=s.call("canvas",json!({"action":"open","goal":"复查入口职责","next":"对照变更后的源码","evidence":"entry.ts:1"}));
    cli(
        root,
        &[
            "remember",
            "GOTCHA",
            "离线校验需要保留证据位置",
            "当入口变化时，应核对证据位置再接受旧摘要。",
        ],
    );
    s.process.finish();
    fs::write(root.join("entry.ts"), "export const entry = 2;\n").unwrap();
    let (mut s, _) = connect(root, None);
    let recovery = s.call("resume", json!({}));
    assert!(
        recovery.starts_with("项目：")
            && recovery.contains(task.trim())
            && recovery.contains("对照变更后的源码")
            && recovery.len() <= 1500,
        "{recovery}"
    );
    assert!(s.call("evidence", json!({"query":"校验"})).contains("校验"));
    assert!(
        s.call("note", json!({"path":"entry.ts"}))
            .contains("需复核")
    );
    s.call("canvas", json!({"action":"close","id":task.trim()}));
    assert!(!s.call("resume", json!({})).contains(task.trim()));
    s.process.finish();
}
