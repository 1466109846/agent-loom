use super::{
    hybrid_recall::configured,
    model_server::{Server, embeddings, reply},
    *,
};
use crate::{
    config::UserConfig,
    enhance::{
        hybrid,
        models::{Failure, Models},
    },
    search::RecallQuery,
};
use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[test]
fn origin_rules_normalize_ports_and_reject_ambiguous_urls() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("model.toml");
    for (url, origin) in [
        ("http://127.2.3.4/api", Some("http://127.2.3.4:80")),
        ("http://[::1]:91/api", Some("http://[::1]:91")),
        ("http://LOCALHOST:80/v1", Some("http://localhost:80")),
        (
            "https://api.example.invalid/v1",
            Some("https://api.example.invalid:443"),
        ),
        ("http://127.0.0.1.example.invalid", None),
        ("http://localhost@else.invalid", None),
        ("https://user@localhost/v1", None),
        ("http://localhost:99999", None),
        ("http://localhost:0", None),
        ("http://localhost/v1?field=data", None),
        ("https://localhost/v1#fragment", None),
    ] {
        fs::write(
            &file,
            format!("[embedding]\nbase_url='{url}'\nmodel='unit'"),
        )
        .unwrap();
        let svc = Models::from_config(UserConfig::load_from(&file)).embedding;
        assert_eq!(
            svc.as_ref()
                .ok()
                .and_then(Option::as_ref)
                .map(|s| s.origin()),
            origin
        );
        if origin.is_none() {
            // 带凭据、查询串这类写法在读配置时就被拒；合法的地址用错了协议才到地址规则。
            assert!(matches!(svc, Err(Failure::Address | Failure::Config)));
        }
    }
}

#[test]
fn only_clean_documents_and_query_leave_the_project_with_origin_scoped_keys() {
    let (dir, _loc, s) = project();
    let token = crate::util::new_id();
    let key = token.clone();
    let embed = Server::new(move |sock, r| {
        assert!(r.auth.as_deref() == Some(&format!("Bearer {key}")));
        let query = r.body["input"][0].as_str().unwrap().starts_with("询问");
        let inputs = if query {
            json!(["询问 token=***"])
        } else {
            json!(["旧 标题 token=***\n正文 密码已省略"])
        };
        assert!(
            r.route == "POST /v1/embeddings"
                && r.body
                    == json!({"model":"embed","input":inputs,"encoding_format":"float","dimensions":2})
        );
        reply(
            sock,
            200,
            &embeddings(&r.body, |_| vec![1., 0.]).to_string(),
        );
    });
    let rank = Server::new(|sock, r| {
        assert!(r.auth.is_none());
        assert!(r.route == "POST /v1/rerank");
        assert!(
            r.body
                == json!({"model":"rank","query":"询问 token=***","documents":["旧 标题 token=***\n正文 密码已省略"],"return_documents":false,"top_n":1})
        );
        reply(
            sock,
            200,
            "{\"results\":[{\"index\":0,\"relevance_score\":0.2}]}",
        );
    });
    record(
        &s,
        1,
        "旧\n标题 token=private-placeholder",
        "正文\n密码已省略",
    );
    s.conn
        .execute("UPDATE memories SET tags='[\"never-send-tag\"]'", [])
        .unwrap();
    s.conn
        .execute(
            "INSERT INTO anchors(memory_id,path) VALUES(?1,'src/private.rs')",
            [id(1)],
        )
        .unwrap();
    let file = dir.path().join("config.toml");
    fs::write(&file,format!("[embedding]\nmodel='embed'\nbase_url='http://127.0.0.1:{}/v1'\ndimensions=2\nbatch=1\n[rerank]\nmodel='rank'\nbase_url='http://127.0.0.1:{}/v1'",embed.port,rank.port)).unwrap();
    fs::write(
        dir.path().join("credentials.toml"),
        format!("[embedding]\napi_key='{token}'"),
    )
    .unwrap();
    let m = || Models::from_config(UserConfig::load_from(&file));
    let pre = hybrid::preview(&s, &m()).unwrap();
    hybrid::enable(&s, &m()).unwrap();
    let indexed = hybrid::index(&s, &m(), None, |_| ()).unwrap();
    let result = hybrid::recall(
        &s,
        &RecallQuery {
            query: "询问 token=other-private".into(),
            ..Default::default()
        },
        m,
    )
    .unwrap();
    assert_eq!(result.heading, "检索方式：关键词 + 向量 + 重排\n");
    for out in [
        pre,
        hybrid::status(&s, &m()).unwrap(),
        indexed.to_string(),
        result.heading,
    ] {
        assert!(!out.contains(&token));
    }
}

#[test]
fn malformed_batches_limits_and_redirects_discard_the_whole_reply() {
    let sink = trap();
    let target = port(&sink);
    let n = Arc::new(AtomicUsize::new(0));
    let step = n.clone();
    let srv = Server::new(move |sock, _| {
        let seq = step.fetch_add(1, Ordering::SeqCst);
        match seq {
            0..=5 => {
                let v = match seq {
                    0 => json!([[1], [1, 2]]),
                    1 => json!([[0], [1]]),
                    2 => json!([[], []]),
                    3 => json!([[1]]),
                    4 => json!([vec![1; 4097], vec![2; 4097]]),
                    _ => json!([[1], [1]]),
                };
                let data: Vec<_> = v
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .map(|(i, v)| json!({"index":if seq==5 {0}else{i},"embedding":v}))
                    .collect();
                reply(sock, 200, &json!({"data":data}).to_string());
            }
            6 => {
                let _ = write!(
                    sock,
                    "HTTP/1.1 307 Redirect\r\nLocation: http://127.0.0.1:{target}/sink\r\nContent-Length: 0\r\n\r\n"
                );
            }
            7 => {
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8388609\r\n\r\n");
            }
            8 => {
                let _ = write!(
                    sock,
                    "HTTP/1.1 200 OK\r\nX-Large: {}\r\nContent-Length: 0\r\n\r\n",
                    "a".repeat(32 << 10)
                );
            }
            9 => {
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 262145\r\n\r\n");
            }
            _ => reply(
                sock,
                200,
                &json!({"results":[{"index":1,"score":2},{"index":1,"score":0}]}).to_string(),
            ),
        }
    });
    let (_dir, _loc, s) = project();
    record(&s, 1, "甲", "乙");
    record(&s, 2, "乙", "甲");
    let m = configured(srv.port, "x", 2, None);
    hybrid::enable(&s, &m).unwrap();
    for seq in 0..9 {
        let r = hybrid::index(&s, &m, None, |_| ()).unwrap();
        assert_eq!((r.new, r.failed), (0, 2));
        assert_eq!(
            r.reason,
            Some(if seq == 6 {
                Failure::Service
            } else {
                Failure::Format
            })
        );
    }
    let model = m.embedding.unwrap().unwrap();
    for _ in 0..2 {
        assert_eq!(model.rank("query", &["one", "two"]), Err(Failure::Format));
    }
    assert_eq!(n.load(Ordering::SeqCst), 11);
    unused(&sink);
}
