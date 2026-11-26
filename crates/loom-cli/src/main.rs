//! `loom` 命令行入口。

mod config;
mod database;
mod guide;
mod knowledge;
mod model;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use loom_core::context::{ContextRequest, DEFAULT_BUDGET, Mode};
use loom_core::import::import_oh_memos_wiki;
use loom_core::project::{Bind, ProjectLocation};
use loom_core::search::RecallQuery;
use loom_core::structure::Direction;
use loom_core::util::fmt_date;

#[derive(Parser)]
#[command(
    name = "loom",
    version,
    about = "Loom：给编码 Agent 用的项目记忆与理解服务"
)]
struct Cli {
    /// 项目根目录；未指定时找最近的已绑定父目录。初始化时必须明确指定。
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    /// 显式覆盖项目记住的数据库路径
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Invocation,
}

#[derive(Subcommand)]
enum Invocation {
    /// 输出写给助手的操作步骤：接入项目（默认）、搬库、启用模型，以及放进 AGENTS.md 的提示词
    Guide {
        #[arg(value_enum)]
        topic: Option<guide::Topic>,
    },
    /// 查看或修改本机用户配置，无需绑定项目
    #[command(subcommand)]
    Config(config::Config),
    /// 测试模型接口或下载本机向量模型，无需绑定项目
    #[command(subcommand)]
    Model(model::Model),
    #[command(flatten)]
    Project(Cmd),
}

#[derive(Subcommand)]
enum Cmd {
    /// 管理当前项目的库位置
    #[command(subcommand)]
    Db(database::Db),
    /// 明确选定根目录后初始化；普通命令不会自动建库
    Init {
        /// 确认这是属于该根目录的旧版无身份库
        #[arg(long, conflicts_with = "relocate")]
        adopt_legacy: bool,
        /// 确认原根已搬迁，须提供原项目的完整标识
        #[arg(long, value_name = "项目标识")]
        relocate: Option<String>,
    },
    /// 以 MCP stdio 服务运行（给 Claude Code / Codex 配置用）
    Serve,
    /// 导入外部记忆
    Import {
        /// 来源格式，目前只支持 oh-memos-wiki
        #[arg(long, default_value = "oh-memos-wiki")]
        from: String,
        /// wiki 目录（memos_export_wiki 的输出，或其中的 pages/）
        dir: PathBuf,
        /// 真正写入；不加时只预演并输出统计
        #[arg(long)]
        apply: bool,
    },
    /// 检索记忆；项目启用了向量检索时是关键词加向量，首行说明这次怎么查的
    Recall {
        query: Vec<String>,
        #[arg(short = 'k', long, default_value_t = 8)]
        top_k: usize,
    },
    /// 按状态和类型统计
    Stats,
    /// 更新当前项目的代码结构索引
    Index {
        /// 以 JSON 输出报告
        #[arg(long)]
        json: bool,
    },
    /// 查询目标文件引用了哪些文件
    Explain(StructureQuery),
    /// 查询哪些文件引用了目标文件
    Impact(StructureQuery),
    /// 读或改一个文件前，一次取齐它的约束、依赖或使用方、职责、相关经验和缺口（只读）
    Context {
        /// 目标文件相对项目根的路径
        target: String,
        /// explain 查依赖，impact 查使用方
        #[arg(long, value_name = "explain|impact")]
        mode: Mode,
        /// 空格分隔的经验关键词，最多 512 字节；不带空格的中文整句只匹配一个词
        #[arg(long)]
        question: Option<String>,
        /// 沿引用关系查询的深度（1 至 3）
        #[arg(long, default_value_t = 1)]
        depth: usize,
        /// 最终文本的预算，按 UTF-8 字节计，不是词元（1 至 6144）
        #[arg(long, default_value_t = DEFAULT_BUDGET, allow_negative_numbers = true)]
        budget: i64,
        /// 以 JSON 输出同一份报告及完整结构证据
        #[arg(long)]
        json: bool,
    },
    #[command(flatten)]
    Knowledge(knowledge::Knowledge),
}

#[derive(clap::Args)]
struct StructureQuery {
    /// 目标文件相对项目根的路径
    target: String,
    /// 沿引用关系查询的深度（1 至 3）
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=3))]
    depth: u8,
    /// 以 JSON 输出报告
    #[arg(long)]
    json: bool,
}

macro_rules! print_report {
    ($report:expr, $json:expr) => {{
        let report = $report;
        if $json {
            println!("{}", serde_json::to_string(&report)?);
        } else {
            println!("{report}");
        }
        Ok(())
    }};
}

impl StructureQuery {
    fn run(self, project: &ProjectLocation, direction: Direction) -> Result<()> {
        let store = project.open(false)?;
        let report = store.query_structure(&self.target, direction, self.depth.into())?;
        if self.json {
            println!("{}", serde_json::to_string(&report)?);
        } else {
            print!("{report}");
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    let cli = Cli::try_parse().unwrap_or_else(|e| config::parse_error(e));
    let cmd = match cli.cmd {
        Invocation::Guide { topic } => {
            print!("{}", topic.unwrap_or_default().text());
            return Ok(());
        }
        Invocation::Config(cmd) => return cmd.run(),
        Invocation::Model(cmd) => return cmd.run(),
        Invocation::Project(cmd) => cmd,
    };
    let cwd = std::env::current_dir().context("无法获取当前目录")?;
    if matches!(cmd, Cmd::Serve) {
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(loom_mcp::serve_stdio(cwd, cli.root, cli.db));
    }
    if matches!(&cmd, Cmd::Init { .. }) {
        anyhow::ensure!(cli.root.is_some(), "初始化、认领或搬迁须显式指定 --root");
    }
    let project = ProjectLocation::select(&cwd, cli.root.as_deref(), cli.db.as_deref())?;

    match cmd {
        Cmd::Db(cmd) => cmd.run(&project),
        Cmd::Init {
            adopt_legacy,
            relocate,
        } => {
            let mode = match (adopt_legacy, relocate.as_deref()) {
                (_, Some(id)) => Bind::Relocate(id),
                (true, None) => Bind::AdoptLegacy,
                (false, None) => Bind::Initialize,
            };
            println!("{}", database::bind(&project, mode)?.heading());
            Ok(())
        }
        Cmd::Serve => unreachable!(),
        Cmd::Import { from, dir, apply } => {
            anyhow::ensure!(from == "oh-memos-wiki", "不支持的来源: {from}");
            let store = project.open(apply)?;
            let r = import_oh_memos_wiki(&store, &dir, apply)?;
            println!(
                "{}扫描 {} 页，解析 {}，新增 {}，重复 {}，跳过 {}，锚点 {}，脱敏 {} 处",
                if r.dry_run { "[预演] " } else { "" },
                r.scanned,
                r.parsed,
                r.created,
                r.duplicate,
                r.skipped.len(),
                r.anchors,
                r.redactions
            );
            for (f, why) in r.skipped.iter().take(20) {
                println!("  跳过 {f}: {why}");
            }
            if r.dry_run {
                println!(
                    "确认无误后加 --apply 写入：{}",
                    project.database().display()
                );
            }
            Ok(())
        }
        Cmd::Recall { query, top_k } => {
            let store = project.open(false)?;
            let query = RecallQuery {
                query: query.join(" "),
                top_k,
                ..Default::default()
            };
            let found = loom_core::enhance::hybrid::recall(&store, &query, model::configured)?;
            print!("{}", found.heading);
            let hits = found.hits;
            if hits.is_empty() {
                println!("没有找到相关记忆。");
            }
            for (i, h) in hits.iter().enumerate() {
                println!(
                    "[{}] {:.3} {} {} · {} · {:?}/{:?} · id={}",
                    i + 1,
                    h.score,
                    h.memory.mem_type,
                    h.memory.title,
                    fmt_date(h.memory.updated_at),
                    h.freshness,
                    h.drift,
                    h.memory.id
                );
                if let Some(note) = h.fold_note() {
                    println!("    {note}");
                }
            }
            Ok(())
        }
        Cmd::Index { json } => print_report!(project.open(true)?.index_structure()?, json),
        Cmd::Explain(query) => query.run(&project, Direction::Outgoing),
        Cmd::Impact(query) => query.run(&project, Direction::Incoming),
        Cmd::Context {
            target,
            mode,
            question,
            depth,
            budget,
            json,
        } => {
            let report = project.open(false)?.context(ContextRequest {
                target,
                mode,
                question,
                depth,
                budget,
            })?;
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                print!("{}", report.text);
            }
            Ok(())
        }
        Cmd::Knowledge(cmd) => cmd.run(&project),
        Cmd::Stats => {
            let store = project.open(false)?;
            println!("{}", store.project().context("缺少项目身份")?.heading());
            database::print_location(project.database())?;
            for (status, ty, n) in store.stats()? {
                println!("{status:<10} {ty:<14} {n}");
            }
            println!("合计 {}", store.count()?);
            Ok(())
        }
    }
}
