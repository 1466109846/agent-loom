use super::{
    model_server::{Server, embeddings, reply},
    *,
};
use crate::{
    config::UserConfig,
    enhance::{hybrid, models::Models},
    search::RecallQuery,
};
use std::{
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

pub(super) fn configured(port: u16, name: &str, batch: usize, rank: Option<u16>) -> Models {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("test.toml");
    let mut text = format!(
        "[embedding]\nbase_url='http://127.0.0.1:{port}/v1'\nmodel='{name}'\nbatch={batch}\ntimeout_ms=1000\n"
    );
    if let Some(p) = rank {
        text.push_str(&format!(
            "[rerank]\nbase_url='http://127.0.0.1:{p}/v1'\nmodel='rank'\ntimeout_ms=1000\n"
        ));
    }
    fs::write(&file, text).unwrap();
    Models::from_config(UserConfig::load_from(&file))
}
fn q(query: &str) -> RecallQuery {
    RecallQuery {
        query: query.into(),
        ..Default::default()
    }
}
fn count(s: &Store) -> i64 {
    s.conn
        .query_row("SELECT count(*) FROM memory_vectors", [], |r| r.get(0))
        .unwrap()
}
fn ids(r: &hybrid::Recall) -> Vec<String> {
    r.hits.iter().map(|h| h.memory.id.clone()).collect()
}

#[test]
fn zero_hit_rrf_rerank_ties_filters_folding_and_model_fallbacks() {
    let (_dir, _loc, s) = project();
    for (n, title) in [(1, "苹果"), (2, "香蕉"), (3, "香蕉"), (4, "桃子")] {
        record(&s, n, title, "水果记忆");
    }
    s.conn
        .execute("UPDATE memories SET type='DECISION',updated_at=100", [])
        .unwrap();
    s.conn
        .execute("UPDATE memories SET type='CONFIG' WHERE id=?1", [id(4)])
        .unwrap();
    s.set_status(&[id(4)], "archived").unwrap();
    let mode = Arc::new(AtomicUsize::new(0));
    let state = mode.clone();
    let srv = Server::new(move |sock, r| {
        let mode = state.load(Ordering::SeqCst);
        if r.route.ends_with("embeddings") {
            if mode == 3 {
                reply(sock, 503, "private reply");
                return;
            }
            if mode == 4 {
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 200\r\n\r\n");
                for _ in 0..50 {
                    if sock.write_all(b" ").is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(70));
                }
                return;
            }
            reply(
                sock,
                200,
                &embeddings(&r.body, |s| {
                    if s.starts_with("苹果") {
                        vec![0., 1.]
                    } else {
                        vec![1., 0.]
                    }
                })
                .to_string(),
            );
        } else {
            assert_eq!(r.route, "POST /v1/rerank");
            if mode == 2 {
                reply(sock, 401, "untrusted failure");
                return;
            }
            let docs = r.body["documents"].as_array().unwrap();
            let results=docs.iter().enumerate().rev().map(|(i,d)|json!({"index":i,"score":if mode==1 && d.as_str().unwrap().starts_with("苹果"){2.0}else{1.0}})).collect::<Vec<_>>();
            reply(sock, 200, &json!({"results":results}).to_string());
        }
    });
    let m = || configured(srv.port, "one", 2, Some(srv.port));
    hybrid::enable(&s, &m()).unwrap();
    hybrid::index(&s, &m(), None, |_| ()).unwrap();
    let query = q("unrelated");
    assert!(s.recall(&query).unwrap().is_empty());
    let run = |query: &RecallQuery| hybrid::recall(&s, query, m).unwrap();
    let ties = run(&query);
    assert_eq!(ids(&ties), [id(2), id(1)]);
    assert_eq!(ties.hits[0].folded_ids, [id(3)]);
    assert_eq!(ties.heading, "检索方式：关键词 + 向量 + 重排\n");
    mode.store(1, Ordering::SeqCst);
    assert_eq!(
        ids(&run(&RecallQuery {
            top_k: 1,
            ..query.clone()
        })),
        [id(1)]
    );
    mode.store(2, Ordering::SeqCst);
    let r = run(&query);
    assert_eq!(ids(&r), ids(&ties));
    assert!(r.heading.contains("重排不可用：鉴权失败"));
    let by_type = RecallQuery {
        types: vec![crate::MemoryType::Config],
        ..q("桃子")
    };
    assert!(run(&by_type).hits.is_empty());
    assert_eq!(
        ids(&run(&RecallQuery {
            include_archived: true,
            ..by_type
        })),
        [id(4)]
    );
    for (mode_no, why) in [(3, "服务端错误"), (4, "超时")] {
        mode.store(mode_no, Ordering::SeqCst);
        let start = Instant::now();
        let fallback = run(&q("苹果"));
        assert_eq!(
            fallback.heading,
            format!("检索方式：仅关键词（向量不可用：{why}）\n")
        );
        assert_eq!(ids(&fallback), [id(1)]);
        assert!(start.elapsed() < Duration::from_millis(2400));
    }
}

#[test]
fn writes_revalidate_digest_consent_and_project_after_wait() {
    for change in ["body", "disable", "identity"] {
        let (dir, loc, s) = project();
        record(&s, 1, "before", "safe body");
        let writer = loc.open(true).unwrap();
        let marker = dir.path().join(".loom/project.id");
        let srv = Server::new(move |sock, r| {
            writer.conn.busy_timeout(Duration::ZERO).unwrap();
            match change {
                "body" => {
                    writer
                        .conn
                        .execute("UPDATE memories SET body='changed'", [])
                        .unwrap();
                }
                "disable" => {
                    hybrid::disable(&writer, true).unwrap();
                }
                _ => fs::write(&marker, id(99)).unwrap(),
            }
            reply(sock, 200, &embeddings(&r.body, |_| vec![1.]).to_string());
        });
        let m = || configured(srv.port, "one", 2, None);
        hybrid::enable(&s, &m()).unwrap();
        let found = hybrid::recall(&s, &q("unrelated"), m);
        if change == "identity" {
            assert!(found.is_err());
        } else {
            let r = found.unwrap();
            assert!(r.hits.is_empty());
            assert_eq!(r.heading.is_empty(), change == "disable");
        }
        assert_eq!(count(&s), 0);
    }
}

#[test]
fn damaged_optional_cache_still_returns_exact_keyword_hits() {
    let (_dir, _loc, s) = project();
    record(&s, 1, "标题", "完全离线的关键词结果");
    let sink = trap();
    let m = || configured(port(&sink), "fixture", 2, None);
    hybrid::enable(&s, &m()).unwrap();
    s.conn
        .execute_batch("DROP TABLE memory_vectors; CREATE TABLE memory_vectors(unrelated TEXT)")
        .unwrap();
    let r = hybrid::recall(&s, &q("标题"), m).unwrap();
    assert!(r.heading.contains("仅关键词（向量不可用：向量缓存不可读）"));
    assert_eq!(ids(&r), [id(1)]);
    unused(&sink);
}

#[test]
fn resumable_index_tracks_payload_model_lifecycle_and_sensitive_rows() {
    let (_dir, loc, s) = project();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let srv = Server::new(move |sock, r| {
        assert_eq!(r.route, "POST /v1/embeddings");
        assert!(r.auth.is_none());
        let inputs = r.body["input"].as_array().unwrap();
        assert!(
            inputs
                .iter()
                .all(|v| v.as_str().unwrap().chars().count() <= 2000)
        );
        assert!(!r.body.to_string().contains("residual-material"));
        seen.lock().unwrap().push(inputs.len());
        let v = embeddings(&r.body, |_| vec![3., 4.]);
        reply(sock, 200, &v.to_string());
    });
    let model = |name| configured(srv.port, name, 2, None);
    for n in 1..=6 {
        record(&s, n, &format!("标题 {n}"), &"字".repeat(2500));
    }
    record(&s, 7, "Bearer", "residual-material-0000");
    let pre = s.schema_version().unwrap();
    let db = loc.database();
    let bytes = fs::read(db).unwrap();
    assert!(
        hybrid::preview(&loc.open(false).unwrap(), &model("one"))
            .unwrap()
            .contains("有效经验 7 条；向量空间：维度未配置，按 1024 维估算约 28.0 KiB")
    );
    assert!(fs::read(db).unwrap() == bytes);
    hybrid::enable(&s, &model("one")).unwrap();
    let index = |name, limit| hybrid::index(&s, &model(name), limit, |_| ()).unwrap();
    assert_eq!(index("one", Some(3)).new, 3);
    let next = index("one", None);
    assert_eq!((next.new, next.skipped), (3, 1));
    assert_eq!(*calls.lock().unwrap(), [2, 1, 2, 1]);
    let v: Vec<u8> = s
        .conn
        .query_row("SELECT vector FROM memory_vectors LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        v,
        [0.6f32, 0.8]
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect::<Vec<_>>()
    );
    s.conn
        .execute("UPDATE memories SET title='标题改动' WHERE id=?1", [id(1)])
        .unwrap();
    s.conn
        .execute("UPDATE memories SET body='正文改动' WHERE id=?1", [id(2)])
        .unwrap();
    s.set_status(&[id(3)], "archived").unwrap();
    s.delete(&[id(4)]).unwrap();
    s.supersede(&id(5), &id(6)).unwrap();
    let st = hybrid::status(&s, &model("one")).unwrap();
    assert!(st.contains("待建或过期 2，因敏感内容跳过 1"));
    let r = index("one", None);
    assert_eq!((r.updated, r.removed, count(&s)), (2, 3, 3));
    assert_eq!(index("two", None).updated, 3);
    assert_eq!(index("two", None).updated, 0);
    hybrid::disable(&s, false).unwrap();
    assert_eq!(count(&s), 3);
    hybrid::disable(&s, true).unwrap();
    assert_eq!((count(&s), s.schema_version().unwrap()), (0, pre));
}

#[test]
fn consent_scopes_each_origin_and_recall_backfills_only_batch_minus_one() {
    let (_dir, loc, s) = project();
    let sink = trap();
    let other = port(&sink);
    for n in 1..=3 {
        record(&s, n, &format!("主题{n}"), "知识");
    }
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let srv = Server::new(move |sock, r| {
        assert_eq!(r.route, "POST /v1/embeddings");
        seen.lock().unwrap().push(r.body["input"].clone());
        reply(sock, 200, &embeddings(&r.body, |_| vec![1.]).to_string());
    });
    let m = || configured(srv.port, "one", 2, None);
    let query = q("查询");
    let reader = loc.open(false).unwrap();
    assert!(
        hybrid::recall(&reader, &query, || panic!("未同意不应加载配置"))
            .unwrap()
            .heading
            .is_empty()
    );
    hybrid::enable(&s, &m()).unwrap();
    let moved = || configured(other, "one", 2, None);
    let r = hybrid::recall(&reader, &query, moved).unwrap();
    assert!(r.heading.contains("仅关键词（向量不可用：模型地址已变更"));
    assert!(
        hybrid::status(&reader, &moved())
            .unwrap()
            .contains("需重新执行 loom semantic enable")
    );
    assert!(
        hybrid::recall(&reader, &q(" "), || panic!())
            .unwrap()
            .heading
            .is_empty()
    );
    assert!(
        hybrid::recall(&reader, &q(&"字".repeat(200)), m)
            .unwrap()
            .heading
            .contains("超限")
    );
    let r = hybrid::recall(&reader, &query, || {
        configured(srv.port, "one", 2, Some(other))
    })
    .unwrap();
    assert!(
        r.heading
            .starts_with("检索方式：关键词 + 向量（重排不可用：模型地址已变更")
    );
    assert!(r.heading.contains("另有 2 条经验尚未建向量"));
    assert_eq!(ids(&r), [id(1)]);
    assert_eq!(count(&s), 1);
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        &[json!(["查询", "主题1\n知识"])]
    );
    hybrid::disable(&s, false).unwrap();
    assert!(
        hybrid::recall(&reader, &query, || panic!())
            .unwrap()
            .heading
            .is_empty()
    );
    unused(&sink);
}
