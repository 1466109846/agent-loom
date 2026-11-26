use std::{fs, path::Path};

use crate::{
    Store,
    project::{Bind, ProjectLocation},
    util::sha256_hex,
};
use anyhow::Result;

mod acceptance;
mod boundaries;

const FILE: &str = "src/writer.ts";
const FIRST: &str = "export function save() { return 1; }\n";
const SECOND: &str = "export function save() { return 2; }\n";
const ROLE: &str = "负责保存，并返回写入结果";

fn put(root: &Path, name: &str, bytes: impl AsRef<[u8]>) -> Result<()> {
    let p = root.join(name);
    fs::create_dir_all(p.parent().unwrap())?;
    Ok(fs::write(p, bytes)?)
}

fn bind(root: &Path) -> Result<ProjectLocation> {
    fs::create_dir_all(root)?;
    let location = ProjectLocation::select(root, Some(root), None)?;
    location.bind(Bind::Initialize)?;
    Ok(location)
}

fn fixture(root: &Path) -> Result<Store> {
    let s = bind(root)?.open(true)?;
    put(root, FILE, FIRST)?;
    assert_eq!(s.index_structure()?.parsed, 1);
    Ok(s)
}

fn field<'a>(report: &'a str, key: &str) -> &'a str {
    report
        .lines()
        .find_map(|line| {
            line.split_once('：')
                .filter(|(k, _)| *k == key)
                .map(|(_, v)| v)
        })
        .unwrap_or_else(|| panic!("缺少字段 {key}：{report}"))
}

fn check(s: &Store, status: &str, baseline: &str, live: &str) -> Result<String> {
    let out = s.note_get(FILE)?;
    assert_eq!(field(&out, "路径"), FILE);
    assert_eq!(field(&out, "状态"), status, "{out}");
    assert_eq!(field(&out, "已知指纹"), baseline, "人工基线不能随索引漂移");
    assert_eq!(field(&out, "当前指纹"), live, "不能把快照冒充现场核对");
    Ok(out)
}

fn refuse_both(s: &Store, path: &str, hash: &str) {
    assert!(s.note_set(path, hash, "不应落库").is_err());
    assert!(s.note_review(path, hash).is_err());
}

#[test]
fn observation_revert_stale_expectation_and_explicit_review() -> Result<()> {
    let d = tempfile::tempdir()?;
    let root = d.path();
    let s = fixture(root)?;
    let a = sha256_hex(FIRST.as_bytes());
    let b = sha256_hex(SECOND.as_bytes());
    let get = check(&s, "未写职责", "无", &a)?;
    // 独立使用 .NET SHA256 对两份固定 UTF-8/LF 字节计算，不从索引读回作为答案。
    assert_eq!(
        a,
        "f22974992f735b7e35ba5d4f42ad426bd8acc2e47f6210535584a550405e09ab"
    );
    assert_eq!(
        b,
        "1b0e78d58adebd81c5754c1d4ba76900567613144709fc7d52586d313fb67136"
    );
    assert_eq!(field(&get, "快照指纹"), a);
    assert!(s.note_review(FILE, &a).is_err(), "复核不能凭空创建职责");
    assert!(s.note_set(FILE, &b, ROLE).is_err());
    s.note_set(".\\src\\writer.ts", field(&get, "当前指纹"), ROLE)?;
    assert_eq!(field(&check(&s, "当前", &a, &a)?, "人工职责"), ROLE);

    put(root, FILE, SECOND)?;
    let stale = check(&s, "需复核", &a, &b)?;
    assert_eq!(field(&stale, "历史职责（非当前）"), ROLE);
    assert_eq!(field(&stale, "快照指纹"), a);
    refuse_both(&s, FILE, &a);
    refuse_both(&s, FILE, &b); // 磁盘对了，解析快照仍未跟上。
    put(root, FILE, FIRST)?;
    check(&s, "当前", &a, &a)?;
    put(root, FILE, SECOND)?;
    s.index_structure()?;
    assert_eq!(field(&check(&s, "需复核", &a, &b)?, "快照指纹"), b);
    refuse_both(&s, FILE, &a);
    assert!(
        s.note_set(FILE, &b, ROLE).is_err(),
        "原文原样重发不能替代人的显式复核"
    );
    s.note_review(FILE, &b)?;
    assert_eq!(field(&check(&s, "当前", &b, &b)?, "人工职责"), ROLE);
    s.note_set(FILE, &b, "负责保存第二版结果")?;
    assert_eq!(field(&s.note_get(FILE)?, "人工职责"), "负责保存第二版结果");
    Ok(())
}

#[test]
fn first_write_also_refuses_a_live_hash_not_yet_parsed() -> Result<()> {
    let d = tempfile::tempdir()?;
    let s = fixture(d.path())?;
    let a = sha256_hex(FIRST.as_bytes());
    let b = sha256_hex(SECOND.as_bytes());
    put(d.path(), FILE, SECOND)?;
    let out = check(&s, "未覆盖或不可校验", "无", &b)?;
    assert_eq!(field(&out, "快照指纹"), a);
    refuse_both(&s, FILE, &a);
    refuse_both(&s, FILE, &b);
    s.index_structure()?;
    s.note_set(FILE, &b, ROLE)?;
    drop(s);
    let reader = ProjectLocation::select(d.path(), Some(d.path()), None)?.open(false)?;
    assert_eq!(field(&check(&reader, "当前", &b, &b)?, "人工职责"), ROLE);
    Ok(())
}

#[test]
fn deletion_rebuild_and_restoration_preserve_path_bound_history() -> Result<()> {
    let d = tempfile::tempdir()?;
    let s = fixture(d.path())?;
    let a = sha256_hex(FIRST.as_bytes());
    let b = sha256_hex(SECOND.as_bytes());
    s.note_set(FILE, &a, ROLE)?;
    fs::remove_file(d.path().join(FILE))?;
    check(&s, "文件消失", &a, "不可校验")?;
    assert_eq!(s.index_structure()?.removed, 1);
    let after = check(&s, "文件消失", &a, "不可校验")?;
    assert_eq!(field(&after, "历史职责（非当前）"), ROLE);
    refuse_both(&s, FILE, &a);
    put(d.path(), "other/writer.ts", FIRST)?;
    s.index_structure()?;
    assert!(
        !s.note_get("other/writer.ts")?.contains(ROLE),
        "同名新路径不能继承历史"
    );
    put(d.path(), FILE, SECOND)?;
    check(&s, "未覆盖或不可校验", &a, "不可校验")?;
    refuse_both(&s, FILE, &b);
    s.index_structure()?;
    check(&s, "需复核", &a, &b)?;
    // 模拟清空全部派生文件和完成标记再重建，不调用任何人工写入口。
    s.conn
        .execute_batch("DELETE FROM files; DELETE FROM structure_snapshot;")?;
    check(&s, "未覆盖或不可校验", &a, "不可校验")?;
    s.index_structure()?;
    check(&s, "需复核", &a, &b)?;
    put(d.path(), FILE, FIRST)?;
    s.index_structure()?;
    assert_eq!(field(&check(&s, "当前", &a, &a)?, "人工职责"), ROLE);
    Ok(())
}

#[test]
fn adopted_v1_anchor_does_not_authorize_source_and_reads_never_upgrade() -> Result<()> {
    use crate::db::SCHEMA_V1;
    let d = tempfile::tempdir()?;
    let root = d.path();
    let loc = ProjectLocation::select(root, Some(root), None)?;
    fs::create_dir(root.join(".loom"))?;
    let db = rusqlite::Connection::open(loc.database())?;
    db.execute_batch(SCHEMA_V1)?;
    db.pragma_update(None, "user_version", 1)?;
    let a = sha256_hex(FIRST.as_bytes());
    db.execute(
        "INSERT INTO files(path,sha256,indexed_at) VALUES (?1,?2,0)",
        [FILE, &a],
    )?;
    drop(db);
    put(root, FILE, FIRST)?;
    assert!(loc.open(false).is_err());
    loc.bind(Bind::AdoptLegacy)?;
    let s = loc.open(true)?;
    let schema = |s: &Store| {
        s.conn
            .pragma_query_value(None, "schema_version", |r| r.get::<_, i64>(0))
    };
    let before = schema(&s)?;
    let ro = loc.open(false)?;
    check(&ro, "未覆盖或不可校验", "无", "不可校验")?;
    refuse_both(&s, FILE, &a);
    assert_eq!(before, schema(&s)?, "查旧库或拒写不能迁移/建表");
    s.index_structure()?;
    let indexed = schema(&s)?;
    check(&ro, "未写职责", "无", &a)?;
    assert!(s.note_review(FILE, &a).is_err());
    assert_eq!(indexed, schema(&s)?);
    s.note_set(FILE, &a, ROLE)?;
    assert_eq!(field(&check(&ro, "当前", &a, &a)?, "人工职责"), ROLE);
    assert!(ro.note_set(FILE, &a, "写只读库").is_err());
    assert_eq!(
        s.conn
            .query_row("SELECT COUNT(*) FROM file_notes", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    Ok(())
}

#[test]
fn role_is_redacted_bounded_and_cannot_inject_report_fields() -> Result<()> {
    let d = tempfile::tempdir()?;
    let s = fixture(d.path())?;
    let a = sha256_hex(FIRST.as_bytes());
    let fake = "deliberately-fictional-value";
    let role = format!("写入路由，token={fake}\n状态：文件消失");
    let written = s.note_set(FILE, &a, &role)?;
    assert!(!written.contains(fake));
    let report = s.note_get(FILE)?;
    assert_eq!(field(&report, "状态"), "当前");
    assert!(field(&report, "人工职责").contains("token=***"));
    assert!(!report.contains(fake));
    let (key, stored): (String, String) = s.conn.query_row(
        "SELECT path, role FROM project_file_roles WHERE project_id=?1 AND instr(role, '写入路由')>0",
        [s.project().unwrap().id()], |r| Ok((r.get(0)?, r.get(1)?)))?;
    assert_eq!(key, FILE, "脱敏后的职责仍可按词定位到项目内文件");
    assert_eq!(
        stored,
        field(&report, "人工职责"),
        "持久化文本本身已脱敏，而不只是显示时遮盖"
    );
    assert_eq!(
        report.lines().filter(|l| l.starts_with("状态：")).count(),
        1
    );
    for invalid in [
        String::new(),
        " \t".into(),
        "坏\0文本".into(),
        "长".repeat(8192),
    ] {
        assert!(s.note_set(FILE, &a, &invalid).is_err());
    }
    for bad in ["", "*", "HEAD", &"0".repeat(63)] {
        refuse_both(&s, FILE, bad);
    }
    assert_eq!(s.note_get(FILE)?, report, "拒写不能部分修改");
    Ok(())
}
