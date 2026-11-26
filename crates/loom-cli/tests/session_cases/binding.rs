use super::*;

const GUIDE: &str =
    "这个项目还没有接入 Loom。运行 `loom guide`，按其中的步骤由你完成接入；完成后再调一次 resume。";
const READY: &str = "其余工具已就绪。宿主没有自动刷新工具列表时，请重连 Loom 服务或新开会话；在那之前可以用同名的命令行。";
const REPAIR: &str = "按上面的提示处理后再调一次 resume；不要删除或新建 .loom 里的任何东西";
const MISSING_DB: &str = "项目库未初始化：";

fn tools(s: &mut Session) -> Vec<Value> {
    s.request("tools/list", json!({}))["tools"]
        .as_array()
        .unwrap()
        .clone()
}

fn tool_error(s: &mut Session, name: &str) -> String {
    let r = s.request("tools/call", json!({"name":name,"arguments":{}}));
    assert_eq!(r["isError"], true, "{r}");
    r["content"][0]["text"].as_str().unwrap().to_owned()
}

#[test]
fn unbound_handshake_lists_only_resume_and_never_creates_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (args, cause) in [
        (
            &["serve"][..],
            "未找到已绑定项目；请明确指定 --root，首次使用需执行 init",
        ),
        (&["--root", ".", "serve"][..], MISSING_DB),
    ] {
        let mut s = Session::with_args(root, args);
        let info = initialize(&mut s, Some("utf8-byte-upper-bound"));
        assert!(info["instructions"].is_null());
        assert_eq!(info["capabilities"]["tools"]["listChanged"], true);
        let listed = tools(&mut s);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["name"], "resume");
        for name in ["resume", "index", "not-a-tool"] {
            // 显式给根时原因里带着期望的库位置；末行是这个服务自己的程序位置，供找不到命令时用。
            let reply = tool_error(&mut s, name);
            let exe = format!("loom{}", std::env::consts::EXE_SUFFIX);
            assert!(
                reply.starts_with(cause)
                    && reply.contains(&format!("\n{GUIDE}\n"))
                    && reply.lines().last().unwrap().contains("完整路径：")
                    && reply.ends_with(&exe),
                "{reply}"
            );
        }
        assert!(s.notifications.is_empty());
        s.process.finish();
        assert_eq!(fs::read_dir(root).unwrap().count(), 0);
    }
}

#[test]
fn live_binding_upgrades_once_and_reserves_resume_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (mut s, opening) = connect(root, Some("utf8-byte-upper-bound"));
    assert!(opening.is_empty());
    let limited = tools(&mut s);
    fs::write(root.join("entry.ts"), "import './leaf';").unwrap();
    fs::write(root.join("leaf.ts"), "export const leaf = 1;").unwrap();
    let heading = cli(root, &["--root", ".", "init"]);
    cli(root, &["index"]);
    for topic in ["网络", "磁盘", "内存", "时钟"] {
        let title = format!("记录：{}", topic.repeat(55));
        cli(
            root,
            &[
                "remember",
                "DECISION",
                &title,
                &format!("保留{topic}的验证依据"),
            ],
        );
    }

    let first = s.call("resume", json!({}));
    assert!(first.starts_with(&heading) && first.len() <= 1500);
    assert_eq!(first.lines().last(), Some(READY));
    assert!(first.contains("省略"));
    let all = tools(&mut s);
    assert_eq!(all.len(), 17);
    assert_eq!(
        limited,
        vec![all.iter().find(|t| t["name"] == "resume").unwrap().clone()]
    );
    let next = s.call("resume", json!({}));
    assert!(next.starts_with(&heading) && !next.contains(READY));
    assert!(s.call("recall", json!({})).contains("记录"));
    assert!(
        s.call("context", json!({"target":"entry.ts","mode":"explain"}))
            .contains("leaf.ts")
    );
    let overview = s.call("overview", json!({}));
    assert!(overview.contains(&heading));
    assert_eq!(
        s.call("overview", json!({})),
        format!("快照：{}", snap(&overview))
    );
    assert_eq!(s.notifications.len(), 1);
    s.process.finish();
}

#[test]
fn idle_store_is_exclusive_and_recovers_without_retracting_tools() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let heading = cli(root, &["--root", ".", "init"]);
    let (mut s, _) = connect(root, None);
    let all = tools(&mut s);
    assert_eq!(all.len(), 17);
    let normal = s.call("resume", json!({}));
    assert!(normal.starts_with(&heading) && !normal.contains(READY));
    let db = root.join(".loom/loom.db");
    let moved = root.join("moved.db");
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let _exclusive = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&db)
            .unwrap();
    }
    fs::rename(&db, &moved).unwrap();
    // 已绑定的项目丢了库：报库不在原处并给出重新关联的写法，不引导去建库。
    let lost = tool_error(&mut s, "recall");
    assert!(
        lost.starts_with("项目库不在 ")
            && lost.contains("init --db")
            && lost.ends_with(&format!("\n{REPAIR}")),
        "{lost}"
    );
    assert_eq!(tools(&mut s), all);
    assert!(!db.exists());
    fs::rename(moved, &db).unwrap();
    assert_eq!(s.call("resume", json!({})), normal);
    assert_eq!(tools(&mut s), all);

    // 即使原址换成另一套自洽的标记和库，也不能把另一项目交给旧会话。
    let meta = root.join(".loom");
    let saved = root.join("saved");
    fs::rename(&meta, &saved).unwrap();
    let other = cli(root, &["--root", ".", "init"]);
    let error = tool_error(&mut s, "resume");
    assert!(error.contains("项目身份不符") && !error.contains(&other) && error.ends_with(REPAIR));
    assert_eq!(tools(&mut s), all);
    fs::rename(&meta, root.join("other")).unwrap();
    fs::rename(saved, meta).unwrap();
    assert_eq!(s.call("resume", json!({})), normal);
    assert!(s.notifications.is_empty());
    s.process.finish();
}

#[test]
fn correcting_an_external_db_allows_the_first_hidden_tool_call() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    for (root, db) in [("ours", "correct.db"), ("theirs", "external.db")] {
        fs::create_dir(cwd.join(root)).unwrap();
        cli(cwd, &["--root", root, "--db", db, "init"]);
        cli(
            cwd,
            &[
                "--root",
                root,
                "--db",
                db,
                "remember",
                "DECISION",
                root,
                "所选项目的私有依据",
            ],
        );
    }
    let external = cwd.join("external.db");
    let before = fs::read(&external).unwrap();
    let mut s = Session::with_args(cwd, &["--root", "ours", "--db", "external.db", "serve"]);
    assert!(initialize(&mut s, Some("utf8-byte-upper-bound"))["instructions"].is_null());
    assert_eq!(tools(&mut s).len(), 1);
    let error = tool_error(&mut s, "recall");
    assert!(error.contains("项目身份不符") && error.ends_with(REPAIR) && !error.contains("theirs"));
    assert_eq!(fs::read(&external).unwrap(), before);
    fs::rename(&external, cwd.join("foreign.db")).unwrap();
    fs::rename(cwd.join("correct.db"), external).unwrap();
    let recall = s.call("recall", json!({}));
    assert!(recall.contains("ours") && !recall.contains("theirs"));
    assert!(!s.call("resume", json!({})).contains(READY));
    assert_eq!(tools(&mut s).len(), 17);
    assert_eq!(s.notifications.len(), 1);
    s.process.finish();
}

#[test]
fn concurrent_resumes_or_list_first_promote_exactly_once() {
    for list_first in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (mut s, _) = connect(root, None);
        let heading = cli(root, &["--root", ".", "init"]);
        if list_first {
            assert_eq!(tools(&mut s).len(), 17);
        }
        let start = s.id + 1;
        for id in start..=start + 1 {
            s.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"resume","arguments":{}}}));
        }
        s.id += 2;
        let mut replies = [s.next_reply(), s.next_reply()];
        replies.sort_by_key(|r| r["id"].as_u64().unwrap());
        let mut hints = 0;
        for (i, r) in replies.iter().enumerate() {
            assert_eq!(r["id"], start + i as u32);
            let text = r["result"]["content"][0]["text"].as_str().unwrap();
            assert!(text.starts_with(&heading));
            hints += text.lines().filter(|&line| line == READY).count();
        }
        assert_eq!(hints, usize::from(!list_first));
        assert_eq!(tools(&mut s).len(), 17);
        assert_eq!(s.notifications.len(), 1);
        s.process.finish();
    }
}
