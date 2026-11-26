//! 以实际核对的源码指纹保存人工文件职责；结构索引不拥有人工记录。

mod links;
pub(crate) mod source;

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::{Store, project::ProjectIdentity, redact::redact, util::now_ts};
use source::Live;

const MAX_ROLE_BYTES: usize = 1536;
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS project_file_roles (
    project_id TEXT NOT NULL, path TEXT NOT NULL, role TEXT NOT NULL,
    source_sha256 TEXT NOT NULL CHECK(length(source_sha256)=64), checked_at INTEGER NOT NULL,
    PRIMARY KEY(project_id, path)) WITHOUT ROWID";

struct Note {
    sha: String,
    role: String,
}

fn valid_sha(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit())
}

fn clean_role(role: &str) -> Result<String> {
    ensure!(
        role.len() <= MAX_ROLE_BYTES,
        "职责超过 {MAX_ROLE_BYTES} 字节；请拆短后再写"
    );
    ensure!(
        !role.chars().any(|c| c.is_control() && !c.is_whitespace()),
        "职责不能含控制字符"
    );
    let (safe, _) = redact(role);
    let line = safe.split_whitespace().collect::<Vec<_>>().join(" ");
    ensure!(
        !line.is_empty() && line.len() <= MAX_ROLE_BYTES,
        "需要非空、有界的职责文本"
    );
    Ok(line)
}

fn has_table(db: &Connection, name: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )?)
}

pub(crate) fn binding_matches(db: &Connection, project: &ProjectIdentity) -> Result<bool> {
    // 长寿命 Store 可能在另一连接显式搬迁绑定前就已打开。仅核对原目录标记并不足够。
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM loom_identity WHERE singleton=1 AND project_id=?1 AND canonical_root=?2)",
        params![project.id(), project.root().to_str().context("项目根编码无效")?], |r| r.get(0))?)
}

fn saved(db: &Connection, project: &ProjectIdentity, key: &str) -> Result<Option<Note>> {
    if !has_table(db, "project_file_roles")? {
        return Ok(None);
    }
    let row = db
        .query_row(
            "SELECT source_sha256, role FROM project_file_roles WHERE project_id=?1 AND path=?2",
            params![project.id(), key],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?;
    row.map(|(sha, role)| {
        ensure!(valid_sha(&sha), "人工记录的基线无效，未作确认");
        Ok(Note {
            sha,
            role: clean_role(&role)?,
        })
    })
    .transpose()
}

enum Snapshot {
    Parsed(String),
    Uncovered(&'static str),
}

impl Snapshot {
    fn get(db: &Connection, key: &str, parsed: bool) -> Result<Self> {
        if !parsed {
            return Ok(Self::Uncovered("结构查询没有返回兼容、已解析的受管文件"));
        }
        let sha: Option<String> = db.query_row("SELECT f.sha256 FROM structure_files s JOIN files f ON f.id=s.file_id WHERE f.path=?1 AND s.state='indexed'",
            [key], |r| r.get(0)).optional()?;
        Ok(match sha {
            Some(sha) if valid_sha(&sha) => Self::Parsed(sha),
            _ => Self::Uncovered("没有可核对的解析指纹"),
        })
    }

    fn sha(&self) -> Option<&str> {
        match self {
            Self::Parsed(s) => Some(s),
            Self::Uncovered(_) => None,
        }
    }
}

struct Evidence {
    saved: Option<Note>,
    snapshot: Snapshot,
    live: Live,
}

impl Evidence {
    fn inspect(
        db: &Connection,
        project: &ProjectIdentity,
        key: &str,
        parsed: bool,
    ) -> Result<Self> {
        let saved = saved(db, project, key)?;
        let snapshot = Snapshot::get(db, key, parsed)?;
        let live = match (&saved, &snapshot) {
            // 未正式收纳且没有人工历史：连文件是否存在都不需要探查。
            (None, Snapshot::Uncovered(reason)) => Live::Unavailable(reason),
            // 历史路径可以在边界内判断「文件消失」，但没有快照不能读取其内容。
            _ => source::observe(project, db.path(), key, snapshot.sha().is_some()),
        };
        Ok(Self {
            saved,
            snapshot,
            live,
        })
    }

    fn write(
        &mut self,
        db: &Connection,
        project: &ProjectIdentity,
        key: &str,
        expected: &str,
        replacement: Option<&str>,
    ) -> Result<()> {
        let Live::Read(current) = &self.live else {
            anyhow::bail!("文件消失、未覆盖或不可校验；未写入职责");
        };
        ensure!(
            self.snapshot.sha() == Some(current.as_str()),
            "当前源码与已解析快照不符；请更新索引后重新核对"
        );
        ensure!(
            current.eq_ignore_ascii_case(expected),
            "期望指纹已过期；请重新核对源码"
        );
        if let Some(text) = replacement {
            let role = clean_role(text)?;
            ensure!(
                !self
                    .saved
                    .as_ref()
                    .is_some_and(|n| n.role == role && n.sha != *current),
                "职责未改但基线已变化；保留原文请显式复核"
            );
            db.execute_batch(SCHEMA)?;
            db.execute("INSERT INTO project_file_roles(project_id,path,role,source_sha256,checked_at) VALUES(?1,?2,?3,?4,?5)
                ON CONFLICT(project_id,path) DO UPDATE SET role=excluded.role,source_sha256=excluded.source_sha256,checked_at=excluded.checked_at",
                params![project.id(), key, role, current, now_ts()])?;
            self.saved = Some(Note {
                role,
                sha: current.clone(),
            });
        } else {
            let old = self.saved.as_mut().context("尚无人工职责可供复核")?;
            let count = db.execute("UPDATE project_file_roles SET source_sha256=?1,checked_at=?2 WHERE project_id=?3 AND path=?4",
                params![current, now_ts(), project.id(), key])?;
            ensure!(count == 1, "复核未能定位原职责");
            old.sha.clone_from(current);
        }
        Ok(())
    }

    fn assessment(&self) -> (Option<&str>, &'static str, &'static str) {
        let baseline = self.saved.as_ref().map(|n| n.sha.as_str());
        let parsed = self.snapshot.sha();
        match &self.live {
            Live::Missing => (None, "文件消失", "保留原路径和人工基线，不迁给同名文件"),
            Live::Unavailable(reason) => (None, "未覆盖或不可校验", *reason),
            Live::Read(sha) => {
                let live = Some(sha.as_str());
                let status = match baseline {
                    Some(known) if known == sha => "当前",
                    Some(_) => "需复核",
                    None if live == parsed => "未写职责",
                    None => "未覆盖或不可校验",
                };
                let hint = if live != parsed {
                    "现场与结构快照不同，写入前须先刷新索引并再次核对"
                } else if status == "需复核" {
                    "人工基线与现场不同，原职责仅供复核"
                } else {
                    "指纹按原始文件字节核对；人工职责不是解析事实"
                };
                (live, status, hint)
            }
        }
    }

    fn render(&self, key: &str) -> String {
        let (current, status, hint) = self.assessment();
        let baseline = self.saved.as_ref().map(|n| n.sha.as_str());
        let parsed = self.snapshot.sha();
        let mut out = format!(
            "状态：{status}\n路径：{key}\n已知指纹：{}\n快照指纹：{}\n当前指纹：{}\n说明：{hint}",
            baseline.unwrap_or("无"),
            parsed.unwrap_or("无"),
            current.unwrap_or("不可校验")
        );
        if let Some(note) = &self.saved {
            out.push_str(if status == "当前" {
                "\n人工职责："
            } else {
                "\n历史职责（非当前）："
            });
            out.push_str(&note.role);
        }
        out
    }
}

pub(crate) fn read_role(
    db: &Connection,
    project: &ProjectIdentity,
    key: &str,
) -> Result<(&'static str, Option<String>)> {
    let parsed = crate::structure::query::parsed_in_snapshot(db, key)?;
    let e = Evidence::inspect(db, project, key, parsed)?;
    let (_, status, _) = e.assessment();
    Ok((status, e.saved.map(|n| n.role)))
}

impl Store {
    /// 只读；已解析但未撰写职责的文件也返回可供核对的完整当前指纹。
    pub fn note_get(&self, path: &str) -> Result<String> {
        self.file_role(path, None)
    }

    /// 仅凭现场、解析快照和调用方明确给出的 SHA-256 三者一致，才能撰写职责。
    pub fn note_set(&self, path: &str, expected_sha: &str, role: &str) -> Result<String> {
        self.file_role(path, Some((expected_sha, Some(role))))
    }

    /// 显式复核原文；不生成职责，也不根据一次读取或刷新自动移动人工基线。
    pub fn note_review(&self, path: &str, expected_sha: &str) -> Result<String> {
        self.file_role(path, Some((expected_sha, None)))
    }

    fn file_role(&self, path: &str, change: Option<(&str, Option<&str>)>) -> Result<String> {
        let project = self.project().context("文件职责必须绑定到一个项目")?;
        project.verify_live_root()?;
        let key = source::key(path)?;
        if let Some((expected, _)) = change {
            ensure!(valid_sha(expected), "需要明确的 64 位 SHA-256 十六进制指纹");
        }
        let behavior = if change.is_some() {
            TransactionBehavior::Immediate
        } else {
            TransactionBehavior::Deferred
        };
        let tx = Transaction::new_unchecked(&self.conn, behavior)?;
        ensure!(binding_matches(&tx, project)?, "项目绑定已改变；未加载职责");
        let parsed = crate::structure::query::parsed_in_snapshot(&tx, &key)?;
        let mut e = Evidence::inspect(&tx, project, &key, parsed)?;
        if let Some((expected, role)) = change {
            e.write(&tx, project, &key, expected, role)?;
        }
        project.verify_live_root()?;
        tx.commit()?;
        Ok(e.render(&key))
    }
}

#[cfg(test)]
mod tests;
