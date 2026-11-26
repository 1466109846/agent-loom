use std::{fs, path::Path};

use anyhow::Result;
use rusqlite::{Connection, params};
use tempfile::{TempDir, tempdir};

use crate::db::{SCHEMA_V1, SCHEMA_VERSION};
use crate::memory::RememberOutcome;
use crate::project::{Bind, ProjectLocation};
use crate::{MemoryType, NewMemory, Store, util};

fn location(root: &Path) -> Result<ProjectLocation> {
    ProjectLocation::select(root, Some(root), None)
}

fn project() -> Result<(TempDir, ProjectLocation, Store)> {
    let dir = tempdir()?;
    let loc = location(dir.path())?;
    loc.bind(Bind::Initialize)?;
    let s = loc.open(true)?;
    Ok((dir, loc, s))
}

fn memory(s: &Store, title: &str, importance: i64) -> Result<String> {
    let outcome = s.remember(NewMemory {
        mem_type: Some(MemoryType::Gotcha),
        title: title.into(),
        body: format!("这里是仅按需读取的经验正文：{title}"),
        importance: Some(importance),
        force: true,
        bypass_admission: true,
        ..Default::default()
    })?;
    let RememberOutcome::Created { id, .. } = outcome else {
        panic!("{outcome:?}")
    };
    Ok(id)
}

#[test]
fn only_effective_intents_and_explicit_pins_survive_reopen() -> Result<()> {
    let (_dir, loc, s) = project()?;
    let rule = s.intent_propose("constraint", "离线也能写入", "红线的完整依据仅按需读")?;
    let goal = s.intent_propose("goal", "交付本地库", "目标的完整依据仅按需读")?;
    let boundary = s.intent_propose("non_goal", "不做跨项目汇总", "非目标的完整依据")?;
    let decision = s.intent_propose("decision", "内部选型原因", "完整决策理由")?;
    let pending = s.intent_propose("constraint", "还在讨论的规则", "未经核对的依据")?;
    let revoked = s.intent_propose("goal", "已取消的方向", "后来改变了决定")?;
    let high = memory(&s, "重要但未置顶", 5)?;
    let pin = memory(&s, "明确置顶的低分经验", 1)?;
    let before = s.overview_text(800)?;
    assert!(before.contains("尚未确认"));
    for id in [&rule, &goal, &high, &pin] {
        assert!(!before.contains(id));
    }
    for id in [&rule, &goal, &boundary, &decision, &revoked] {
        s.intent_confirm(id)?;
    }
    s.intent_revoke(&revoked)?;
    s.pin_memory(&pin, true)?;
    // 日期只用于稳定排序，不能作红线的生效窗口。
    s.conn.execute(
        "UPDATE intents SET updated_at=?1 WHERE id=?2",
        params![util::now_ts() - 90 * 86_400, rule],
    )?;
    drop(s);
    let ro = loc.open(false)?;
    let text = ro.overview_text(800)?;
    assert!(text.len() <= 800);
    assert_eq!(
        text.lines().next().unwrap(),
        ro.project().unwrap().heading()
    );
    for (id, title) in [
        (&rule, "离线也能写入"),
        (&goal, "交付本地库"),
        (&boundary, "不做跨项目汇总"),
    ] {
        assert_eq!(text.matches(id).count(), 1, "{text}");
        assert!(text.contains(title));
        assert!(text.find(id) < text.find(&pin));
    }
    assert!(text.contains(&pin));
    for hidden in [&decision, &pending, &revoked, &high] {
        assert!(!text.contains(hidden));
    }
    for hidden in [
        "完整依据",
        "仅按需读取的经验正文",
        "重要但未置顶",
        "内部选型原因",
    ] {
        assert!(!text.contains(hidden));
    }
    assert!(ro.intent_get(&decision)?.contains("完整决策理由"));
    assert!(!ro.intent_list()?.contains("完整决策理由"));
    Ok(())
}

#[test]
fn transitions_are_conditional_and_replacement_history_is_bidirectional() -> Result<()> {
    let (_dir, _loc, s) = project()?;
    let a = s.intent_propose("goal", "第一版", "原始理由")?;
    let b = s.intent_propose("goal", "第二版", "替代理由")?;
    let c = s.intent_propose("goal", "第三版", "最新理由")?;
    assert!(s.intent_replace(&a, &b).is_err());
    assert!(s.intent_revoke(&a).is_err());
    s.intent_confirm(&a)?;
    assert!(s.intent_confirm(&a).is_err());
    assert!(s.intent_replace(&a, &a).is_err());
    assert!(s.intent_replace(&a, "missing").is_err());
    s.intent_confirm(&c)?;
    assert!(s.intent_replace(&a, &c).is_err());
    assert!(s.overview_text(800)?.contains(&a));
    s.intent_replace(&a, &b)?;
    assert!(s.intent_get(&a)?.contains(&b));
    assert!(s.intent_get(&b)?.contains(&a));
    assert!(s.intent_get(&a)?.contains("原始理由"));
    assert!(s.intent_get(&a)?.contains("已取代"));
    assert!(s.intent_confirm(&b).is_err());
    assert!(s.intent_confirm(&a).is_err());
    assert!(s.intent_replace(&a, &b).is_err());
    assert!(s.intent_revoke(&a).is_err());
    let d = s.intent_propose("goal", "第四版", "继续更替")?;
    s.intent_replace(&b, &d)?;
    let middle = s.intent_get(&b)?;
    assert!(middle.contains(&a) && middle.contains(&d));
    s.intent_revoke(&d)?;
    assert!(s.intent_revoke(&d).is_err());
    assert!(s.intent_confirm(&d).is_err());
    assert!(s.intent_get(&d)?.contains("已撤销"));
    let overview = s.overview_text(800)?;
    assert!(overview.contains(&c));
    for inactive in [&a, &b, &d] {
        assert!(!overview.contains(inactive));
    }
    Ok(())
}

#[test]
fn a_late_replacement_failure_leaves_both_records_and_history_untouched() -> Result<()> {
    let (_dir, _loc, s) = project()?;
    let a = s.intent_propose("constraint", "原红线", "原依据")?;
    let b = s.intent_propose("constraint", "拟替换红线", "新依据")?;
    s.intent_confirm(&a)?;
    let before = (s.intent_get(&a)?, s.intent_get(&b)?);
    // 分别拒绝两端的更新，不依赖实现先写哪一端。
    for title in ["原红线", "拟替换红线"] {
        s.conn.execute_batch(&format!("CREATE TEMP TRIGGER fail_update AFTER UPDATE ON intents WHEN NEW.title='{title}' BEGIN SELECT RAISE(ABORT, 'fixture'); END;"))?;
        assert!(s.intent_replace(&a, &b).is_err());
        assert_eq!((s.intent_get(&a)?, s.intent_get(&b)?), before);
        assert!(s.overview_text(800)?.contains(&a));
        assert!(!s.overview_text(800)?.contains(&b));
        s.conn.execute_batch("DROP TRIGGER fail_update")?;
    }
    s.intent_replace(&a, &b)?;
    Ok(())
}

#[test]
fn inactive_or_foreign_memory_cannot_be_presented_as_a_pin() -> Result<()> {
    let (_dir, _loc, s) = project()?;
    let (_other_dir, _other_loc, other) = project()?;
    let id = memory(&s, "手工置顶", 2)?;
    assert!(other.pin_memory(&id, true).is_err());
    assert!(!other.overview_text(800)?.contains(&id));
    s.pin_memory(&id, true)?;
    let _ = s.pin_memory(&id, true);
    assert_eq!(s.overview_text(800)?.matches(&id).count(), 1);
    s.pin_memory(&id, false)?;
    assert!(!s.overview_text(800)?.contains(&id));
    for status in ["archived", "superseded"] {
        s.pin_memory(&id, true)?;
        s.set_status(std::slice::from_ref(&id), status)?;
        assert!(s.pin_memory(&id, true).is_err());
        assert!(!s.overview_text(800)?.contains(&id));
        s.set_status(std::slice::from_ref(&id), "active")?;
        assert!(
            !s.overview_text(800)?.contains(&id),
            "恢复有效不等于用户再次置顶"
        );
    }
    Ok(())
}

#[test]
fn every_byte_counts_including_omission_warning_and_identity() -> Result<()> {
    let (_dir, _loc, s) = project()?;
    let mut rules = Vec::new();
    for i in 0..90 {
        let id = s.intent_propose(
            "constraint",
            &format!("第{i}条🚧不可跨界"),
            "不会自动加载的正文",
        )?;
        s.intent_confirm(&id)?;
        rules.push(id);
    }
    let low = memory(&s, "先展示红线才展示我", 1)?;
    s.pin_memory(&low, true)?;
    let heading = s.project().unwrap().heading();
    for budget in (0..=heading.len() + 120).chain([799, 800, 801, 1_499, 2_000, usize::MAX]) {
        let text = s.overview_text(budget)?;
        assert!(text.len() <= budget.min(2_000));
        if text.is_empty() {
            continue;
        } // 身份和安全提示容纳不下，宁可不注入。
        assert_eq!(text.lines().next(), Some(heading.as_str()));
        assert!(
            text.contains("还有约束未展示，请先查看"),
            "{budget}: {text}"
        );
        assert!(!text.contains(&low));
    }
    let auto = s.overview_text(800)?;
    assert!(rules.iter().any(|id| auto.contains(id)));
    assert!(s.intent_list()?.len() <= 1_500);
    Ok(())
}

#[test]
fn no_silent_omission_at_the_exact_boundary_even_for_one_rule() -> Result<()> {
    let (_dir, _loc, s) = project()?;
    let rule = s.intent_propose("constraint", "🚧保护项目边界", "仅在查询时读取")?;
    s.intent_confirm(&rule)?;
    let full = s.overview_text(2_000)?;
    assert!(full.contains(&rule) && !full.contains("还有约束未展示"));
    assert_eq!(s.overview_text(full.len())?, full);
    let heading = s.project().unwrap().heading();
    for budget in heading.len()..full.len() {
        let text = s.overview_text(budget)?;
        assert!(text.len() <= budget);
        assert!(
            text.is_empty() || text.contains(&rule) || text.contains("还有约束未展示，请先查看")
        );
    }
    Ok(())
}

#[test]
fn proposal_fields_and_all_views_are_bounded_redacted_single_lines() -> Result<()> {
    let (_dir, _loc, s) = project()?;
    let secret = "不得泄漏的测试值";
    let key = "password";
    let text = format!("边界🚧\r\n伪行\t口令 {key}={secret}\u{2028}续\u{202e}尾");
    let id = s.intent_propose("constraint", &text, &text)?;
    s.intent_confirm(&id)?;
    let stored: (String, String) =
        s.conn
            .query_row("SELECT title,body FROM intents WHERE id=?1", [&id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
    for field in [stored.0, stored.1] {
        assert!(field.contains("***") && !field.contains(secret));
        assert!(!field.chars().any(char::is_control));
        assert!(!field.contains(['\u{2028}', '\u{202e}']));
    }
    for view in [s.intent_list()?, s.intent_get(&id)?, s.overview_text(800)?] {
        assert!(!view.contains(secret) && !view.contains("\n伪行") && !view.contains('\u{202e}'));
        assert!(view.contains("边界🚧"));
    }
    let huge = "🚧".repeat(10_000);
    for (kind, title, body) in [
        ("not_a_kind", "标题", "正文"),
        ("goal", &huge, "正文"),
        ("goal", "标题", &huge),
        ("goal", "\n\u{202e}", "正文"),
        ("goal", "标题", "\t"),
    ] {
        assert!(s.intent_propose(kind, title, body).is_err());
    }
    let bad_id = format!("bad\npassword={secret}");
    assert!(
        !s.intent_get(&bad_id)
            .unwrap_err()
            .to_string()
            .contains(secret)
    );
    Ok(())
}

#[test]
fn adopted_old_library_can_be_queried_read_only_without_auxiliary_schema() -> Result<()> {
    let dir = tempdir()?;
    fs::create_dir(dir.path().join(".loom"))?;
    let loc = location(dir.path())?;
    let conn = Connection::open(loc.database())?;
    conn.execute_batch(SCHEMA_V1)?;
    conn.pragma_update(None, "user_version", 1)?;
    let secret = "旧库敏感数据";
    let title = format!("旧红线\n伪行 token={secret}");
    conn.execute(
        "INSERT INTO intents VALUES('legacy-1','constraint',?1,'原始理由','active',1,0)",
        [&title],
    )?;
    conn.execute(
        "INSERT INTO intents VALUES('legacy-big','constraint',?1,?1,'active',1,0)",
        ["🚧".repeat(30_000)],
    )?;
    drop(conn);
    assert!(loc.open(false).is_err());
    loc.bind(Bind::AdoptLegacy)?;
    let ro = loc.open(false)?;
    let version = || {
        ro.conn
            .pragma_query_value(None, "schema_version", |r| r.get::<_, i64>(0))
    };
    let before = version()?;
    let overview = ro.overview_text(800)?;
    assert!(overview.contains("legacy-1") && overview.contains("还有约束未展示，请先查看"));
    for view in [
        overview,
        ro.intent_get("legacy-1")?,
        ro.intent_get("legacy-big")?,
        ro.intent_list()?,
    ] {
        assert!(view.len() <= 1_500 && !view.contains(secret) && !view.contains("\n伪行"));
    }
    assert!(ro.intent_get("legacy-1")?.contains("原始理由"));
    assert!(ro.intent_propose("goal", "不能写入", "只读连接").is_err());
    assert_eq!(version()?, before);
    assert_eq!(ro.schema_version()?, SCHEMA_VERSION);
    Ok(())
}

#[test]
fn same_name_projects_and_stale_bindings_fail_closed() -> Result<()> {
    let temp = tempdir()?;
    let roots = [temp.path().join("a/twin"), temp.path().join("b/twin")];
    for root in &roots {
        fs::create_dir_all(root)?;
    }
    let a = location(&roots[0])?;
    let b = location(&roots[1])?;
    a.bind(Bind::Initialize)?;
    let b_id = b.bind(Bind::Initialize)?;
    let s = a.open(true)?;
    let id = s.intent_propose("constraint", "只属于甲的红线", "不能外泄")?;
    s.intent_confirm(&id)?;
    let other = b.open(true)?;
    assert!(other.intent_get(&id).is_err());
    assert!(other.intent_revoke(&id).is_err());
    assert!(!other.overview_text(800)?.contains(&id));
    assert!(
        ProjectLocation::select(&roots[1], Some(&roots[1]), Some(a.database()))?
            .open(true)
            .is_err()
    );
    // 已打开连接不能沿用旧授权：分别替换磁盘标识、库内绑定。
    let marker = roots[0].join(".loom/project.id");
    let saved = fs::read(&marker)?;
    fs::write(&marker, b_id.id())?;
    assert!(s.overview_text(800).is_err() && s.intent_get(&id).is_err());
    assert!(s.intent_revoke(&id).is_err());
    fs::write(&marker, saved)?;
    assert!(s.overview_text(800)?.contains(&id));
    s.conn
        .execute("UPDATE loom_identity SET project_id=?1", [b_id.id()])?;
    assert!(s.intent_list().is_err() && s.intent_revoke(&id).is_err());
    assert!(
        Store::open_in_memory()?
            .intent_propose("goal", "未绑定", "不能写")
            .is_err()
    );
    Ok(())
}

#[test]
fn project_heading_masks_credentials_and_never_splices_a_partial_root() -> Result<()> {
    let temp = tempdir()?;
    let secret = "仅测试目录字段";
    let root = temp
        .path()
        .join(format!("token={secret}"))
        .join("双字节🚧\u{2028}项目");
    fs::create_dir_all(&root)?;
    let loc = location(&root)?;
    let project = loc.bind(Bind::Initialize)?;
    let s = loc.open(true)?;
    let rule = s.intent_propose("constraint", "身份完整才展示", "不能截断根")?;
    s.intent_confirm(&rule)?;
    let full = s.overview_text(800)?;
    let heading = full.lines().next().unwrap();
    assert!(heading.contains(project.id()) && heading.contains("***"));
    assert!(!heading.contains(secret) && !heading.contains('\u{2028}'));
    assert!(s.overview_text(heading.len() - 1)?.is_empty());
    Ok(())
}

#[test]
fn input_and_explicit_detail_round_trip_the_exact_utf8_field_limits() -> Result<()> {
    let (_dir, _loc, s) = project()?;
    let title = "🚧".repeat(48);
    let body = "🔎".repeat(256);
    let id = s.intent_propose("decision", &title, &body)?;
    let detail = s.intent_get(&id)?;
    assert!(detail.contains(&body) && detail.contains(&title) && detail.len() <= 1_500);
    assert!(
        s.intent_propose("goal", &(title.clone() + "x"), &body)
            .is_err()
    );
    assert!(s.intent_propose("goal", &title, &(body + "x")).is_err());
    Ok(())
}

#[test]
fn long_explicitly_pinned_memory_keeps_its_id_and_a_safe_title_preview() -> Result<()> {
    let (_dir, _loc, s) = project()?;
    let secret = "不要泄漏开头";
    let title = format!(
        "长标题{} token={secret} {}",
        "甲".repeat(57),
        "🚧".repeat(65)
    );
    let id = memory(&s, &title, 1)?;
    s.pin_memory(&id, true)?;
    let overview = s.overview_text(800)?;
    assert!(overview.contains(&id) && overview.contains("长标题") && overview.contains('…'));
    assert!(!overview.contains(secret) && !overview.contains("仅按需读取的经验正文"));
    Ok(())
}
