//! 分享只从本地人工 CLI 进入，不是会话工具。提示走 stderr；预览 stdout 始终是一份完整 JSON。
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use loom_core::{project::ProjectLocation, share};

use super::{Confirmation, report};

const REVIEW: &str = "请逐条人工核查标题和全文：点选的经验须确为项目资料，不含用户偏好、机器信息、凭据或第三方私密内容。筛查不能判定自然语言是否可公开。SHA-256 只核对本次文本，不是签名或授权。";

#[derive(Args)]
pub(crate) struct Selection {
    #[arg(from_global)]
    root: Option<PathBuf>,
    /// 本次要分享的已确认有效项目目标或红线编号；可重复，不展开关系
    #[arg(long, value_name = "ID")]
    intent: Vec<String>,
    /// 本次亲自选作项目资料的有效经验编号；逐条指定，不能用标签或通配符授权
    #[arg(long, value_name = "ID")]
    memory: Vec<String>,
}

#[derive(Subcommand)]
pub(crate) enum Share {
    /// 只读查看确切的分享 JSON 和规范 payload 的 SHA-256；没有默认全选
    Preview {
        #[command(flatten)]
        selection: Selection,
    },
    /// 复核后只创建新文件。--yes 表示已逐条人工确认本次所选均为可分享的项目资料
    Export {
        file: PathBuf,
        #[command(flatten)]
        selection: Selection,
        /// 逐条读过本次预览后复制的 SHA-256（不是签名）
        #[arg(long)]
        reviewed_sha: String,
        #[command(flatten)]
        confirmation: Confirmation,
    },
    /// 默认只读：查看全文、差异与不可信来源；同时给出摘要和 --yes 才接收整批
    Import {
        #[arg(from_global)]
        root: Option<PathBuf>,
        file: PathBuf,
        #[arg(long, requires = "yes")]
        reviewed_sha: Option<String>,
        #[arg(long, requires = "reviewed_sha")]
        yes: bool,
    },
}

impl Share {
    pub(super) fn run(self, location: &ProjectLocation) -> Result<()> {
        let root = match &self {
            Self::Preview { selection } | Self::Export { selection, .. } => &selection.root,
            Self::Import { root, .. } => root,
        };
        let project = &share::ShareProject::select(
            &std::env::current_dir()?,
            root.as_deref(),
            location.database(),
        )?;
        match self {
            Self::Preview { selection } => {
                eprintln!(
                    "{REVIEW}\n摘要输入：payload 的紧凑 UTF-8 JSON，键按 ASCII 排序，无 BOM、末尾换行或外层摘要。"
                );
                report(share::preview(
                    project,
                    &selection.intent,
                    &selection.memory,
                )?)
            }
            Self::Export {
                file,
                selection,
                reviewed_sha,
                confirmation,
            } => {
                anyhow::ensure!(confirmation.yes, "导出需要显式 --yes");
                share::export(
                    project,
                    &file,
                    &selection.intent,
                    &selection.memory,
                    &reviewed_sha,
                )?;
                report("已新建本地分享文件，未发送到网络。\n".into())
            }
            Self::Import {
                file,
                reviewed_sha,
                yes,
                ..
            } => {
                anyhow::ensure!(
                    yes == reviewed_sha.is_some(),
                    "写入须同时提供 --reviewed-sha 和 --yes"
                );
                report(share::import(project, &file, reviewed_sha.as_deref())?)
            }
        }
    }
}
