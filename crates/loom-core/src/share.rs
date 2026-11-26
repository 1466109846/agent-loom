//! 离线选择性分享 v1。一次 `--memory` 点名一项，不是给作者、项目或后续版本的长期授权。
//! 先逐条人工核查预览中的标题与全文，再用摘要确认这一次选择。规则只能拦住明显的私密信息。
//! SHA-256 的输入是 payload 的紧凑 UTF-8 JSON（键按 ASCII 排序、无 BOM/末尾换行），
//! 记录按 (来源项目、来源记录) 排序。它承诺内容，不签名、不鉴别来源，也不授予任何执行权限。

mod files;
mod receive;
mod snapshot;
mod wire;

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::Store;
use crate::project::{ProjectIdentity, ProjectLocation};
use wire::{Class, Envelope, MAX_RECORDS, Record, valid_id};

/// 预演和写入绑定到同一个显式根，不能从来件、外置数据库或其中自报的路径反推根。
pub struct ShareProject {
    root: std::path::PathBuf,
    location: ProjectLocation,
}

impl ShareProject {
    pub fn select(cwd: &Path, root_arg: Option<&Path>, database: &Path) -> Result<Self> {
        let location = ProjectLocation::select(cwd, root_arg, Some(database))?;
        let root = location.root().to_owned();
        Ok(Self { root, location })
    }
    fn database(&self) -> &Path {
        self.location.database()
    }
}

/// 写事务在身份和来源检查前保留写锁；提交失败也由 RAII 回滚。
fn in_project<T>(
    store: &Store,
    f: impl FnOnce(&Connection, &ProjectIdentity) -> Result<T>,
) -> Result<T> {
    let project = store.project().context("分享需要已绑定的项目库")?;
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    project.verify_live_root()?;
    let bound: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.loom_identity WHERE singleton=1 AND project_id=?1 AND canonical_root=?2)",
        params![project.id(), project.root().to_str().context("项目根编码无效")?], |r| r.get(0),
    )?;
    ensure!(
        bound && valid_id(project.id()),
        "项目身份不符；未加载项目内容"
    );
    let out = f(&tx, project)?;
    tx.commit()?;
    Ok(out)
}

fn selected(project: &ShareProject, intents: &[String], memories: &[String]) -> Result<Envelope> {
    let count = intents.len().saturating_add(memories.len());
    ensure!(
        (1..=MAX_RECORDS).contains(&count),
        "须逐条点选 1 至 {MAX_RECORDS} 项；没有默认全选"
    );
    let mut ids = BTreeSet::new();
    for (class, list) in [(Class::Intent, intents), (Class::Memory, memories)] {
        for id in list {
            ensure!(
                valid_id(id),
                "仅接受当前记录的稳定编号，不接受别名、通配符或遗留编号"
            );
            ids.insert((class, id.as_str()));
        }
    }
    snapshot::read(project, |conn, identity| {
        let mapped = receive::has_map(conn)?;
        let mut records = Vec::with_capacity(ids.len());
        for (class, id) in ids {
            // 在 SQLite 中先检查字节数，不把整个超长遗留字段或未选中的任何记录读入 Rust。
            let sql = match class {
                Class::Intent => "SELECT kind,title,body FROM main.intents WHERE id=?1 AND status='active' AND confirmed=1
                    AND octet_length(kind)<=32 AND octet_length(title)<=?2 AND octet_length(body)<=?3",
                Class::Memory => "SELECT type,title,body FROM main.memories WHERE id=?1 AND status='active'
                    AND octet_length(type)<=32 AND octet_length(title)<=?2 AND octet_length(body)<=?3",
            };
            let (title_limit, body_limit) = class.limits();
            let (kind, title, body) = conn
                .query_row(
                    sql,
                    params![id, title_limit as i64, body_limit as i64],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .context("所选记录的状态或必需字段无效；不支持此遗留结构")?
                .context("所选记录不存在、未确认、不再有效或超限；整份选择未导出")?;
            let mut row = Record {
                record_type: class,
                origin_project_id: identity.into(),
                origin_record_id: id.into(),
                kind,
                title,
                body,
            };
            // 只信本地映射，不把可由用户填写的 origin/legacy_id 等字段当作来源凭证。
            if mapped
                && let Some((project_id, record_id, digest)) =
                    receive::mapped_origin(conn, class, id)?
            {
                row.origin_project_id = project_id;
                row.origin_record_id = record_id;
                row.validate()?;
                ensure!(
                    row.digest()? == digest,
                    "本地记录已不同于接收版本；不能以原来源再次分享"
                );
            }
            records.push(row);
        }
        Envelope::from_selection(records)
    })
}

/// 只读预览。标准输出可直接与导出文件逐字节比较，无附加标题或截断。
pub fn preview(project: &ShareProject, intents: &[String], memories: &[String]) -> Result<String> {
    selected(project, intents, memories)?.text()
}

/// 重新从当前有效记录生成同一份规范内容，核对人的摘要后只创建新文件。
pub fn export(
    project: &ShareProject,
    file: &Path,
    intents: &[String],
    memories: &[String],
    reviewed_sha: &str,
) -> Result<()> {
    let _db = files::local_database(project.database())?;
    let envelope = selected(project, intents, memories)?;
    envelope.review(reviewed_sha)?;
    files::create(file, project.database(), envelope.text()?.as_bytes())
}

/// `None` 只打开只读库并计算差异；显式摘要才允许进入接收事务。CLI 另强制 `--yes`。
pub fn import(project: &ShareProject, file: &Path, reviewed_sha: Option<&str>) -> Result<String> {
    let _db = files::local_database(project.database())?;
    let envelope = Envelope::decode(&files::read(file, project.database())?)?;
    if let Some(reviewed) = reviewed_sha {
        envelope.review(reviewed)?;
        let store = project.location.open(true)?;
        in_project(&store, |conn, identity| {
            receive::process(Some(&store), conn, identity.id(), &envelope)
        })
    } else {
        snapshot::read(project, |conn, identity| {
            receive::process(None, conn, identity, &envelope)
        })
    }
}
