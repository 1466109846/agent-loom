use super::{Confirmation, report};
use anyhow::Result;
use clap::Subcommand;
use loom_core::{
    enhance::{hybrid, semantic},
    project::ProjectLocation,
};

#[derive(Subcommand)]
pub(crate) enum Semantic {
    /// 预演发送范围；加 --yes 授权当前模型地址并建向量
    Enable {
        #[arg(long)]
        yes: bool,
    },
    /// 停用混合检索；加 --purge 同时删除缓存向量
    Disable {
        #[command(flatten)]
        confirmation: Confirmation,
        #[arg(long)]
        purge: bool,
    },
    /// 查看授权、模型地址和向量覆盖情况
    Status,
    /// 给有效经验增量建向量，可重复续建
    Index {
        #[arg(long)]
        limit: Option<usize>,
    },
    /// 明确授权本项目的一条有效记忆参与语义查找（最多 12 条）
    Allow {
        id: String,
        #[command(flatten)]
        confirmation: Confirmation,
    },
    /// 明确撤回这条记忆的语义查找授权
    Deny {
        id: String,
        #[command(flatten)]
        confirmation: Confirmation,
    },
    /// 列出本项目已授权的有效记忆；不调用模型
    List,
    /// 只在本项目人工授权集内跨措辞查找，未启用模型时在同一范围回退
    Find { query: String },
}

impl Semantic {
    pub(super) fn run(self, project: &ProjectLocation) -> Result<()> {
        // 这三条是给人在终端里看的，结尾补上换行；旧入口的输出保持原样。
        let line = |text: String| text + "\n";
        report(match self {
            Self::Status => line(hybrid::status(
                &project.open(false)?,
                &crate::model::configured(),
            )?),
            Self::Enable { yes: false } => line(hybrid::preview(
                &project.open(false)?,
                &crate::model::configured(),
            )?),
            Self::Disable {
                confirmation,
                purge,
            } => line(hybrid::disable(&confirmation.open(project)?, purge)?),
            Self::Enable { yes: true } | Self::Index { .. } => {
                let store = project.open(true)?;
                let models = crate::model::configured();
                let limit = if let Self::Index { limit } = self {
                    limit
                } else {
                    hybrid::enable(&store, &models)?;
                    println!("已授权当前模型地址。");
                    None
                };
                let result = hybrid::index(&store, &models, limit, |r| eprintln!("建向量：{r}"))?;
                println!("{result}");
                anyhow::ensure!(
                    result.failed == 0,
                    "本次索引未完成，请执行 loom semantic index 续建"
                );
                return Ok(());
            }
            Self::Allow { id, confirmation } => semantic::allow(&confirmation.open(project)?, &id)?,
            Self::Deny { id, confirmation } => semantic::deny(&confirmation.open(project)?, &id)?,
            Self::List => semantic::list(&project.open(false)?)?,
            Self::Find { query } => semantic::find(&project.open(false)?, &query)?,
        })
    }
}
