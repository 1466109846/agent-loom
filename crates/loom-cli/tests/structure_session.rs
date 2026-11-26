use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[path = "session_cases/hybrid.rs"]
mod hybrid;
#[path = "session_cases/snapshot.rs"]
mod snapshot;

const TIMEOUT: Duration = Duration::from_secs(10);

struct Process(Child);

impl Process {
    fn start(root: &Path, args: &[&str]) -> Self {
        Self(
            Command::new(env!("CARGO_BIN_EXE_loom"))
                .current_dir(root)
                .env("LOOM_CONFIG", root.join("fixture-config.toml"))
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        )
    }

    fn finish(mut self) {
        drop(self.0.stdin.take());
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "子进程退出失败：{status}");
                return;
            }
            assert!(Instant::now() < deadline, "关闭输入后子进程未按时退出");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Session {
    process: Process,
    lines: Receiver<String>,
    id: u32,
    notifications: Vec<Value>,
}

impl Session {
    fn start(root: &Path) -> Self {
        Self::with_args(root, &["serve"])
    }

    fn with_args(root: &Path, args: &[&str]) -> Self {
        let mut process = Process::start(root, args);
        let output = process.0.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            process,
            lines,
            id: 0,
            notifications: Vec::new(),
        }
    }

    fn send(&mut self, message: Value) {
        let input = self.process.0.stdin.as_mut().unwrap();
        writeln!(input, "{message}").unwrap();
        input.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        self.send(json!({"jsonrpc":"2.0", "id":self.id, "method":method, "params":params}));
        let reply = self.next_reply();
        assert_eq!(reply["id"], self.id, "{method}: {reply}");
        assert!(reply.get("error").is_none(), "{method}: {reply}");
        reply["result"].clone()
    }

    fn next_reply(&mut self) -> Value {
        loop {
            let line = self
                .lines
                .recv_timeout(TIMEOUT)
                .expect("会话响应超时或进程已退出");
            let reply: Value = serde_json::from_str(&line).unwrap();
            if reply.get("id").is_some() {
                return reply;
            }
            assert_eq!(reply["method"], "notifications/tools/list_changed");
            self.notifications.push(reply);
        }
    }

    fn call(&mut self, name: &str, args: Value) -> String {
        let result = self.request("tools/call", json!({"name":name, "arguments":args}));
        assert_ne!(result["isError"], true, "{name}: {result}");
        let block = &result["content"][0];
        assert_eq!(block["type"], "text", "{name}: {result}");
        let text = block["text"].as_str().unwrap();
        assert!(!text.trim().is_empty(), "{name} 应返回报告文本");
        text.to_owned()
    }
}

#[test]
fn cli_json_and_mcp_preview_agree_on_generation_changed_files_and_verdict() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    fs::write(root.join("leaf.ts"), "export const leaf=1;").unwrap();
    let mut long = String::new();
    for i in 0..26 {
        let name = format!("part-{i:02}.ts");
        fs::write(root.join(&name), "").unwrap();
        long.push_str(&format!("import './{name}';\n"));
    }
    fs::write(root.join("batch.ts"), long).unwrap();
    Process::start(root, &["--root", ".", "init"]).finish();
    let mut s = Session::start(root);
    s.request("initialize",json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"u1","version":"1"}}));
    s.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    s.call("index", json!({}));
    let report = |action: &str, p: &str| -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_loom"))
            .current_dir(root)
            .args([action, p, "--json"])
            .output()
            .unwrap();
        assert!(out.status.success());
        serde_json::from_slice(&out.stdout).unwrap()
    };
    fs::write(root.join("leaf.ts"), "export const leaf=2;").unwrap();
    let changed = report("impact", "leaf.ts");
    let generation = changed["snapshot"]["generation"].as_i64().unwrap();
    assert_eq!(
        changed["conclusion"],
        json!({"verdict":"gap","gaps":["changed"]})
    );
    assert_eq!(
        changed["verification"]["files"],
        json!([{"path":"leaf.ts","state":"changed"}])
    );
    let text = s.call("impact", json!({"target":"leaf.ts"}));
    for part in [
        &format!("第 {generation} 代"),
        "已变化：leaf.ts",
        "不能判断",
        "其余文件未核对",
    ] {
        assert!(text.contains(part), "{text}");
    }
    let full = report("explain", "batch.ts");
    let text = s.call("explain", json!({"target":"batch.ts"}));
    assert_eq!(full["truncated"], false);
    assert_eq!(full["conclusion"], json!({"verdict":"found","gaps":[]}));
    assert!(
        text.len() <= 1500
            && text.contains(&format!("第 {generation} 代"))
            && text.contains("其余文件未核对")
    );
    let displayed: Vec<_> = text.lines().filter(|l| l.contains(" → ")).collect();
    let relations = full["relations"].as_array().unwrap();
    assert!(!displayed.is_empty() && displayed.len() < relations.len());
    assert!(text.contains(&format!(
        "省略 {} 条关系",
        relations.len() - displayed.len()
    )));
    for (line, e) in displayed.into_iter().zip(relations) {
        assert_eq!(
            line,
            format!(
                "batch.ts → {} · 第 {} 行 · 静态确定 · 第 1 跳",
                e["target"].as_str().unwrap(),
                e["line"]
            )
        );
    }
    s.process.finish();
}

#[test]
fn session_lists_queries_and_retracts_indexed_references() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dir.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/entry.ts"), "import './middle';\n").unwrap();
    fs::write(root.join("src/middle.ts"), "import './leaf';\n").unwrap();
    fs::write(root.join("src/leaf.ts"), "export const leaf = 1;\n").unwrap();
    Process::start(root, &["--root", ".", "init"]).finish();

    let mut session = Session::start(root);
    session.request("initialize", json!({
        "protocolVersion":"2025-06-18", "capabilities":{}, "clientInfo":{"name":"loom-test", "version":"1"}
    }));
    session.send(json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
    let list = session.request("tools/list", json!({}));
    let tools = list["tools"].as_array().unwrap();
    for name in ["index", "explain", "impact"] {
        assert!(
            tools.iter().any(|t| t["name"] == name),
            "工具列表缺少 {name}"
        );
    }

    session.call("index", json!({}));
    let middle = json!({"target":"src/middle.ts"});
    let incoming = "src/entry.ts → src/middle.ts";
    let outgoing = "src/middle.ts → src/leaf.ts";
    let explain = session.call("explain", middle.clone());
    assert!(
        explain.contains(outgoing) && !explain.contains(incoming),
        "{explain}"
    );
    let impact = session.call("impact", middle.clone());
    assert!(
        impact.contains(incoming) && !impact.contains(outgoing),
        "{impact}"
    );

    let entry = json!({"target":"src/entry.ts"});
    let direct = session.call("explain", entry);
    assert!(
        direct.contains(incoming) && !direct.contains(outgoing),
        "{direct}"
    );
    let deep = session.call("explain", json!({"target":"src/entry.ts", "depth":2}));
    assert!(deep.contains(outgoing), "{deep}");

    fs::write(root.join("src/entry.ts"), "export const entry = 1;\n").unwrap();
    session.call("index", json!({}));
    let after = session.call("impact", middle);
    assert!(
        !after.contains("src/entry.ts"),
        "重建索引后仍有旧引用：{after}"
    );
    session.process.finish();
}
