//! 本地知识命令：选择读写权限、收取显式确认，原样交付核心生成的报告。
use std::io::{self, Write};

mod semantic;

use anyhow::{Result, bail, ensure};
use clap::{Args, Subcommand};
use loom_core::memory::RememberOutcome;
use loom_core::project::ProjectLocation;
use loom_core::{MemoryType, NewMemory, Store};

mod manage;
mod share;

#[derive(Subcommand)]
pub(super) enum Knowledge {
    /// 主动查看本项目的目标、红线和置顶经验（整份最多 2000 个 UTF-8 字节）
    Overview,
    /// 离线分享逐条人工选定的项目资料；先预览再确认
    #[command(subcommand)]
    Share(share::Share),
    /// 保存一条经验并输出编号；相似标题须先核对，不强制新建
    Remember {
        #[arg(value_name = "TYPE")]
        ty: MemoryType,
        title: String,
        body: String,
        /// 项目内的相对路径，可附 #符号
        #[arg(long)]
        anchor: Option<String>,
    },
    /// 按编号取记忆全文，或归档、恢复、取代、删除
    #[command(subcommand)]
    Manage(manage::Manage),
    /// 查看项目意图，或显式变更其确认、置顶状态
    #[command(subcommand)]
    Intent(Intent),
    /// 查看、撰写或复核受管文件的人工职责
    #[command(subcommand)]
    Note(Note),
    /// 记录未完成的任务、下一步和证据位置
    #[command(subcommand)]
    Canvas(Canvas),
    /// 查阅证据包或明确声明记忆间的关系
    #[command(subcommand)]
    Evidence(Evidence),
    /// 按需对关键词候选重排；不改动记忆
    Rerank { query: String },
    /// 向量检索：按项目启用、停用、查看和建向量；另有逐条授权标题的小范围语义查找
    #[command(subcommand)]
    Semantic(semantic::Semantic),
    /// 从本次给出的文本提取待审建议；不写库
    Extract { text: String },
}

#[derive(Args)]
pub(super) struct Confirmation {
    /// 已核对本次变更并确认执行
    #[arg(long, required = true)]
    yes: bool,
}

impl Confirmation {
    fn open(self, project: &ProjectLocation) -> Result<Store> {
        ensure!(self.yes, "本次变更需要显式指定 --yes");
        project.open(true)
    }
}

#[derive(Subcommand)]
pub(super) enum Intent {
    /// 登记提议并输出编号；提议本身不生效
    Propose {
        /// goal、constraint、non_goal 或 decision
        kind: String,
        title: String,
        body: String,
    },
    /// 列出意图的编号、标题及状态
    List,
    /// 按编号查看正文和变更记录
    Get { id: String },
    /// 确认一项待审提议
    Confirm {
        id: String,
        #[command(flatten)]
        confirmation: Confirmation,
    },
    /// 撤销当前有效的意图
    Revoke {
        id: String,
        #[command(flatten)]
        confirmation: Confirmation,
    },
    /// 用待审的新意图原子地取代当前有效意图
    Replace {
        old: String,
        new: String,
        #[command(flatten)]
        confirmation: Confirmation,
    },
    /// 将有效经验置顶到主动概览
    Pin {
        memory_id: String,
        #[command(flatten)]
        confirmation: Confirmation,
    },
    /// 取消置顶，不删除经验
    Unpin {
        memory_id: String,
        #[command(flatten)]
        confirmation: Confirmation,
    },
}

#[derive(Subcommand)]
pub(super) enum Note {
    /// 查职责、当前指纹和人工基线
    Get { path: String },
    /// 以已核对的 SHA-256 写入职责
    Set {
        path: String,
        expected_sha: String,
        role: String,
    },
    /// 保留原职责，以已核对的 SHA-256 复核
    Review {
        path: String,
        expected_sha: String,
        #[command(flatten)]
        confirmation: Confirmation,
    },
}

#[derive(Subcommand)]
pub(super) enum Canvas {
    /// 新建任务并输出编号
    Open {
        goal: String,
        next: String,
        evidence: String,
    },
    /// 列出未关闭任务的当前步骤
    List,
    /// 按编号查看任务及历史步骤
    Get { id: String },
    /// 追加下一步及证据位置
    Update {
        id: String,
        next: String,
        evidence: String,
    },
    /// 关闭任务，保留历史记录
    Close { id: String },
}

#[derive(Subcommand)]
pub(super) enum Evidence {
    /// 按问题查阅直接记录、明确关系、复核提示和缺口
    Pack { query: String },
    /// 声明从一条有效记忆指向另一条的支持或冲突关系
    Link {
        from: String,
        to: String,
        #[arg(value_name = "supports|conflicts")]
        kind: String,
        #[command(flatten)]
        confirmation: Confirmation,
    },
}

// 不给核心已计量的报告添加前缀或换行；尤其不能把恰好 2000 字节的概览印成 2001。
fn report(text: String) -> Result<()> {
    Ok(io::stdout().lock().write_all(text.as_bytes())?)
}

impl Knowledge {
    pub(super) fn run(self, project: &ProjectLocation) -> Result<()> {
        let receipt = match self {
            Self::Overview => return report(project.open(false)?.overview_text(2_000)?),
            Self::Share(cmd) => return cmd.run(project),
            Self::Manage(cmd) => return cmd.run(project),
            Self::Semantic(cmd) => return cmd.run(project),
            Self::Rerank { query } => {
                return report(loom_core::enhance::rerank(&project.open(false)?, &query)?);
            }
            Self::Extract { text } => {
                return report(loom_core::enhance::extract(&project.open(false)?, &text)?);
            }
            Self::Remember {
                ty,
                title,
                body,
                anchor,
            } => {
                let outcome = project.open(true)?.remember(NewMemory {
                    mem_type: Some(ty),
                    title,
                    body,
                    anchors: anchor.into_iter().collect(),
                    origin: Some("cli".into()),
                    ..Default::default()
                })?;
                match outcome {
                    RememberOutcome::Created { id, .. } => id,
                    RememberOutcome::Duplicate { id } => {
                        eprintln!("内容相同，未新建；返回原编号。");
                        id
                    }
                    RememberOutcome::Similar { candidates } => {
                        let ids: Vec<_> = candidates.into_iter().map(|(id, _)| id).collect();
                        bail!("存在相似记忆，未写入；请先核对候选编号：{}", ids.join("、"));
                    }
                    RememberOutcome::Rejected { reason } => bail!("未写入：{reason}"),
                }
            }
            Self::Intent(cmd) => match cmd {
                Intent::List => return report(project.open(false)?.intent_list()?),
                Intent::Get { id } => return report(project.open(false)?.intent_get(&id)?),
                Intent::Propose { kind, title, body } => {
                    project.open(true)?.intent_propose(&kind, &title, &body)?
                }
                Intent::Confirm { id, confirmation } => {
                    confirmation.open(project)?.intent_confirm(&id)?
                }
                Intent::Revoke { id, confirmation } => {
                    confirmation.open(project)?.intent_revoke(&id)?
                }
                Intent::Replace {
                    old,
                    new,
                    confirmation,
                } => confirmation.open(project)?.intent_replace(&old, &new)?,
                Intent::Pin {
                    memory_id,
                    confirmation,
                } => confirmation.open(project)?.pin_memory(&memory_id, true)?,
                Intent::Unpin {
                    memory_id,
                    confirmation,
                } => confirmation.open(project)?.pin_memory(&memory_id, false)?,
            },
            Self::Note(cmd) => match cmd {
                Note::Get { path } => return report(project.open(false)?.note_get(&path)?),
                Note::Set {
                    path,
                    expected_sha,
                    role,
                } => project.open(true)?.note_set(&path, &expected_sha, &role)?,
                Note::Review {
                    path,
                    expected_sha,
                    confirmation,
                } => confirmation
                    .open(project)?
                    .note_review(&path, &expected_sha)?,
            },
            Self::Canvas(cmd) => match cmd {
                Canvas::List => return report(project.open(false)?.canvas_pending()?),
                Canvas::Get { id } => return report(project.open(false)?.canvas_get(&id)?),
                Canvas::Open {
                    goal,
                    next,
                    evidence,
                } => project.open(true)?.canvas_open(&goal, &next, &evidence)?,
                Canvas::Update { id, next, evidence } => {
                    project.open(true)?.canvas_update(&id, &next, &evidence)?
                }
                Canvas::Close { id } => project.open(true)?.canvas_close(&id)?,
            },
            Self::Evidence(cmd) => match cmd {
                Evidence::Pack { query } => {
                    return report(project.open(false)?.evidence_pack(&query)?);
                }
                Evidence::Link {
                    from,
                    to,
                    kind,
                    confirmation,
                } => confirmation
                    .open(project)?
                    .evidence_link(&from, &to, &kind)?,
            },
        };
        Ok(writeln!(io::stdout().lock(), "{receipt}")?)
    }
}
