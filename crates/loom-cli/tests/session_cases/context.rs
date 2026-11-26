use super::*;

#[test]
fn context_wire_bytes_and_json_match_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    project(root, "不可访问网络");
    fs::write(root.join("target.ts"), "").unwrap();
    for n in 0..14 {
        fs::write(
            root.join(format!("entry-{n:02}.ts")),
            "import './target.ts';",
        )
        .unwrap();
    }
    fs::write(root.join("lazy.ts"), "import('./target');").unwrap();
    cli(root, &["index"]);
    let (mut s, _) = connect(root, None);
    let tools = s.request("tools/list", json!({}));
    let tool = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "context")
        .unwrap();
    assert_eq!(tool["annotations"]["readOnlyHint"], true);
    assert!(
        tool["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("mode"))
    );
    let args = [
        "context",
        "target.ts",
        "--mode",
        "impact",
        "--budget",
        "1200",
    ];
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_loom"))
            .args(args)
            .current_dir(root)
            .output()
            .unwrap()
    };
    let out = run(&args);
    let text = s.call(
        "context",
        json!({"target":"target.ts","mode":"impact", "budget":1200}),
    );
    assert_eq!(out.stdout, text.as_bytes());
    let mut full = args.to_vec();
    full.push("--json");
    let r: Value = serde_json::from_slice(&run(&full).stdout).unwrap();
    assert_eq!(r["text"], text);
    assert_eq!(r["bytes"], text.len());
    let standalone: Value =
        serde_json::from_slice(&run(&["impact", "target.ts", "--json"]).stdout).unwrap();
    assert_eq!(r["structure"], standalone);
    assert!(text.contains(&format!(
        "快照第 {} 代",
        standalone["snapshot"]["generation"]
    )));
    let shown = text
        .lines()
        .filter(|l| l.contains(" · 静态确定 · "))
        .count();
    let missing = 14 - shown;
    assert!(shown > 0 && missing > 0 && text.contains(" · 可能 · "));
    assert_eq!(r["omitted"]["budget"]["exact"], missing);
    assert!(text.contains(&format!("确定关系 {missing} 条")));
    let bad = run(&["context", "target.ts", "--mode", "impact", "--budget", "1"]);
    assert!(!bad.status.success() && bad.stdout.is_empty());
    let err = s.request(
        "tools/call",
        json!({"name":"context","arguments":{"target":"target.ts","mode":"impact","budget":1}}),
    );
    assert_eq!(err["isError"], true);
    assert!(
        String::from_utf8(bad.stderr)
            .unwrap()
            .contains(err["content"][0]["text"].as_str().unwrap())
    );
    s.process.finish();
}

#[test]
fn context_leaves_opening_overview_and_resume_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let id = project(root, "离线红线");
    let heading = cli(root, &["stats"]).lines().next().unwrap().to_owned();
    let baseline = format!("{heading}\n目标：尚未确认。\n红线 [{id}] 离线红线");
    let resume = format!(
        "{heading}\n未完成任务：无。\n记忆标题（最近 24 小时；锚点复核仅查最近 200 条有效记忆）：\n此窗口内无近期标题或需复核项。\n"
    );
    for host in [None, Some("utf8-byte-upper-bound")] {
        let (mut s, opening) = connect(root, host);
        s.call("context", json!({"target":"target.ts","mode":"explain"}));
        let overview = s.call("overview", json!({}));
        if host.is_some() {
            assert_eq!(opening, format!("{baseline}\n{overview}"));
            assert_eq!(overview, format!("快照：{}", snap(&opening)));
        } else {
            assert!(opening.is_empty());
            assert_eq!(overview, format!("{baseline}\n快照：{}", snap(&overview)));
        }
        assert_eq!(s.call("resume", json!({})), resume);
        assert_eq!(cli(root, &["overview"]), baseline);
        s.process.finish();
    }
}
