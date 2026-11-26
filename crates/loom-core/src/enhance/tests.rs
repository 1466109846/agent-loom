use super::*;
use crate::project::{Bind, ProjectLocation};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::{process::Command, thread, time::Instant};

mod evaluation;
mod failures;
mod hybrid_recall;
mod model_protocol;
#[path = "../../tests/support/model.rs"]
mod model_server;
mod semantic_consent;
const WAIT: Duration = Duration::from_secs(5);
const PRIVATE_BODY: &str = "这段私有正文绝不能出现在模型请求或报告里";

fn project() -> (tempfile::TempDir, ProjectLocation, Store) {
    let dir = tempfile::tempdir().unwrap();
    let loc = ProjectLocation::select(dir.path(), Some(dir.path()), None).unwrap();
    loc.bind(Bind::Initialize).unwrap();
    let store = loc.open(true).unwrap();
    (dir, loc, store)
}

fn id(n: usize) -> String {
    ulid::Ulid::from(n as u128).to_string()
}

// 用真实 FTS 触发器；直接插入可测试含旧式未脱敏标题的库。日期间隔使评分没有随机并列。
fn record(store: &Store, n: usize, title: &str, body: &str) {
    store
        .conn
        .execute(
            "INSERT INTO memories(id,type,title,body,content_hash,created_at,updated_at)
        VALUES(?1,'CONFIG',?2,?3,?1,?4,?4)",
            params![id(n), title, body, crate::util::now_ts() - n as i64 * 86400],
        )
        .unwrap();
}

fn trap() -> TcpListener {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap()
}
fn port(listener: &TcpListener) -> u16 {
    listener.local_addr().unwrap().port()
}
fn unused(listener: &TcpListener) {
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
fn model(p: u16) -> Model {
    Model::parse(Some("mock-test".as_ref()), Some(p.to_string().as_ref()))
}

struct Request {
    head: String,
    json: Value,
}

fn with_mock<R>(
    serve: impl FnOnce(&mut TcpStream, Request) + Send,
    client: impl FnOnce(u16) -> R,
) -> R {
    let listener = trap();
    let p = port(&listener);
    listener.set_nonblocking(true).unwrap();
    thread::scope(|scope| {
        let server = scope.spawn(move || {
            let end = Instant::now() + WAIT;
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < end =>
                    {
                        thread::sleep(Duration::from_millis(2))
                    }
                    _ => panic!("模拟服务未收到请求"),
                }
            };
            // Windows 接受的连接继承监听套接字的非阻塞模式；后续按超时阻塞读取。
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(WAIT)).unwrap();
            stream.set_write_timeout(Some(WAIT)).unwrap();
            let mut r = BufReader::new(&stream);
            let mut head = String::new();
            while !head.ends_with("\r\n\r\n") {
                assert!(head.len() < 8192 && r.read_line(&mut head).unwrap() > 0);
            }
            let len: usize = head
                .lines()
                .filter_map(|s| s.split_once(':'))
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .unwrap()
                .1
                .trim()
                .parse()
                .unwrap();
            assert!(len <= 16 * 1024);
            let mut bytes = vec![0; len];
            r.read_exact(&mut bytes).unwrap();
            assert!(!head.to_ascii_lowercase().contains("authorization:"));
            serve(
                &mut stream,
                Request {
                    head,
                    json: serde_json::from_slice(&bytes).unwrap(),
                },
            );
        });
        let result = client(p);
        server.join().unwrap();
        result
    })
}
fn json_reply(s: &mut TcpStream, value: Value) {
    let bytes = serde_json::to_vec(&value).unwrap();
    write!(
        s,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        bytes.len()
    )
    .unwrap();
    s.write_all(&bytes).unwrap();
}
fn rows(report: &str) -> Vec<(usize, String)> {
    report
        .lines()
        .filter_map(|line| line.strip_prefix("- 关键词#"))
        .map(|line| {
            let mut fields = line.split_whitespace();
            (
                fields.next().unwrap().parse().unwrap(),
                fields.next().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn admin_switches() {
    // 只对子进程设置环境，不在并行的 Rust 测试中修改进程全局环境。
    const CHILD: &str = "LOOM_ENHANCE_TEST_CASE";
    if let Ok(case) = std::env::var(CHILD) {
        let (_dir, loc, writer) = project();
        record(&writer, 1, "主题", PRIVATE_BODY);
        let store = loc.open(false).unwrap();
        let r = rerank(&store, "主题").unwrap();
        let e = extract(&store, "当次输入").unwrap();
        assert_eq!(r.contains("候选内重排"), case == "embed");
        assert_eq!(r.contains("关键词回退"), case != "embed");
        assert_eq!(e.contains("模拟建议"), case == "extract");
        if case != "extract" {
            assert!(e.contains("未启用") || e.contains("配置无效"));
        }
        return;
    }
    let proxy = trap();
    let run = |case: &str, p: u16| {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "enhance::tests::admin_switches", "--nocapture"])
            .env(CHILD, case)
            .env("LOOM_MODEL_PORT", p.to_string())
            .env_remove("LOOM_EMBED_MODEL")
            .env_remove("LOOM_EXTRACT_MODEL");
        let fake = format!("http://127.0.0.1:{}", port(&proxy));
        for key in [
            "http_proxy",
            "HTTP_PROXY",
            "ALL_PROXY",
            "OLLAMA_HOST",
            "LOOM_MODEL_URL",
        ] {
            child.env(key, &fake);
        }
        child.env("NO_PROXY", "");
        match case {
            "embed" => {
                child.env("LOOM_EMBED_MODEL", "nomic-embed-text:latest");
            }
            "extract" => {
                child.env("LOOM_EXTRACT_MODEL", "qwen2.5:3b");
            }
            "invalid" => {
                child
                    .env("LOOM_EMBED_MODEL", &fake)
                    .env("LOOM_EXTRACT_MODEL", " ");
            }
            _ => (),
        }
        let out = child.output().unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let closed = trap();
    run("off", port(&closed));
    run("invalid", port(&closed));
    unused(&closed);
    for (case, route, reply) in [
        ("embed", "/api/embed", json!({"embeddings":[[1],[1]]})),
        ("extract", "/api/generate", json!({"response":"模拟建议"})),
    ] {
        with_mock(
            |s, req| {
                assert!(req.head.starts_with(&format!("POST {route} ")));
                json_reply(s, reply);
            },
            |p| run(case, p),
        );
    }
    unused(&proxy);
}

#[test]
fn port_and_model_are_data_not_addresses() {
    let val = |s: &'static str| Some(std::ffi::OsStr::new(s));
    assert!(
        matches!(Model::parse(val("nomic-embed-text:latest"), None), Model::Local { port, .. } if port.get() == 11434)
    );
    assert!(
        matches!(Model::parse(val("qwen2.5:3b"), val("65535")), Model::Local { port, .. } if port.get() == 65535)
    );
    for bad in [
        "",
        "0",
        "65536",
        "+80",
        "-1",
        " 80",
        "127.0.0.1:80",
        "80/else",
    ] {
        assert!(matches!(
            Model::parse(val("model"), val(bad)),
            Model::Invalid
        ));
    }
    for bad in [
        "",
        " ",
        "https://elsewhere",
        "../secret",
        "name\nextra",
        "model:",
        "m:a:b",
        "@registry",
        "--flag",
        &"x".repeat(256),
    ] {
        assert!(!matches!(
            Model::parse(Some(bad.as_ref()), None),
            Model::Local { .. }
        ));
    }
}

#[test]
fn embed_only_the_redacted_shortlist_and_keep_lexical_ranks_on_ties() {
    let (_dir, loc, writer) = project();
    for n in 1..=16 {
        record(
            &writer,
            n,
            &format!("batch 用户选题 {n} token=fixture-{n}"),
            PRIVATE_BODY,
        );
    }
    writer.set_status(&[id(2)], "archived").unwrap();
    let store = loc.open(false).unwrap();
    let baseline = rows(&rerank_with(&store, "batch", Model::Off, WAIT).unwrap());
    let ids = baseline
        .iter()
        .map(|(_, id)| id.clone())
        .collect::<Vec<_>>();
    let report = with_mock(
        |s, req| {
            assert!(req.head.starts_with("POST /api/embed "));
            let inputs = req.json["input"].as_array().unwrap();
            assert_eq!(inputs.len(), 13);
            assert_eq!(inputs[0], "batch token=***");
            for (i, input) in inputs[1..].iter().enumerate() {
                let n = ulid::Ulid::from_string(&ids[i]).unwrap().0;
                assert_eq!(*input, format!("batch 用户选题 {n} token=***"));
            }
            let payload = req.json.to_string();
            assert!(!payload.contains(PRIVATE_BODY) && !payload.contains(&ids[0]));
            let mut v = vec![json!([1000, 1000]); 13];
            v[0] = json!([1, 0]);
            v[12] = json!([0.5, 0]);
            json_reply(s, json!({"embeddings":v}));
        },
        |p| {
            rerank_with(
                &store,
                &format!("batch token={}", "fixture-query"),
                model(p),
                WAIT,
            )
            .unwrap()
        },
    );
    assert!(
        report.contains("候选内重排")
            && !report.contains(PRIVATE_BODY)
            && !report.contains("fixture-")
    );
    let mut expected = baseline;
    assert_eq!(expected.len(), 12);
    expected.rotate_right(1);
    assert_eq!(rows(&report), expected);
    assert!(report.len() <= 4096);
}

#[test]
fn extract_only_submitted_text_and_never_changes_state() {
    let (dir, loc, writer) = project();
    record(&writer, 1, "库内标题不得外发", PRIVATE_BODY);
    let db = dir.path().join(".loom/loom.db");
    let store = loc.open(false).unwrap();
    let long = "字".repeat(3000);
    let report = with_mock(
        |s, req| {
            assert!(req.head.starts_with("POST /api/generate "));
            assert_eq!(req.json["stream"], false);
            assert!(
                req.json["prompt"]
                    .as_str()
                    .unwrap()
                    .contains("只提交这一段 token=***")
            );
            let payload = req.json.to_string();
            for omitted in [PRIVATE_BODY, "库内标题不得外发", "fixture-secret"] {
                assert!(!payload.contains(omitted));
            }
            let other = Connection::open(&db).unwrap();
            other.busy_timeout(Duration::ZERO).unwrap();
            other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
            json_reply(
                s,
                json!({"response":format!("建议\n\u{1b}token=fixture-output \u{202e}已确认 {long}"),"done":true}),
            );
        },
        |p| {
            extract_with(
                &store,
                &format!("只提交这一段\ntoken={}", "fixture-secret"),
                model(p),
                WAIT,
            )
            .unwrap()
        },
    );
    assert!(
        report.contains("待人工审阅") && report.contains("来源：本次") && report.contains("未写入")
    );
    assert!(report.contains("建议") && report.contains("token=***") && report.contains('…'));
    assert!(!report.contains(['\u{1b}', '\u{202e}']) && !report.contains("fixture-output"));
    assert!(report.len() <= 4096);
    assert_eq!(store.conn.total_changes(), 0);
    assert_eq!(
        store
            .conn
            .query_row("SELECT count(*) FROM intents", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(store.count().unwrap(), 1);
}

#[test]
fn stale_marker_database_or_unbound_store_stops_both_paths() {
    let (dir, _loc, s) = project();
    record(&s, 1, "private-marker", PRIVATE_BODY);
    let trap = trap();
    let refused = |s: &Store| {
        assert!(rerank_with(s, "private-marker", model(port(&trap)), WAIT).is_err());
        assert!(extract_with(s, "private-marker", model(port(&trap)), WAIT).is_err());
    };
    refused(&Store::open_in_memory().unwrap());
    s.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    refused(&s);
    s.conn.execute_batch("ROLLBACK").unwrap();
    let marker = dir.path().join(".loom/project.id");
    let saved = std::fs::read(&marker).unwrap();
    std::fs::write(&marker, id(99)).unwrap();
    refused(&s);
    std::fs::write(&marker, saved).unwrap();
    s.conn
        .execute("UPDATE loom_identity SET project_id=?1", [id(98)])
        .unwrap();
    refused(&s);
    unused(&trap);
}

#[test]
fn recheck_binding_after_model_wait() {
    let (dir, _loc, store) = project();
    record(&store, 1, "查询", PRIVATE_BODY);
    let marker = dir.path().join(".loom/project.id");
    let r = with_mock(
        |s, _| {
            std::fs::write(&marker, id(88)).unwrap();
            json_reply(s, json!({"embeddings":[[1],[1]]}));
        },
        |p| rerank_with(&store, "查询", model(p), WAIT),
    );
    assert!(r.is_err());
}

#[test]
fn inputs_are_small_and_empty_results_are_not_model_errors() {
    let (_dir, _loc, store) = project();
    let trap = trap();
    let p = port(&trap);
    for q in [" ", &"查".repeat(2048)] {
        assert!(rerank_with(&store, q, model(p), WAIT).is_err());
    }
    for text in ["\u{202e}\n", &"文".repeat(4096)] {
        assert!(extract_with(&store, text, model(p), WAIT).is_err());
    }
    assert!(
        rerank_with(&store, "无命中", model(p), WAIT)
            .unwrap()
            .contains("无关键词候选")
    );
    unused(&trap);
    let empty = with_mock(
        |s, _| json_reply(s, json!({"response":" \n "})),
        |p| extract_with(&store, "当次文本", model(p), WAIT).unwrap(),
    );
    assert!(empty.contains("无可审阅候选") && !empty.contains("不可用"));
}
