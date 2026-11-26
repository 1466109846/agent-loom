//! 真实 CLI 和同一个长寿命 stdio 会话。模型只在核心单测中由临时环回服务模拟。
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

fn loom(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom"));
    cmd.current_dir(root)
        .env_remove("LOOM_EMBED_MODEL")
        .env_remove("LOOM_EXTRACT_MODEL");
    cmd
}
fn cli(root: &Path, args: &[&str]) -> Output {
    loom(root).args(args).output().unwrap()
}
#[track_caller]
fn ok(root: &Path, args: &[&str]) -> String {
    let out = cli(root, args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
struct Session {
    child: Child,
    lines: mpsc::Receiver<String>,
    id: u32,
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Session {
    fn new(root: &Path) -> Self {
        let mut child = loom(root)
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut session = Self {
            child,
            lines,
            id: 0,
        };
        let hello = session.rpc("initialize", json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"semantic-test","version":"1"}}));
        assert!(hello["result"].is_object());
        session.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        session
    }
    fn send(&mut self, msg: Value) {
        let input = self.child.stdin.as_mut().unwrap();
        writeln!(input, "{msg}").unwrap();
        input.flush().unwrap();
    }
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        self.send(json!({"jsonrpc":"2.0", "id":self.id,"method":method,"params":params}));
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(10))
            .expect("stdio 会话超时");
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], self.id);
        reply
    }
    fn find(&mut self) -> String {
        let reply = self.rpc(
            "tools/call",
            json!({"name":"semantic","arguments":{"query":"备份"}}),
        );
        assert!(reply.get("error").is_none() && reply["result"]["isError"] != true);
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .into()
    }
}

#[test]
fn only_confirmed_local_cli_changes_the_live_session_scope() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    ok(root, &["--root", ".", "init"]);
    let saved = ok(
        root,
        &[
            "remember",
            "decision",
            "备份保留期限",
            "因为现场维护需要恢复故障之前的版本，保留历史副本。",
        ],
    );
    let id = saved.trim();
    assert_eq!(id.len(), 26);
    assert!(ok(root, &["semantic", "list"]).contains("无授权"));
    assert!(!cli(root, &["semantic", "allow", id]).status.success());
    let mut session = Session::new(root);
    let list = session.rpc("tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let named: Vec<_> = tools
        .iter()
        .filter(|t| t["name"].as_str().unwrap().starts_with("semantic"))
        .collect();
    assert_eq!(named.len(), 1);
    assert_eq!(named[0]["name"], "semantic");
    assert_eq!(named[0]["annotations"]["readOnlyHint"], true);
    assert_eq!(
        named[0]["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    for op in ["allow", "deny"] {
        let invalid = session.rpc("tools/call", json!({"name":"semantic", "arguments":{"query":"备份", "operation":op,"id":id,"yes":true}}));
        assert!(invalid.get("error").is_some() || invalid["result"]["isError"] == true);
    }
    assert!(session.find().contains("无授权"));
    ok(root, &["semantic", "allow", id, "--yes"]);
    assert!(ok(root, &["semantic", "list"]).contains(id));
    let fallback = ok(root, &["semantic", "find", "备份"]);
    assert!(fallback.contains("关键词回退") && fallback.contains(id));
    assert_eq!(session.find(), fallback);
    assert!(!cli(root, &["semantic", "deny", id]).status.success());
    assert!(session.find().contains(id));
    ok(root, &["semantic", "deny", id, "--yes"]);
    let after = session.find();
    assert!(after.contains("无授权") && !after.contains(id));
}
