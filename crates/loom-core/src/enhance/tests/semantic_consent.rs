use super::*;
use crate::enhance::semantic::{allow, deny, find_with, list};

// 冻结跨措辞反例及其相关结果。mock 向量只证明管道，不估算真实模型质量。
const QUESTION: &str = "如何找回上次做到一半的活儿？";
const TARGET: &str = "任务画布恢复现场";

#[test]
fn zero_lexical_hits_still_searches_only_consented_active_titles() {
    let (dir, loc, s) = project();
    let (_foreign, _, other) = project();
    for (n, title) in [
        (1, "颜色配置"),
        (2, TARGET),
        (3, "未授权标题"),
        (4, "归档标题"),
    ] {
        record(
            &s,
            n,
            &format!("{title} token={}", "synthetic-value"),
            PRIVATE_BODY,
        );
    }
    record(&other, 5, "另一项目私有标题", PRIVATE_BODY);
    allow(&other, &id(5)).unwrap();
    assert!(allow(&s, &id(5)).is_err());
    for n in [1, 2, 4] {
        allow(&s, &id(n)).unwrap();
    }
    s.set_status(&[id(4)], "archived").unwrap();
    s.conn
        .execute(
            "INSERT INTO anchors(memory_id,path) VALUES(?1,'src/private-file.rs')",
            [id(2)],
        )
        .unwrap();
    let reader = loc.open(false).unwrap();
    assert!(
        reader
            .recall(&crate::search::RecallQuery {
                query: QUESTION.into(),
                ..Default::default()
            })
            .unwrap()
            .is_empty()
    );
    let off = find_with(&reader, QUESTION, Model::Off, WAIT).unwrap();
    assert!(off.contains("关键词回退") && !off.contains(&id(2)));
    let on = with_mock(
        |stream, req| {
            assert!(req.head.starts_with("POST /api/embed "));
            assert_eq!(
                req.json,
                json!({"model":"mock-test", "input":[format!("{QUESTION} token=***"), "颜色配置 token=***", format!("{TARGET} token=***")]})
            );
            json_reply(stream, json!({"embeddings":[[1,0],[0,1],[1,0]]}));
        },
        |p| {
            find_with(
                &reader,
                &format!("{QUESTION} token={}", "synthetic-query"),
                model(p),
                WAIT,
            )
            .unwrap()
        },
    );
    assert!(
        on.contains("授权集") && on.contains("来源") && on.contains(&id(2)) && on.contains(TARGET)
    );
    let exact = with_mock(
        |stream, req| {
            assert_eq!(req.json["input"][0], QUESTION);
            let vectors: Vec<_> = req.json["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| {
                    let s = v.as_str().unwrap();
                    if s == QUESTION || s.starts_with(TARGET) {
                        [1, 0]
                    } else {
                        [0, 1]
                    }
                })
                .collect();
            json_reply(stream, json!({"embeddings":vectors}));
        },
        |p| find_with(&reader, QUESTION, model(p), WAIT).unwrap(),
    );
    assert_eq!(on, exact);
    for omitted in [
        id(1),
        id(3),
        id(4),
        id(5),
        "归档标题".into(),
        "未授权标题".into(),
        "另一项目".into(),
        PRIVATE_BODY.into(),
        "synthetic-".into(),
        "private-file.rs".into(),
        dir.path().display().to_string(),
    ] {
        assert!(!on.contains(&omitted));
    }
    assert_eq!(reader.conn.total_changes(), 0);
    assert_eq!(
        reader
            .conn
            .query_row("SELECT count(*) FROM vectors", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn consent_is_local_bounded_and_never_revives_on_restore() {
    let (dir, loc, s) = project();
    let reader = loc.open(false).unwrap();
    let trap = trap();
    assert!(list(&reader).unwrap().contains("无授权"));
    assert!(
        find_with(&reader, QUESTION, model(port(&trap)), WAIT)
            .unwrap()
            .contains("无授权")
    );
    unused(&trap);
    let schema = || {
        s.conn
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name LIKE 'semantic_%'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
    };
    assert_eq!(schema(), 0);
    assert!(allow(&s, "not-an-id").is_err() && allow(&s, &id(133)).is_err());
    assert_eq!(schema(), 0);
    for n in 1..=13 {
        record(&s, n, &format!("记录-{n}"), PRIVATE_BODY);
    }
    for n in 1..=12 {
        allow(&s, &id(n)).unwrap();
    }
    allow(&s, &id(12)).unwrap();
    let original = list(&reader).unwrap();
    assert!(allow(&s, &id(13)).unwrap_err().to_string().contains("12"));
    assert_eq!(list(&reader).unwrap(), original);
    s.set_status(&[id(1)], "archived").unwrap();
    assert!(allow(&s, &id(1)).is_err());
    s.set_status(&[id(1)], "active").unwrap();
    s.supersede(&id(2), &id(13)).unwrap();
    s.set_status(&[id(2)], "active").unwrap();
    deny(&s, &id(3)).unwrap();
    s.delete(&[id(4)]).unwrap();
    allow(&s, &id(13)).unwrap();
    let after = list(&reader).unwrap();
    for n in 1..=4 {
        assert!(!after.contains(&id(n)));
    }
    assert!(after.contains(&id(13)));
    std::fs::write(dir.path().join(".loom/project.id"), id(50)).unwrap();
    assert!(list(&reader).is_err() && deny(&s, &id(5)).is_err() && allow(&s, &id(1)).is_err());
    assert!(find_with(&reader, QUESTION, model(port(&trap)), WAIT).is_err());
    unused(&trap);
}

#[test]
fn fallback_searches_inside_the_set_before_limiting_not_after_global_recall() {
    let (_dir, _loc, s) = project();
    for n in 1..=80 {
        record(&s, n, &format!("needle needle needle 集外-{n}"), "needle");
    }
    record(
        &s,
        81,
        "集内记录",
        "only the body has needle; never return this sentence",
    );
    record(&s, 82, "其他授权标题", PRIVATE_BODY);
    allow(&s, &id(81)).unwrap();
    allow(&s, &id(82)).unwrap();
    let global = s
        .recall(&crate::search::RecallQuery {
            query: "needle".into(),
            top_k: 50,
            ..Default::default()
        })
        .unwrap();
    assert!(!global.iter().any(|h| h.memory.id == id(81)));
    let off = find_with(&s, "needle", Model::Off, WAIT).unwrap();
    let bad = with_mock(
        |stream, req| {
            assert_eq!(
                req.json["input"],
                json!(["needle", "集内记录", "其他授权标题"])
            );
            json_reply(stream, json!({"embeddings":[[1,0],[1],[1,0]]}));
        },
        |p| find_with(&s, "needle", model(p), WAIT).unwrap(),
    );
    for result in [off, bad] {
        assert!(
            result.contains("关键词回退")
                && result.contains(&id(81))
                && !result.contains(&id(82))
                && !result.contains("集外")
                && !result.contains("never return")
        );
    }
}

#[test]
fn changes_during_model_wait_discard_stale_results_without_holding_writer() {
    for change in ["deny", "archive", "retitle"] {
        let (_dir, loc, s) = project();
        record(&s, 1, TARGET, PRIVATE_BODY);
        allow(&s, &id(1)).unwrap();
        let reader = loc.open(false).unwrap();
        let writer = loc.open(true).unwrap();
        let result = with_mock(
            move |stream, req| {
                assert_eq!(req.json["input"], json!([QUESTION, TARGET]));
                writer.conn.busy_timeout(Duration::ZERO).unwrap();
                match change {
                    "deny" => {
                        deny(&writer, &id(1)).unwrap();
                    }
                    "archive" => {
                        writer.set_status(&[id(1)], "archived").unwrap();
                    }
                    _ => {
                        writer
                            .conn
                            .execute("UPDATE memories SET title='替换记录' WHERE id=?1", [id(1)])
                            .unwrap();
                    }
                }
                json_reply(stream, json!({"embeddings":[[1],[1]]}));
            },
            |p| find_with(&reader, QUESTION, model(p), WAIT).unwrap(),
        );
        assert!(!result.contains(&id(1)) && !result.contains(TARGET) && result.contains("变化"));
    }
}
