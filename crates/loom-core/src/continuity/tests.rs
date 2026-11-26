use std::{fs, path::Path, process::Command};

use anyhow::{Result, bail};
use rusqlite::{Connection, params};

use crate::memory::RememberOutcome;
use crate::project::{Bind, ProjectLocation};
use crate::{MemoryType, NewMemory, Store};

fn project(root: &Path) -> Result<ProjectLocation> {
    fs::create_dir_all(root)?;
    let location = ProjectLocation::select(root, Some(root), None)?;
    location.bind(Bind::Initialize)?;
    Ok(location)
}

const BODY: &str = "只在按编号取原文时才读取展示这一段历史正文。";

fn record(s: &Store, title: &str, anchor: &str, ty: MemoryType, age_days: i64) -> Result<String> {
    let saved = s.remember(NewMemory {
        mem_type: Some(ty),
        title: title.into(),
        body: BODY.into(),
        anchors: if anchor.is_empty() {
            vec![]
        } else {
            vec![anchor.into()]
        },
        updated_at: Some(crate::util::now_ts() - age_days * 86_400),
        force: true,
        ..Default::default()
    })?;
    match saved {
        RememberOutcome::Created { id, .. } => Ok(id),
        other => bail!("测试记忆未写入：{other:?}"),
    }
}

fn note(s: &Store, title: &str) -> Result<String> {
    record(s, title, "", MemoryType::Decision, 0)
}

#[test]
fn canvas_survives_process_restart_until_closed() -> Result<()> {
    const ROOT: &str = "LOOM_M3_CONTINUITY_CHILD_ROOT";
    const TASK: &str = "LOOM_M3_CONTINUITY_CHILD_TASK";
    const MEMORY: &str = "LOOM_M3_CONTINUITY_CHILD_MEMORY";
    if let Some(root) = std::env::var_os(ROOT) {
        let root = Path::new(&root);
        let location = ProjectLocation::select(root, Some(root), None)?;
        let task = std::env::var(TASK)?;
        let pending = location.open(false)?.canvas_pending()?;
        for text in [
            &task[..],
            "定位回归",
            "复核运行记录",
            &std::env::var(MEMORY)?,
        ] {
            assert!(pending.contains(text), "{pending}");
        }
        assert!(!pending.contains("已走完的第一步"));
        assert!(!pending.contains(BODY));
        location.open(true)?.canvas_close(&task)?;
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let loc = project(dir.path())?;
    let s = loc.open(true)?;
    let memory = note(&s, "长期依据不能随任务一起关闭")?;
    let task = s.canvas_open("定位回归", "已走完的第一步", "run.log:12")?;
    s.canvas_update(&task, "复核运行记录", &memory)?;
    drop(s);

    // 子进程只拿根目录和可公开引用的编号，没有父进程的连接或缓存。
    let result = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            std::thread::current().name().unwrap(),
            "--nocapture",
        ])
        .env(ROOT, dir.path())
        .env(TASK, &task)
        .env(MEMORY, &memory)
        .output()?;
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let s = loc.open(true)?;
    assert_eq!(s.canvas_pending()?, "");
    let closed = s.canvas_get(&task)?;
    for text in ["已关闭", "复核运行记录", "已走完的第一步", &memory] {
        assert!(closed.contains(text), "{closed}");
    }
    assert!(s.canvas_update(&task, "不能复活", "x:1").is_err());
    let original = s.get(&memory)?.unwrap();
    assert_eq!(
        (original.status.as_str(), original.body.as_str()),
        ("active", BODY)
    );
    Ok(())
}

#[test]
fn namesakes_and_replaced_root_cannot_share_tasks_or_links() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let a_root = dir.path().join("a/repo");
    let b_root = dir.path().join("b/repo");
    let a_loc = project(&a_root)?;
    let b_loc = project(&b_root)?;
    let a = a_loc.open(true)?;
    let b = b_loc.open(true)?;
    let aa = note(&a, "Alpha 独有记录")?;
    let bb = note(&b, "Beta 独有记录")?;
    let cc = note(&a, "另一个本项目编号")?;
    let ta = a.canvas_open("私有甲", "检查甲", &aa)?;
    let tb = b.canvas_open("私有乙", "检查乙", &bb)?;
    assert!(!a.canvas_pending()?.contains(&tb));
    assert!(!b.canvas_pending()?.contains("私有甲"));
    assert!(a.canvas_get(&tb).is_err());
    assert!(b.canvas_update(&ta, "错误更新", &bb).is_err());
    assert!(b.canvas_close(&ta).is_err());
    assert!(a.evidence_link(&aa, &bb, "supports").is_err());
    assert!(!b.evidence_pack("Alpha")?.contains(&aa));
    assert!(
        ProjectLocation::select(&a_root, Some(&a_root), Some(b_loc.database()))?
            .open(true)
            .is_err()
    );

    let marker = a_root.join(".loom/project.id");
    let old_marker = fs::read(&marker)?;
    fs::copy(b_root.join(".loom/project.id"), &marker)?;
    assert!(a.canvas_open("不应写入", "停", "x:1").is_err());
    assert!(a.canvas_update(&ta, "不应覆盖", &aa).is_err());
    assert!(a.canvas_close(&ta).is_err());
    assert!(a.evidence_link(&aa, &cc, "conflicts").is_err());
    assert!(a.evidence_pack("Alpha").is_err());
    fs::write(&marker, old_marker)?;
    assert!(a.canvas_pending()?.contains("检查甲"));
    assert!(!a.evidence_pack("Alpha")?.contains(&cc));

    // 模拟长寿命连接建立后库内的绑定变化，而根目录标记尚未变化。
    a.conn.execute(
        "UPDATE loom_identity SET project_id=?1",
        [b.project().unwrap().id()],
    )?;
    assert!(a.canvas_pending().is_err());
    assert!(a.canvas_get(&ta).is_err());
    assert!(a.canvas_close(&ta).is_err());
    assert!(a.evidence_link(&aa, &cc, "supports").is_err());
    assert!(a.evidence_pack("Alpha").is_err());
    Ok(())
}

#[test]
fn readonly_pre_m3_database_works_without_ddl_or_identity_upgrade() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let loc = project(dir.path())?;
    let s = loc.open(true)?;
    let id = note(&s, "baseline 原有库中有依据")?;
    let other = note(&s, "保持原有表结构")?;
    let schema = |conn: &Connection| -> Result<(i64, i64, i64)> {
        Ok((
            conn.pragma_query_value(None, "user_version", |r| r.get(0))?,
            conn.pragma_query_value(None, "application_id", |r| r.get(0))?,
            conn.pragma_query_value(None, "schema_version", |r| r.get(0))?,
        ))
    };
    let before = schema(&s.conn)?;
    assert!(
        s.evidence_link(&id, &crate::util::new_id(), "supports")
            .is_err()
    );
    assert_eq!(schema(&s.conn)?, before);
    drop(s);
    let bytes = fs::read(loc.database())?;
    let s = loc.open(false)?;
    assert_eq!(s.canvas_pending()?, "");
    assert!(s.canvas_get(&id).is_err());
    assert!(s.evidence_pack("baseline")?.contains(&id));
    assert!(s.canvas_open("不可建表", "不可写", "a:1").is_err());
    assert!(s.evidence_link(&id, &other, "supports").is_err());
    assert_eq!(schema(&s.conn)?, before);
    drop(s);
    assert_eq!(fs::read(loc.database())?, bytes);
    Ok(())
}

#[test]
fn bounded_nonempty_question_only_and_no_recent_memory_fallback() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let loc = project(dir.path())?;
    let s = loc.open(true)?;
    let unrelated = note(&s, "最近这条与问题无关")?;
    assert!(s.canvas_pending()?.is_empty());
    assert!(s.evidence_pack("  ").is_err());
    assert!(s.evidence_pack(&"问".repeat(2000)).is_err());
    let miss = s.evidence_pack("nothere")?;
    assert!(miss.contains("缺失"));
    assert!(!miss.contains(&unrelated));
    assert!(!miss.contains(BODY));
    Ok(())
}

#[test]
fn only_explicit_one_hop_links_keep_their_original_direction() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let loc = project(dir.path())?;
    let s = loc.open(true)?;
    fs::write(dir.path().join("basis.rs"), "fn base() {}")?;
    let hit = record(
        &s,
        "Needle 当前判断",
        "basis.rs#base",
        MemoryType::Decision,
        0,
    )?;
    let similar = note(&s, "Needle 相似措辞不是反证")?;
    let source = record(
        &s,
        "Witness 独立复核",
        "basis.rs#base",
        MemoryType::Decision,
        0,
    )?;
    let counter = note(&s, "Counter 不同条件的复现")?;
    let far = note(&s, "Secondhop 不得传递")?;
    let support = format!("{source} --supports--> {hit}");
    let conflict = format!("{hit} --conflicts--> {counter}");
    let two = format!("{counter} --conflicts--> {far}");
    // 通用图中的非人工关系不能被抬升成人工旁证。
    s.conn.execute(
        "INSERT INTO edges(src,dst,kind,provenance) VALUES (?1,?2,'conflicts','inferred')",
        params![format!("mem:{similar}"), format!("mem:{hit}")],
    )?;
    let before = s.evidence_pack("Needle")?;
    assert!(before.contains(&hit) && before.contains(&similar));
    assert!(!before.contains("--supports-->"));
    assert!(!before.contains("--conflicts-->"));
    s.evidence_link(&source, &hit, "supports")?;
    s.evidence_link(&source, &hit, "supports")?;
    s.evidence_link(&hit, &counter, "conflicts")?;
    s.evidence_link(&counter, &far, "conflicts")?;
    drop(s);
    let s = loc.open(false)?;
    let pack = s.evidence_pack("Needle")?;
    for text in [
        &support[..],
        &conflict,
        "basis.rs#base",
        "Witness",
        "Counter",
    ] {
        assert!(pack.contains(text), "{pack}");
    }
    assert_eq!(pack.matches(&support).count(), 1);
    assert!(!pack.contains(&far) && !pack.contains(BODY));
    let reverse = s.evidence_pack("Counter")?;
    assert!(
        reverse.contains(&conflict) && reverse.contains(&two),
        "{reverse}"
    );
    assert!(!reverse.contains(&source));
    assert!(s.evidence_pack("Witness")?.contains(&support));
    assert_eq!(s.get(&hit)?.unwrap().body, BODY);
    Ok(())
}

#[test]
fn archived_matches_do_not_crowd_out_current_evidence() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let loc = project(dir.path())?;
    let store = loc.open(true)?;
    let active = record(&store, "Crowded 当前依据", "", MemoryType::Progress, 120)?;
    for i in 0..12 {
        let old = note(&store, &format!("Crowded 历史记录 {i}"))?;
        store.set_status(&[old], "archived")?;
    }
    let pack = store.evidence_pack("Crowded")?;
    assert!(
        pack.contains(&active) && pack.contains("已归档") && pack.contains("省略"),
        "{pack}"
    );
    assert!(!pack.contains("无有效直接命中"));
    Ok(())
}

#[test]
fn lifecycle_and_drift_are_not_conflict_edges() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let loc = project(dir.path())?;
    let s = loc.open(true)?;
    let path = dir.path().join("engine.rs");
    fs::write(&path, "fn run() {}")?;
    let hit = record(
        &s,
        "Driftroot 可核对的进度",
        "engine.rs#run",
        MemoryType::Progress,
        500,
    )?;
    let archived = note(&s, "已归档的证人")?;
    let replaced = note(&s, "被取代的证人")?;
    s.evidence_link(&archived, &hit, "supports")?;
    s.evidence_link(&hit, &replaced, "conflicts")?;
    s.set_status(std::slice::from_ref(&archived), "archived")?;
    s.supersede(&replaced, &hit)?;
    fs::write(&path, "fn run() { todo!() }")?;
    let pack = s.evidence_pack("Driftroot")?;
    for text in [
        &hit[..],
        "engine.rs#run",
        "时间",
        "过期",
        "源码",
        "变化",
        "归档",
        "取代",
        "缺失",
    ] {
        assert!(pack.contains(text), "{pack}");
    }
    assert!(!pack.contains("--conflicts-->") && !pack.contains("--supports-->"));
    assert!(s.evidence_link(&archived, &hit, "supports").is_err());
    assert!(s.evidence_link(&hit, &replaced, "conflicts").is_err());
    assert_eq!(s.get(&replaced)?.unwrap().status, "superseded");
    Ok(())
}

#[test]
fn validation_is_atomic_and_secrets_are_not_stored() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let loc = project(dir.path())?;
    let s = loc.open(true)?;
    let long = "超长".repeat(1000);
    let task = s.canvas_open(
        "查 O'Brien 的记录",
        "检查 password=fixture-only-5678",
        "log:7",
    )?;
    for bad in ["", " \t", "x\r\n伪造标题", "x\u{2028}伪造段", long.as_str()] {
        assert!(s.canvas_open(bad, "一步", "a:1").is_err());
        assert!(s.canvas_update(&task, "不应写入半条更新", bad).is_err());
    }
    let current = s.canvas_get(&task)?;
    assert!(current.contains("O'Brien") && current.contains("password=***"));
    assert!(!current.contains("不应写入半条更新"));
    let a = note(&s, "A 独立记忆")?;
    let b = note(&s, "B 独立记忆")?;
    for (from, to, kind) in [
        (&a[..], &a[..], "supports"),
        (&a, &b, "supersedes"),
        (&a, &b, "SUPPORTS"),
        (&a, "not-found", "conflicts"),
        ("' OR 1=1", &b, "supports"),
    ] {
        assert!(s.evidence_link(from, to, kind).is_err());
    }
    assert!(
        Store::open_in_memory()?
            .canvas_open("没有绑定", "先选项目", "p")
            .is_err()
    );
    drop(s);
    let secret = b"fixture-only-5678";
    assert!(
        !fs::read(loc.database())?
            .windows(secret.len())
            .any(|s| s == secret)
    );
    Ok(())
}

#[test]
fn utf8_budget_keeps_ids_sections_red_line_and_omission_notice() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let loc = project(dir.path())?;
    let s = loc.open(true)?;
    let mut last = String::new();
    let mut previous: Option<String> = None;
    let long = "界🧭".repeat(27);
    for i in 0..18 {
        last = s.canvas_open(&format!("目标{i}{long}"), &long, &long)?;
        let id = record(
            &s,
            &format!("Budget {i} {long}"),
            &format!("{long}.rs#symbol"),
            MemoryType::Progress,
            120,
        )?;
        if let Some(ref prev) = previous {
            for kind in ["supports", "conflicts"] {
                s.evidence_link(prev, &id, kind)?;
            }
        }
        previous = Some(id);
    }
    for i in 0..18 {
        s.canvas_update(&last, &format!("{i}{long}"), &long)?;
    }
    let pack = s.evidence_pack("Budget")?;
    for text in ["直接", "支持", "冲突", "复核", "缺失", "红线", "省略"] {
        assert!(pack.contains(text), "{pack}");
    }
    let pending = s.canvas_pending()?;
    assert!(pending.contains(&last));
    for out in [pack, pending, s.canvas_get(&last)?] {
        assert!(
            out.len() <= 1500 && out.contains("省略"),
            "{} 字节：{out}",
            out.len()
        );
    }
    Ok(())
}
