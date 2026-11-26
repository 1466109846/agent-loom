//! 会话工具 `manage` 的命令行对应：按编号取全文，或改变一条记忆的状态。
use std::io::{self, Write};

use super::{Confirmation, report};
use anyhow::{Result, bail, ensure};
use clap::Subcommand;
use loom_core::{Store, project::ProjectLocation};

#[derive(Subcommand)]
pub(crate) enum Manage {
    /// 按编号取全文：类型、状态、重要度、漂移、标签、正文和锚点
    Get {
        /// 记忆编号，可给多个
        #[arg(required = true, value_name = "编号")]
        ids: Vec<String>,
    },
    /// 归档：不再出现在默认检索里，可用 activate 恢复
    Archive {
        /// 记忆编号，可给多个
        #[arg(required = true, value_name = "编号")]
        ids: Vec<String>,
    },
    /// 把已归档或已被取代的记忆恢复为有效
    Activate {
        /// 记忆编号，可给多个
        #[arg(required = true, value_name = "编号")]
        ids: Vec<String>,
    },
    /// 用一条新记忆取代旧记忆；旧条目保留，标为已取代
    Supersede {
        /// 被取代的旧记忆，可给多个
        #[arg(required = true, value_name = "旧编号")]
        ids: Vec<String>,
        /// 取代它们的新记忆
        #[arg(long, value_name = "新编号")]
        by: String,
    },
    /// 物理删除，不可恢复；只是不再适用时用 archive
    Delete {
        /// 记忆编号，可给多个
        #[arg(required = true, value_name = "编号")]
        ids: Vec<String>,
        #[command(flatten)]
        confirmation: Confirmation,
    },
}

/// 把给出的编号换成库内编号，导入时保留的旧编号也认。
/// 任何一个不存在就整体拒绝：不能只改一部分，再报告成功。
fn resolve(store: &Store, ids: &[String]) -> Result<Vec<String>> {
    let mut found = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(memory) = store.get(id)? else {
            bail!("记忆不存在：{id}");
        };
        if !found.contains(&memory.id) {
            found.push(memory.id);
        }
    }
    Ok(found)
}

impl Manage {
    pub(super) fn run(self, project: &ProjectLocation) -> Result<()> {
        let receipt = match self {
            Self::Get { ids } => {
                let store = project.open(false)?;
                let mut blocks = Vec::new();
                for id in resolve(&store, &ids)? {
                    blocks.extend(store.memory_detail(&id)?);
                }
                return report(blocks.join("\n"));
            }
            Self::Archive { ids } => {
                let store = project.open(true)?;
                let changed = store.set_status(&resolve(&store, &ids)?, "archived")?;
                format!("已归档 {changed} 条")
            }
            Self::Activate { ids } => {
                let store = project.open(true)?;
                let changed = store.set_status(&resolve(&store, &ids)?, "active")?;
                format!("已恢复 {changed} 条")
            }
            Self::Supersede { ids, by } => {
                let store = project.open(true)?;
                let new = resolve(&store, std::slice::from_ref(&by))?.remove(0);
                let old = resolve(&store, &ids)?;
                ensure!(!old.contains(&new), "新旧编号不能相同：{by}");
                for id in &old {
                    store.supersede(id, &new)?;
                }
                format!("{} 条旧记忆已被 {new} 取代", old.len())
            }
            Self::Delete { ids, confirmation } => {
                let store = confirmation.open(project)?;
                let removed = store.delete(&resolve(&store, &ids)?)?;
                format!("已删除 {removed} 条")
            }
        };
        Ok(writeln!(io::stdout().lock(), "{receipt}")?)
    }
}
