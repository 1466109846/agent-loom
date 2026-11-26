use super::*;
use std::{
    net::{Ipv4Addr, TcpListener},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
#[path = "../../../loom-core/tests/support/model.rs"]
mod mock;

fn cli(root: &Path, args: &[&str], success: bool) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_loom"))
        .current_dir(root)
        .env("LOOM_CONFIG", root.join("fixture-config.toml"))
        .args(args)
        .output()
        .unwrap();
    assert_eq!(out.status.success(), success, "子进程状态");
    (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}
fn config(root: &Path, url: &str, rank: bool) {
    let mut body = format!(
        "[embedding]\nmodel='mock'\nbase_url='{url}'\nbatch=2\ndimensions=2\ntimeout_ms=500\napi_key_env='LOOM_SYNTHETIC_NO_KEY'\n"
    );
    if rank {
        body.push_str(&format!(
            "[rerank]\nmodel='rank'\nbase_url='{url}'\ntimeout_ms=500\n"
        ));
    }
    fs::write(root.join("fixture-config.toml"), body).unwrap();
}

#[test]
fn model_test_classifies_failures_without_project_or_secret_disclosure() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let key = loom_core::util::new_id();
    let calls = Arc::new(AtomicUsize::new(0));
    let state = calls.clone();
    let expected = key.clone();
    let srv = mock::Server::new(move |sock, r| {
        assert!(r.auth.as_deref() == Some(&format!("Bearer {expected}")));
        assert!(r.body["input"] == json!(["loom 连通性测试"]));
        match state.fetch_add(1, Ordering::SeqCst) {
            0 => mock::reply(sock, 401, &expected),
            1 => mock::reply(sock, 404, &expected),
            2 => mock::reply(sock, 200, "not JSON"),
            3 => {
                thread::sleep(Duration::from_millis(650));
                mock::reply(sock, 200, "{}");
            }
            _ => mock::reply(
                sock,
                200,
                &mock::embeddings(&r.body, |_| vec![3., 4.]).to_string(),
            ),
        }
    });
    fs::write(
        root.join("credentials.toml"),
        format!("[embedding]\napi_key='{key}'"),
    )
    .unwrap();
    config(root, &format!("http://127.0.0.1:{}/v1", srv.port), false);
    for (ok, why) in [
        (false, "鉴权失败"),
        (false, "模型不存在"),
        (false, "返回格式不符"),
        (false, "超时"),
        (true, "维度 2"),
    ] {
        let (out, err) = cli(root, &["model", "test"], ok);
        assert!(!out.contains(&key) && !err.contains(&key));
        assert!(out.contains(why) && out.contains("凭据文件") && out.contains("重排：未配置"));
    }
    config(root, "http://example.invalid/v1", false);
    assert!(
        cli(root, &["model", "test"], false)
            .0
            .contains("地址不合规")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 5);
}

#[test]
fn model_pull_streams_only_missing_loopback_ollama_embeddings() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let mode = Arc::new(AtomicUsize::new(0));
    let state = mode.clone();
    let downloads = Arc::new(AtomicUsize::new(0));
    let calls = downloads.clone();
    let srv = mock::Server::new(move |sock, r| {
        assert!(r.auth.is_none());
        if r.route == "GET /api/tags" {
            let v = match state.load(Ordering::SeqCst) {
                0 => json!({"models":[{"name":"mock:latest"}]}),
                1 => json!({"models":[]}),
                _ => json!({"different":"service"}),
            };
            mock::reply(sock, 200, &v.to_string());
        } else {
            assert!(r.route == "POST /api/pull" && r.body == json!({"model":"mock","stream":true}));
            calls.fetch_add(1, Ordering::SeqCst);
            let frames = "{\"status\":\"downloading\",\"completed\":50,\"total\":200}\n{\"status\":\"success\"}\n";
            thread::sleep(Duration::from_millis(600));
            write!(
                sock,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                frames.len()
            )
            .unwrap();
            let (a, b) = frames.split_at(20);
            sock.write_all(a.as_bytes()).unwrap();
            thread::sleep(Duration::from_millis(650));
            sock.write_all(b.as_bytes()).unwrap();
        }
    });
    config(root, &format!("http://127.0.0.1:{}/v1", srv.port), true);
    let (have, _) = cli(root, &["model", "pull"], true);
    assert!(have.contains("已存在") && have.contains("Ollama 没有重排接口"));
    mode.store(1, Ordering::SeqCst);
    let (pulled, progress) = cli(root, &["model", "pull"], true);
    assert!(pulled.contains("下载成功") && progress.contains("25%"));
    mode.store(2, Ordering::SeqCst);
    assert_eq!(
        cli(root, &["model", "pull"], true)
            .0
            .matches("不适用")
            .count(),
        2
    );
    config(root, "https://never-contact.invalid/v1", true);
    assert_eq!(
        cli(root, &["model", "pull"], true)
            .0
            .matches("不适用")
            .count(),
        2
    );
    assert_eq!(downloads.load(Ordering::SeqCst), 1);
}

#[test]
fn inactive_cli_and_session_match_frozen_bytes_and_active_preview_stays_bounded() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let cfg = root.join("fixture-config.toml");
    let trap = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    trap.set_nonblocking(true).unwrap();
    let trap_url = format!("http://127.0.0.1:{}/v1", trap.local_addr().unwrap().port());
    let call = |args: &[&str]| cli(root, args, true).0;
    call(&["--root", ".", "init"]);
    let body = "选用不会随时间衰减的决定验证固定格式。";
    let id = call(&["remember", "DECISION", "固定标题", body]);
    let id = id.trim();
    let p = loom_core::project::ProjectLocation::select(root, Some(root), None).unwrap();
    let date =
        loom_core::util::fmt_date(p.open(false).unwrap().get(id).unwrap().unwrap().updated_at);
    let expected = format!("[1] 0.800 DECISION 固定标题 · {date} · Fresh/Clean · id={id}\n");
    let short = format!("[1] DECISION 固定标题 · {date} · id={id}\n[1] 摘要：{body}\n");
    let mut session = Session::start(root);
    session.request("initialize",json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"mock","version":"1"}}));
    session.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    for configured in [false, true] {
        if configured {
            config(root, &trap_url, true);
        }
        assert_eq!(
            call(&["recall", "固定标题"]).as_bytes(),
            expected.as_bytes()
        );
        assert_eq!(
            session
                .call("recall", json!({"query":"固定标题"}))
                .as_bytes(),
            short.as_bytes()
        );
    }
    fs::remove_file(&cfg).unwrap();
    assert!(
        cli(root, &["semantic", "enable"], false)
            .1
            .contains("embedding.base_url")
    );
    let srv = mock::Server::new(|sock, r| {
        let v = if r.route.ends_with("embeddings") {
            mock::embeddings(&r.body, |_| vec![1., 0.])
        } else {
            json!({"results":r.body["documents"].as_array().unwrap().iter().enumerate().map(|(i,_)|json!({"index":i,"score":1})).collect::<Vec<_>>()})
        };
        mock::reply(sock, 200, &v.to_string());
    });
    config(root, &format!("http://127.0.0.1:{}/v1", srv.port), true);
    call(&["semantic", "enable", "--yes"]);
    let long = "材料".repeat(250);
    call(&["remember", "GOTCHA", "另一个检查条件", &long]);
    let r = session.call("recall", json!({"query":"完全不相干"}));
    assert!(
        r.starts_with("检索方式：关键词 + 向量 + 重排\n")
            && r.len() <= 1500
            && r.contains("已省略")
    );
    let st = call(&["semantic", "status"]);
    assert!(st.contains("已有当前向量 2，待建或过期 0"));
    config(root, &trap_url, true);
    assert!(
        session
            .call("recall", json!({"query":"固定标题"}))
            .contains("模型地址已变更")
    );
    call(&["semantic", "disable", "--yes", "--purge"]);
    assert_eq!(session.call("recall", json!({"query":"固定标题"})), short);
    assert_eq!(
        trap.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    session.process.finish();
}
