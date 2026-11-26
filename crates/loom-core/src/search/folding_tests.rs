//! 真实 SQLite/FTS 夹具；直接插入可保留写入去重前的旧行，不改写业务数据。
use super::*;
use serde_json::json;

const TITLE: &str = "核对项目恢复时的写入范围";
const BODY: &str = "只有核实当前项目后才允许写入，并保留原始记录。";

fn id(n: usize) -> String {
    ulid::Ulid::from(n as u128).to_string()
}

fn put(s: &Store, n: usize, title: &str, body: &str, importance: i64) {
    s.conn
        .execute(
            "INSERT INTO memories(id,type,title,body,importance,content_hash,created_at,updated_at)
         VALUES (?1,'DECISION',?2,?3,?4,?1,1,1)",
            params![id(n), title, body, importance],
        )
        .unwrap();
}

fn recall(s: &Store, query: &str, top_k: usize) -> Vec<Hit> {
    s.recall(&RecallQuery {
        query: query.into(),
        top_k,
        include_archived: true,
        ..Default::default()
    })
    .unwrap()
}

fn pair() -> Store {
    let s = Store::open_in_memory().unwrap();
    put(&s, 1, TITLE, BODY, 5);
    put(&s, 2, &format!("{TITLE}。"), BODY, 4);
    s
}

fn anchor(s: &Store, n: usize, a: (&str, &str, Option<&str>)) {
    s.conn
        .execute(
            "INSERT INTO anchors VALUES (?1,?2,?3,?4)",
            params![id(n), a.0, a.1, a.2],
        )
        .unwrap();
}

#[test]
fn ranks_then_folds_then_takes_top_k_and_preserves_each_original() {
    let s = pair();
    put(
        &s,
        3,
        "核对独立的只读备份",
        "只读备份与写入授权是两件不同的事。",
        2,
    );
    put(&s, 4, TITLE, BODY, 1); // 在第二个代表之后，仍属于第一个代表。
    let hits = recall(&s, "核", 2);
    assert_eq!(
        hits.iter().map(|h| &h.memory.id).collect::<Vec<_>>(),
        [&id(1), &id(3)]
    );
    let wire = serde_json::to_value(&hits[0]).unwrap();
    assert_eq!(wire["folded_ids"], json!([id(2), id(4)]));
    assert_eq!(wire["folded_count"], 2);
    assert_eq!(s.count().unwrap(), 4);
    let old = s.get(&id(2)).unwrap().unwrap();
    assert_eq!((old.title, old.body), (format!("{TITLE}。"), BODY.to_owned()));
}

#[test]
fn equal_scores_use_stable_ids_not_hashmap_or_insertion_order() {
    let s = Store::open_in_memory().unwrap();
    for (n, title) in [
        (3, "核对  项目范围。"),
        (2, "核对 项目范围"),
        (1, "核对 项目范围"),
    ] {
        put(&s, n, title, BODY, 3);
    }
    for query in ["", "核", "项目范围"] {
        // FTS 相关度可以不同，但重复查询必须一致。短词/空查询分数严格相等。
        let first = recall(&s, query, 1);
        let repeated = recall(&s, query, 1);
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(repeated).unwrap()
        );
        assert_eq!(serde_json::to_value(&first[0]).unwrap()["folded_count"], 2);
        if query != "项目范围" {
            assert_eq!(first[0].memory.id, id(1));
        }
    }
}

#[test]
fn similar_words_are_not_proof_of_identical_meaning() {
    let cases = [
        ("核对是否允许默认写入", "核对是否不允许默认写入"),
        ("核对若已确认才允许写入", "核对若未确认才允许写入"),
        ("核对默认等待 10 秒", "核对默认等待 100 秒"),
        ("核对默认等待 -10 秒", "核对默认等待 10 秒"),
        ("核对默认阈值 1 2", "核对默认阈值 12"),
        (r"核对 C:\src\a.rs", r"核对 C:\src\ars"),
        (r"核对 C:\Program  Files\a", r"核对 C:\Program Files\a"),
        (r"核对 \\?\C:\src\file.", r"核对 \\?\C:\src\file"),
        ("核对写入前需 a!=b", "核对写入前需 a=b"),
        ("核对命令 git --no-edit", "核对命令 git --noedit"),
        ("核对字符串 'a  b'", "核对字符串 'a b'"),
        ("核对程序中的 C++", "核对程序中的 C"),
        ("核对程序中的 a::b", "核对程序中的 a.b"),
        ("核对 API 标识 Foo", "核对 API 标识 foo"),
        ("核对权限 now here", "核对权限 nowhere"),
        ("核对乒乓球 拍卖完了", "核对乒乓球拍 卖完了"),
    ];
    for (a, b) in cases {
        let s = Store::open_in_memory().unwrap();
        put(&s, 1, a, BODY, 3);
        put(&s, 2, b, BODY, 3);
        assert_eq!(recall(&s, "核", 2).len(), 2, "{a} / {b}");
    }
    for (a, b) in [
        (BODY, "只有核实当前项目后才禁止写入，并保留原始记录。"),
        ("遇到 10 次失败后回滚。", "遇到 11 次失败后回滚。"),
        ("if ready:\n    save()", "if ready:\nsave()"),
        ("", ""),
        (" \n", " \n"),
        ("........", "........"),
    ] {
        let s = Store::open_in_memory().unwrap();
        put(&s, 1, TITLE, a, 3);
        put(&s, 2, TITLE, b, 3);
        assert_eq!(recall(&s, "核", 2).len(), 2, "正文 {a:?} / {b:?}");
    }
}

#[test]
fn provenance_status_type_tags_and_structured_baselines_are_boundaries() {
    for change in [
        "origin='cli'",
        "status='archived'",
        "status='superseded'",
        "type='GOTCHA'",
        "tags='[\"离线\"]'",
        "legacy_id='page-b'",
    ] {
        let s = pair();
        s.conn
            .execute(&format!("UPDATE memories SET {change} WHERE id=?1"), [id(2)])
            .unwrap();
        assert_eq!(recall(&s, "核", 2).len(), 2, "{change}");
    }
    let s = pair();
    for n in [1, 2] {
        s.conn
            .execute(
                "UPDATE memories SET origin='import:oh-memos',legacy_id=?2 WHERE id=?1",
                params![id(n), format!("wiki-{n}")],
            )
            .unwrap();
    }
    assert_eq!(recall(&s, "核", 2).len(), 2);
    assert_eq!(s.get("wiki-2").unwrap().unwrap().id, id(2));

    let a = ("src/a.rs", "run", Some("then"));
    for (left, right) in [
        (a, ("src/b.rs", "run", a.2)),
        (a, (a.0, "stop", a.2)),
        (a, (a.0, a.1, Some("later"))),
        (a, (a.0, a.1, None)),
        (("src/a#b", "c", a.2), ("src/a", "b#c", a.2)),
    ] {
        let s = pair();
        anchor(&s, 1, left);
        anchor(&s, 2, right);
        assert_eq!(recall(&s, "核", 2).len(), 2, "{left:?} / {right:?}");
    }
    let s = pair();
    anchor(&s, 1, a);
    anchor(&s, 2, a);
    assert_eq!(recall(&s, "核", 2).len(), 1, "相同的已存锚点不应阻止折叠");
}

#[test]
fn old_rows_with_an_unreadable_type_or_tag_list_do_not_prove_equality() {
    for change in ["type='UNRECOGNIZED'", "tags='not-an-array'"] {
        let s = pair();
        s.conn
            .execute("UPDATE memories SET type='SYNTHESIS'", [])
            .unwrap();
        s.conn
            .execute(&format!("UPDATE memories SET {change} WHERE id=?1"), [id(2)])
            .unwrap();
        assert_eq!(recall(&s, "核", 2).len(), 2, "{change}");
    }
}

#[test]
fn declared_evidence_is_never_hidden_behind_a_similar_title() {
    use crate::project::{Bind, ProjectLocation};
    let dir = tempfile::tempdir().unwrap();
    let loc = ProjectLocation::select(dir.path(), Some(dir.path()), None).unwrap();
    loc.bind(Bind::Initialize).unwrap();
    let s = loc.open(true).unwrap();
    put(&s, 1, TITLE, BODY, 5);
    put(&s, 2, &format!("{TITLE}。"), BODY, 4);
    put(&s, 3, "另行人工复核", "只认人工登记的冲突声明。", 3);
    s.evidence_link(&id(3), &id(2), "conflicts").unwrap();
    assert_eq!(recall(&s, "核", 3).len(), 3);
}

#[test]
fn caps_listed_ids_without_spending_extra_slots_or_reading_past_the_pool() {
    let s = Store::open_in_memory().unwrap();
    for n in 1..=49 {
        put(&s, n, TITLE, BODY, 5);
    }
    put(
        &s,
        50,
        "核对独立的验收",
        "这是独立的结论，不应被同组重复项挤掉。",
        2,
    );
    put(&s, 51, "核对更早的结论", "在原有五十条候选窗口之外。", 5);
    s.conn
        .execute("UPDATE memories SET updated_at=0 WHERE id=?1", [id(51)])
        .unwrap();
    let hits = recall(&s, "", 2);
    assert_eq!(
        hits.iter().map(|h| &h.memory.id).collect::<Vec<_>>(),
        [&id(1), &id(50)]
    );
    let wire = serde_json::to_value(&hits[0]).unwrap();
    assert_eq!(wire["folded_count"], 48);
    assert_eq!(wire["folded_ids"], json!((2..=9).map(id).collect::<Vec<_>>()));
    assert!(s.get(&id(49)).unwrap().is_some());

    // 超过候选池，且打分与更新时间全部相同。三种入口都不能由插入顺序抢走窗口。
    let s = Store::open_in_memory().unwrap();
    for n in (1..=55).rev() {
        put(&s, n, TITLE, BODY, 5);
    }
    for query in ["", "核", "项目恢复"] {
        let hits = recall(&s, query, 1);
        assert_eq!(hits[0].memory.id, id(1), "{query:?}");
        assert_eq!(hits[0].folded_count, 49, "只能折叠窗口内的项：{query:?}");
    }
}
