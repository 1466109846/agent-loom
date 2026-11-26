//! Loom MCP 工具层（stdio）。
//!
//! 输出刻意用紧凑文本而不是 JSON：省 token，Agent 读起来也直接。
//! 正常状态（fresh / clean）不输出标记，只有需要注意时才标出来。

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use loom_core::context::{ContextRequest, DEFAULT_BUDGET};
use loom_core::memory::{Drift, RememberOutcome};
use loom_core::search::{Hit, RecallQuery};
use loom_core::structure::Direction;
use loom_core::util::{fmt_date, now_ts, truncate_chars};
use loom_core::{MemoryType, NewMemory, Store};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::schemars::{self, JsonSchema};
use rmcp::{tool, tool_router};
use serde::Deserialize;

mod knowledge;
mod session;
mod snapshot;

pub use session::serve_stdio;

// 请求取消后 spawn_blocking 仍可能在运行；先关库，再把轮次交给下一个请求。
struct CallStore {
    db: Store,
    _turn: tokio::sync::OwnedMutexGuard<session::Turn>,
}

#[derive(Clone)]
struct LoomServer {
    store: Arc<Mutex<CallStore>>,
    delivered_snapshot: Arc<Mutex<Option<String>>>,
    newly_bound: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RememberArgs {
    /// 记忆类型：DECISION / BUGFIX / GOTCHA / ERROR_PATTERN / CONFIG / FEATURE / MILESTONE / PROGRESS / CODE_PATTERN / SYNTHESIS
    #[serde(rename = "type")]
    pub mem_type: String,
    /// 一行标题，≤120 字符
    pub title: String,
    /// 正文：背景、结论、原因，≤2000 字符
    pub body: String,
    /// 相关源码，相对项目根，可带符号：`src/a.rs` 或 `src/a.rs#Foo::bar`
    #[serde(default)]
    pub anchors: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// 1-5，默认 2
    pub importance: Option<i64>,
    /// 已看过相似候选、确认要新建时设为 true
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecallArgs {
    /// 关键词，空格分隔；中英文都可以。为空时返回最近的记忆
    #[serde(default)]
    pub query: String,
    /// 只看这些类型
    #[serde(default)]
    pub types: Vec<String>,
    /// 默认 8，最多 50
    pub top_k: Option<usize>,
    /// 显示完整正文（默认截断到 240 字符）
    #[serde(default)]
    pub full: bool,
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResumeArgs {
    /// 回看多少小时，默认 24
    pub hours: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StructureQueryArgs {
    /// 目标文件相对项目根的路径
    pub target: String,
    /// 沿引用关系查询的深度，默认 1
    #[serde(default = "default_depth")]
    #[schemars(range(min = 1, max = 3))]
    pub depth: usize,
}

fn default_depth() -> usize {
    1
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ContextArgs {
    /// 项目内的相对文件路径。
    target: String,
    /// explain 查依赖，impact 查使用方。
    #[schemars(extend("enum" = ["explain", "impact"]))]
    mode: String,
    /// 空格分隔的关键词；不带空格的中文整句算一个词。最多 512 个 UTF-8 字节。
    question: Option<String>,
    #[serde(default = "default_depth")]
    #[schemars(range(min = 1, max = 3))]
    depth: usize,
    /// 默认 1500 个 UTF-8 字节。
    #[schemars(range(min = 1, max = 6144))]
    budget: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ManageArgs {
    /// get / archive / activate / supersede / delete / stats
    pub action: String,
    /// 目标记忆 id（supersede 时为旧记忆）
    #[serde(default)]
    pub ids: Vec<String>,
    /// supersede：取代旧记忆的新记忆 id
    pub by: Option<String>,
    /// delete 必须为 true
    #[serde(default)]
    pub confirm: bool,
}

fn marks(h: &Hit) -> String {
    h.marks()
}

fn render_hits(hits: &[Hit], full: bool, mut out: String) -> String {
    if !full {
        return knowledge::recall_preview(hits, out);
    }
    if hits.is_empty() {
        out.push_str("没有找到相关记忆。");
        return out;
    }
    for (i, h) in hits.iter().enumerate() {
        let m = &h.memory;
        let _ = writeln!(
            out,
            "[{}] {} {} · {} · id={}{}",
            i + 1,
            m.mem_type,
            m.title,
            fmt_date(m.updated_at),
            m.id,
            marks(h)
        );
        for line in m.body.lines().filter(|l| !l.trim().is_empty()) {
            let _ = writeln!(out, "    {line}");
        }
        if !h.anchors.is_empty() {
            let _ = writeln!(out, "    ↳ {}", h.anchors.join(", "));
        }
    }
    if hits.iter().any(|h| matches!(h.drift, Drift::Drift | Drift::Orphaned)) {
        out.push_str("\n注：code_drift 表示锚定的文件在保存后改过，orphaned 表示文件已不存在；采信前先核对代码。\n");
    }
    out
}

fn parse_types(raw: &[String]) -> Result<Vec<MemoryType>, String> {
    raw.iter().map(|t| t.parse::<MemoryType>().map_err(|e| e.to_string())).collect()
}

impl LoomServer {
    #[cfg(test)]
    fn new(db: Store) -> Self {
        let turn = Arc::new(tokio::sync::Mutex::new(session::Turn::default()))
            .try_lock_owned()
            .unwrap();
        Self {
            store: Arc::new(Mutex::new(CallStore { db, _turn: turn })),
            delivered_snapshot: Arc::default(),
            newly_bound: false,
        }
    }

    fn with_store<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T, String> {
        let store = self
            .store
            .lock()
            .map_err(|_| "存储锁已损坏，请重启 loom".to_string())?;
        f(&store.db).map_err(|e| format!("{e:#}"))
    }

    async fn structure_report<R: std::fmt::Display>(
        &self,
        work: impl FnOnce(&Store) -> Result<R> + Send + 'static,
    ) -> Result<String, String> {
        let server = self.clone();
        tokio::task::spawn_blocking(move || server.with_store(work).map(|report| report.to_string()))
            .await
            .map_err(|e| format!("结构任务未完成：{e}"))?
    }

    async fn query_report(&self, a: StructureQueryArgs, direction: Direction) -> Result<String, String> {
        self.structure_report(move |s| Ok(s.query_structure(&a.target, direction, a.depth)?.text(1_500))).await
    }
}

#[tool_router]
impl LoomServer {
    #[tool(description = "按需取本项目开工概览；如果本会话已经交付，只返回原快照编号。")]
    async fn overview(&self) -> Result<String, String> {
        self.snapshot_report(false, 2_000).await
    }

    #[tool(description = "显式重选当前有效目标、红线和置顶条目并返回新快照，不刷新源码或复核职责。")]
    async fn refresh(&self) -> Result<String, String> {
        self.snapshot_report(true, 2_000).await
    }

    #[tool(description = "项目意图：list；get 需要 id；propose 需要 kind、title、body。提议不生效，MCP 不能确认、撤销或取代。")]
    async fn intent(&self, Parameters(a): Parameters<knowledge::IntentArgs>) -> Result<String, String> {
        self.knowledge_task(move |s| a.run(s)).await
    }

    #[tool(description = "人工文件职责：get（默认）按 path 查看详情；set 须给出已核对的 expected_sha 和新 role。不能代为复核。")]
    async fn note(&self, Parameters(a): Parameters<knowledge::NoteArgs>) -> Result<String, String> {
        self.knowledge_task(move |s| a.run(s)).await
    }

    #[tool(description = "任务画布：list 列未完成任务；get/close 需要 id；open 需要 goal、next、evidence；update 需要 id、next、evidence。")]
    async fn canvas(&self, Parameters(a): Parameters<knowledge::CanvasArgs>) -> Result<String, String> {
        self.knowledge_task(move |s| a.run(s)).await
    }

    #[tool(description = "只读证据包：按问题查直接记录、已明确声明的支持或冲突关系、复核提示与缺口；不推断或写入关系。", annotations(read_only_hint = true))]
    async fn evidence(&self, Parameters(a): Parameters<knowledge::QueryArgs>) -> Result<String, String> {
        self.knowledge_task(move |s| s.evidence_pack(&a.query).map(knowledge::report_preview)).await
    }

    #[tool(description = "对关键词候选按需重排；本机增强默认关闭，未启用或不可用时回退关键词结果。", annotations(read_only_hint = true))]
    async fn rerank(&self, Parameters(a): Parameters<knowledge::QueryArgs>) -> Result<String, String> {
        self.knowledge_task(move |s| loom_core::enhance::rerank(s, &a.query).map(knowledge::report_preview)).await
    }

    #[tool(description = "只在本项目人工授权集内跨措辞查找；没有模型时在同一集合内用关键词查找。不能授权或撤权。", annotations(read_only_hint = true))]
    async fn semantic(&self, Parameters(a): Parameters<knowledge::QueryArgs>) -> Result<String, String> {
        self.knowledge_task(move |s| loom_core::enhance::semantic::find(s, &a.query).map(knowledge::report_preview)).await
    }

    #[tool(description = "仅提炼本次显式提交的 text；本机模型默认关闭，建议待人工审阅，不写入记忆或确认意图。", annotations(read_only_hint = true))]
    async fn extract(&self, Parameters(a): Parameters<knowledge::TextArgs>) -> Result<String, String> {
        self.knowledge_task(move |s| loom_core::enhance::extract(s, &a.text).map(knowledge::report_preview)).await
    }

    #[tool(description = "更新当前项目的代码结构索引。源码变动后再次调用以刷新引用关系。")]
    async fn index(&self) -> Result<String, String> {
        self.structure_report(Store::index_structure).await
    }

    #[tool(description = "从已有索引查询目标文件引用了哪些文件，不更新索引。")]
    async fn explain(&self, Parameters(a): Parameters<StructureQueryArgs>) -> Result<String, String> {
        self.query_report(a, Direction::Outgoing).await
    }

    #[tool(description = "从已有索引查询哪些文件引用了目标文件，不更新索引。")]
    async fn impact(&self, Parameters(a): Parameters<StructureQueryArgs>) -> Result<String, String> {
        self.query_report(a, Direction::Incoming).await
    }

    #[tool(
        description = "读或改一个文件前，一次取齐它的约束、依赖（explain）或使用方（impact）、职责、相关经验和缺口；只读，不更新索引。预算按 UTF-8 字节计，不是词元；question 是空格分隔的关键词，不带空格的中文整句算一个词。",
        annotations(read_only_hint = true)
    )]
    async fn context(&self, Parameters(a): Parameters<ContextArgs>) -> Result<String, String> {
        self.knowledge_task(move |s| {
            Ok(s.context(ContextRequest {
                target: a.target,
                mode: a.mode.parse()?,
                question: a.question,
                depth: a.depth,
                budget: a.budget.unwrap_or(DEFAULT_BUDGET),
            })?
            .text)
        })
        .await
    }

    #[tool(description = "保存一条记忆。只存读代码推断不出来、又能防止出错的信息（决策原因、坑、修复根因、约束）。\
内容完全重复会合并；有相似条目时会返回候选，确认要新建再加 force=true。凭据会自动脱敏。")]
    async fn remember(&self, Parameters(a): Parameters<RememberArgs>) -> Result<String, String> {
        let ty: MemoryType = a.mem_type.parse().map_err(|e: anyhow::Error| e.to_string())?;
        let out = self.knowledge_task(move |s| {
            s.remember(NewMemory {
                mem_type: Some(ty),
                title: a.title,
                body: a.body,
                tags: a.tags,
                importance: a.importance,
                anchors: a.anchors,
                force: a.force,
                ..Default::default()
            })
        }).await?;
        Ok(match out {
            RememberOutcome::Created { id, notes, .. } => {
                let mut s = format!("已保存 id={id}");
                for n in notes {
                    let _ = write!(s, "\n提示：{n}");
                }
                s
            }
            RememberOutcome::Duplicate { id } => format!("内容与已有记忆相同，未新建：id={id}"),
            RememberOutcome::Similar { candidates } => {
                let mut s = String::from("有相似记忆，未保存。请先确认：\n");
                for (id, t) in candidates {
                    let _ = writeln!(s, "- id={id} {t}");
                }
                s.push_str("如果是同一件事的新结论，先保存新条目（force=true），再用 manage supersede 取代旧条目；确实是不同的事则 force=true 重试。");
                s
            }
            RememberOutcome::Rejected { reason } => return Err(format!("未保存：{reason}")),
        })
    }

    #[tool(description = "检索记忆（关键词，中英文都可）。结果按相关度、重要度、新鲜度和代码漂移综合排序；\
⚠stale/expired 表示时间久了，⚠code_drift 表示锚定的代码改过，需要复核。")]
    async fn recall(&self, Parameters(a): Parameters<RecallArgs>) -> Result<String, String> {
        let types = parse_types(&a.types)?;
        let q = RecallQuery {
            query: a.query,
            types,
            top_k: a.top_k.unwrap_or(8),
            include_archived: a.include_archived,
        };
        self.knowledge_task(move |s| {
            let found = loom_core::enhance::hybrid::recall(s, &q, || {
                loom_core::enhance::models::Models::from_config(
                    loom_core::config::UserConfig::load(),
                )
            })?;
            Ok(render_hits(&found.hits, a.full, found.heading))
        })
        .await
    }

    #[tool(description = "恢复现场：先核对项目身份，再看未完成任务、下一步和证据，最后看近期记忆标题与需复核项；整份最多 1500 个 UTF-8 字节。")]
    async fn resume(&self, Parameters(a): Parameters<ResumeArgs>) -> Result<String, String> {
        let hours = a.hours.unwrap_or(24).clamp(1, 24 * 30);
        let newly_bound = self.newly_bound;
        self.knowledge_task(move |s| knowledge::resume(s, hours, newly_bound))
            .await
    }

    #[tool(description = "管理记忆：get（看全文和锚点）/ archive / activate / supersede（ids=[旧], by=新）/ delete（需 confirm=true，优先用 archive）/ stats。")]
    async fn manage(&self, Parameters(a): Parameters<ManageArgs>) -> Result<String, String> {
        self.knowledge_task(move |s| match a.action.as_str() {
            "get" => {
                let mut out = String::new();
                for id in &a.ids {
                    let Some(detail) = s.memory_detail(id)? else {
                        let _ = writeln!(out, "id={id} 不存在");
                        continue;
                    };
                    out.push_str(&detail);
                    out.push('\n');
                }
                Ok(out)
            }
            "archive" => Ok(format!("已归档 {} 条", s.set_status(&a.ids, "archived")?)),
            "activate" => Ok(format!("已恢复 {} 条", s.set_status(&a.ids, "active")?)),
            "supersede" => {
                let by = a.by.as_deref().ok_or_else(|| anyhow::anyhow!("supersede 需要 by=新记忆 id"))?;
                for old in &a.ids {
                    s.supersede(old, by)?;
                }
                Ok(format!("{} 条旧记忆已被 {by} 取代", a.ids.len()))
            }
            "delete" => {
                anyhow::ensure!(a.confirm, "删除不可恢复，确认后加 confirm=true；只是不再适用的话用 archive");
                Ok(format!("已删除 {} 条", s.delete(&a.ids)?))
            }
            "stats" => {
                let mut out = String::new();
                for (status, ty, n) in s.stats()? {
                    let _ = writeln!(out, "{status:<10} {ty:<14} {n}");
                }
                if out.is_empty() {
                    out.push_str("库是空的。");
                }
                Ok(out)
            }
            other => anyhow::bail!("未知 action: {other}"),
        }).await
    }
}
