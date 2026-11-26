use super::*;
use crate::{
    MemoryType, NewMemory,
    memory::RememberOutcome,
    project::{Bind, ProjectLocation},
    util::sha256_hex,
};
use std::fs;
use tempfile::TempDir;

const TARGET: &str = "target.ts";
const SRC: &str = "export const target = 1;";
const ROLE: &str = "只负责本地持久化，不发网络请求。";

fn fixture() -> Result<(TempDir, ProjectLocation, Store)> {
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join(TARGET), SRC)?;
    fs::write(dir.path().join("entry.ts"), "import './target.ts';")?;
    let p = ProjectLocation::select(dir.path(), Some(dir.path()), None)?;
    p.bind(Bind::Initialize)?;
    let s = p.open(true)?;
    Ok((dir, p, s))
}

fn request() -> ContextRequest {
    ContextRequest {
        mode: Mode::Impact,
        target: TARGET.into(),
        budget: DEFAULT_BUDGET,
        depth: 1,
        question: None,
    }
}

fn rule(s: &Store, kind: &str, title: &str) -> Result<String> {
    let id = s.intent_propose(kind, title, "不得自动生效。")?;
    s.intent_confirm(&id)?;
    Ok(id)
}

fn memory(s: &Store, title: &str, anchored: bool) -> Result<String> {
    let outcome = s.remember(NewMemory {
        mem_type: Some(MemoryType::Gotcha),
        title: title.into(),
        body: "只有事务已经回滚时，才允许重试。".into(),
        anchors: if anchored {
            vec![format!("{TARGET}#write"), TARGET.into()]
        } else {
            vec![]
        },
        bypass_admission: true,
        force: true,
        ..Default::default()
    })?;
    let RememberOutcome::Created { id, .. } = outcome else {
        panic!("{outcome:?}")
    };
    Ok(id)
}

#[test]
fn default_budget_delivers_all_four_sources() -> Result<()> {
    let (_d, p, s) = fixture()?;
    s.index_structure()?;
    s.note_set(TARGET, &sha256_hex(SRC.as_bytes()), ROLE)?;
    let id = rule(&s, "constraint", "只能离线写入")?;
    let mem = memory(&s, "锁冲突时先退避", true)?;
    let r = p.open(false)?.context(request())?;
    assert!(r.bytes <= 1500 && r.bytes == r.text.len());
    let mut lines = r.text.as_str();
    for part in [
        s.project().unwrap().id(),
        &format!("红线 [{id}] 只能离线写入"),
        "结构：快照第 1 代",
        "entry.ts → target.ts · 第 1 行 · 静态确定 · 第 1 跳",
        "职责与经验",
        ROLE,
        "经验 [GOTCHA] 锁冲突时先退避",
        &format!("id={mem} · 锚定本文件"),
        "缺口",
        "续查：impact target.ts --depth 2",
    ] {
        lines = lines
            .split_once(part)
            .unwrap_or_else(|| panic!("缺 {part}: {}", r.text))
            .1;
    }
    // 没给 question、按文件名也没有别的命中：不算缺口，也不劝人再查一遍同样的词。
    assert!(!r.text.contains("question") && !r.text.contains("recall "));
    Ok(())
}

#[test]
fn byte_floor_and_whole_sanitized_lines() -> Result<()> {
    let (_d, _p, s) = fixture()?;
    let long = "边界不可破坏".repeat(150);
    let secret = "abcdef".repeat(12);
    let raw = format!("中文\"引号\"\nBearer {secret} 不得外传");
    for i in 0..67 {
        let id = rule(
            &s,
            "constraint",
            &format!("约束 {i:02}，不能在核对之前动手"),
        )?;
        if i < 2 {
            s.conn.execute(
                "UPDATE intents SET title=?1,updated_at=updated_at+?2 WHERE id=?3",
                rusqlite::params![if i == 0 { &long } else { &raw }, 200 - i, id],
            )?;
        }
    }
    let mem = memory(&s, &"超长中文标题".repeat(60), true)?;
    let mut req = request();
    req.budget = 1;
    let error = s.context(req.clone()).unwrap_err().to_string();
    let min: i64 = error
        .strip_prefix("预算 1 字节不够，这次至少需要 ")
        .unwrap()
        .strip_suffix(" 字节")
        .unwrap()
        .parse()?;
    req.budget = min - 1;
    assert!(s.context(req.clone()).is_err());
    let safe = "中文\"引号\" Bearer *** 不得外传";
    for budget in [min, min + 1, 1500, 6144] {
        req.budget = budget;
        let r = s.context(req.clone())?;
        assert_eq!(r.bytes, r.text.len());
        assert!(r.bytes <= budget as usize && !r.text.contains(&secret));
        assert!(r.text.contains("还有约束未展示，请先查看") && r.text.contains("续查："));
        assert_eq!(r.omitted.retrieval["constraints"], 3);
        let rules: Vec<_> = r.text.lines().filter(|l| l.starts_with("红线 [")).collect();
        assert_eq!(
            rules.len() + r.omitted.budget.get("constraints").unwrap_or(&0),
            64
        );
        assert!(
            rules.iter().all(|l| l.ends_with(&long)
                || l.ends_with(safe)
                || l.ends_with("不能在核对之前动手"))
        );
        if budget == 1500 {
            assert!(r.text.contains(safe) && !r.text.contains("边界不可破坏"));
            assert!(r.text.contains("（长标题已省略）") && r.text.contains(&mem));
        }
    }
    Ok(())
}

#[test]
fn changes_history_lifecycle_and_partial_failure() -> Result<()> {
    let (d, _p, s) = fixture()?;
    s.index_structure()?;
    s.note_set(TARGET, &sha256_hex(SRC.as_bytes()), ROLE)?;
    let active = memory(&s, "保留经验", true)?;
    let old = memory(&s, "target 已归档经验", true)?;
    s.set_status(std::slice::from_ref(&old), "archived")?;
    let revoked = rule(&s, "constraint", "已撤销约束")?;
    s.intent_revoke(&revoked)?;
    let keep = rule(&s, "non_goal", "不支持在线存储")?;
    fs::write(d.path().join(TARGET), "export const target = 2;")?;
    fs::write(d.path().join("entry.ts"), "")?;
    let read = || {
        s.context(ContextRequest {
            budget: 6144,
            ..request()
        })
    };
    let r = read()?;
    for part in [
        "已变化：target.ts、entry.ts",
        "历史职责（需复核）",
        ROLE,
        &active,
        "⚠code_drift",
        "源码已变",
        &keep,
    ] {
        assert!(r.text.contains(part), "{part}: {}", r.text);
    }
    assert!(!r.text.contains(&old) && !r.text.contains(&revoked));
    fs::remove_file(d.path().join(TARGET))?;
    let missing = read()?;
    assert!(
        missing.text.contains("已消失：target.ts")
            && missing.text.contains("职责：文件消失")
            && missing.text.contains("⚠orphaned")
    );
    s.conn.execute_batch(
        "ALTER TABLE project_file_roles RENAME COLUMN role TO missing_role; DROP TABLE fts;",
    )?;
    let r = read()?;
    assert!(r.gap_codes.contains(&"role_failed") && r.gap_codes.contains(&"keywords_failed"));
    for part in [
        "职责未取到：",
        "no such column: role",
        "关键词经验未取到：",
        "entry.ts → target.ts",
        &active,
        &keep,
    ] {
        assert!(r.text.contains(part), "{}", r.text);
    }
    assert!(!r.text.contains("未写职责") && !r.text.contains("没有命中经验"));
    // 没有锚定的经验、关键词又没取到时，不能写成“没有”。
    let other = s.context(ContextRequest {
        target: "entry.ts".into(),
        budget: 6144,
        ..request()
    })?;
    assert!(other.text.contains("经验：未取全") && !other.text.contains("也没查到"));
    Ok(())
}

#[test]
fn confidence_groups_exclude_proposals() -> Result<()> {
    let (d, _p, s) = fixture()?;
    fs::write(d.path().join("aaa.ts"), "import('./target');")?;
    s.index_structure()?;
    let id = s.intent_propose("constraint", "不要把提议当红线", "尚未批准。")?;
    let r = s.context(request())?;
    assert!(!r.text.contains(&id) && !r.text.contains("不要把提议当红线"));
    assert!(r.text.contains("静态确定（1 条）\nentry.ts → target.ts · 第 1 行 · 静态确定 · 第 1 跳\n可能（1 条）\naaa.ts → target.ts · 第 1 行 · 可能 · 第 1 跳"));
    Ok(())
}

#[test]
fn readonly_identity_validation_and_question_leave_no_trace() -> Result<()> {
    let (dir, p, writer) = fixture()?;
    let s = p.open(false)?;
    let version = || {
        writer
            .conn
            .pragma_query_value(None, "data_version", |r| r.get::<_, i64>(0))
    };
    let before = version()?;
    let marker = "UNPERSISTED_U3_0123456789";
    let req = ContextRequest {
        question: Some(marker.into()),
        ..request()
    };
    let r = s.context(req.clone())?;
    assert!(r.gap_codes.contains(&"not_indexed") && r.gap_codes.contains(&"keyword_miss"));
    assert!(r.text.contains("按 question 也没查到") && r.text.contains("它按空格分词"));
    assert_eq!(version()?, before);
    for entry in fs::read_dir(p.database().parent().unwrap())? {
        let bytes = fs::read(entry?.path())?;
        assert!(!bytes.windows(marker.len()).any(|w| w == marker.as_bytes()));
    }
    for budget in [-1, 0, 6145] {
        assert!(
            s.context(ContextRequest {
                budget,
                ..req.clone()
            })
            .unwrap_err()
            .to_string()
            .contains("1 至 6144")
        );
    }
    for depth in [0, 4] {
        assert!(
            s.context(ContextRequest {
                depth,
                ..req.clone()
            })
            .is_err()
        );
    }
    assert!(
        s.context(ContextRequest {
            question: Some("词".repeat(171)),
            ..req.clone()
        })
        .is_err()
    );
    for target in [
        "../a.ts",
        "C:/a.ts",
        "a\nb.ts",
        "sk-12345678901234567890.ts",
    ] {
        assert!(
            s.context(ContextRequest {
                target: target.into(),
                ..req.clone()
            })
            .is_err()
        );
    }
    fs::write(dir.path().join(".loom/project.id"), crate::util::new_id())?;
    assert!(s.context(req).unwrap_err().to_string().contains("身份不符"));
    Ok(())
}

#[test]
fn anchor_and_keyword_windows_are_separate_after_deduplication() -> Result<()> {
    let (_d, _p, s) = fixture()?;
    for i in 0..7 {
        memory(&s, &format!("target 锚定记录 {i}"), true)?;
        memory(&s, &format!("target.ts 补充记录 {i}"), false)?;
    }
    let r = s.context(ContextRequest {
        budget: 6144,
        ..request()
    })?;
    let titles: Vec<_> = r.text.lines().filter(|s| s.starts_with("经验 [")).collect();
    assert_eq!(titles.len(), 10);
    assert!(titles[..5].iter().all(|l| l.ends_with("锚定本文件")));
    assert!(
        titles[5..]
            .iter()
            .all(|l| l.contains("补充记录") && l.ends_with("提到本文件名"))
    );
    assert_eq!(r.omitted.retrieval["anchors"], 2);
    assert_eq!(r.omitted.retrieval["keywords"], 2);
    assert!(r.omitted.budget.is_empty());
    // 还有没展示的命中，才值得用同样的词再查。
    assert!(r.text.contains("；recall target.ts；"));
    Ok(())
}
