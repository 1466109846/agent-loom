//! 项目级混合检索。旧增强的逐条授权与这里的端点授权完全独立。
use super::{
    checked,
    models::{Configured, Models, Service},
};
use crate::{
    Store,
    util::{fmt_date, fmt_size, new_id, now_ts},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};

mod recall;
mod vectors;
pub use recall::{Recall, recall};
use vectors::{inventory, model_key};

const CHANGED: &str = "模型地址已变更，需重新执行 loom semantic enable";
const SETUP: &str = "未配置向量模型：请执行 loom config set embedding.base_url … 和 loom config set embedding.model …";
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS project_semantic (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), generation TEXT NOT NULL, enabled_at INTEGER NOT NULL,
 embedding TEXT NOT NULL, rerank TEXT);
 CREATE TABLE IF NOT EXISTS memory_vectors(memory_id TEXT PRIMARY KEY, model_key TEXT NOT NULL,
 digest TEXT NOT NULL, dim INTEGER NOT NULL, vector BLOB NOT NULL, embedded_at INTEGER NOT NULL);
 CREATE INDEX IF NOT EXISTS memory_vectors_model ON memory_vectors(model_key);";

fn table(s: &Store, name: &str) -> Result<bool> {
    Ok(s.conn
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE type='table' AND name=?1)",
        )?
        .query_row([name], |r| r.get(0))?)
}

#[derive(PartialEq, Eq)]
struct Consent {
    generation: String,
    time: i64,
    embedding: String,
    rerank: Option<String>,
}
fn consent(s: &Store) -> Result<Option<Consent>> {
    if !table(s, "project_semantic")? {
        return Ok(None);
    }
    Ok(s.conn.prepare_cached("SELECT generation,enabled_at,embedding,rerank FROM main.project_semantic WHERE singleton=1")?.query_row([], |r| Ok(Consent { generation:r.get(0)?, time:r.get(1)?, embedding:r.get(2)?, rerank:r.get(3)? })).optional()?)
}
fn authorized(s: &Store, grant: &Consent) -> Result<bool> {
    checked(s, || Ok(consent(s)?.as_ref() == Some(grant)))
}
fn embedding(m: &Models) -> Result<&Service> {
    m.embedding
        .as_ref()
        .map_err(|e| *e)?
        .as_ref()
        .context(SETUP)
}
fn describe(c: &Configured) -> String {
    match c {
        Ok(Some(s)) => format!("{}，密钥来源：{}", s.description(), s.key_source()),
        Ok(None) => "未配置".into(),
        Err(e) => e.to_string(),
    }
}

pub fn preview(s: &Store, models: &Models) -> Result<String> {
    let model = embedding(models)?;
    checked(s, || {
        let n: i64 = s.conn.query_row(
            "SELECT count(*) FROM main.memories WHERE status='active'",
            [],
            |r| r.get(0),
        )?;
        // 没配维度时要等第一次请求才知道；预演不发请求，按常见的 1024 维给个量级。
        let space = model.dimensions().map_or_else(
            || format!("维度未配置，按 1024 维估算约 {}", fmt_size(n as u64 * 4096)),
            |d| format!("约 {}", fmt_size(n as u64 * d as u64 * 4)),
        );
        Ok(format!(
            "[预演] 向量：{}\n重排：{}\n发送：脱敏查询词、有效经验的脱敏标题和正文（每条最多 2000 字符；重排最多 24 条）。\n不发送：记忆编号、锚点、文件路径、标签、项目名、根目录、项目标识、源码、意图、文件职责、任务画布。\n有效经验 {n} 条；向量空间：{space}。\n确认后执行 loom semantic enable --yes",
            describe(&models.embedding),
            describe(&models.rerank)
        ))
    })
}

pub fn enable(s: &Store, models: &Models) -> Result<()> {
    let embed = embedding(models)?;
    let rank = models
        .rerank
        .as_ref()
        .map_err(|e| *e)?
        .as_ref()
        .map(Service::origin);
    checked(s, || {
        s.conn.execute_batch(SCHEMA)?;
        s.conn.execute(
            "INSERT OR REPLACE INTO main.project_semantic VALUES(1,?1,?2,?3,?4)",
            params![new_id(), now_ts(), embed.origin(), rank],
        )?;
        Ok(())
    })
}
pub fn disable(s: &Store, purge: bool) -> Result<String> {
    checked(s, || {
        if table(s, "project_semantic")? {
            s.conn.execute("DELETE FROM main.project_semantic", [])?;
        }
        if purge && table(s, "memory_vectors")? {
            let n = s.conn.execute("DELETE FROM main.memory_vectors", [])?;
            Ok(format!("已停用；清理 {n} 条向量。"))
        } else {
            Ok("已停用；保留已有向量。".into())
        }
    })
}
pub fn status(s: &Store, m: &Models) -> Result<String> {
    checked(s, || {
        let grant = consent(s)?;
        let state = grant.as_ref().map_or_else(
            || "未启用".into(),
            |g| format!("已启用（{}）", fmt_date(g.time)),
        );
        let key = model_key(s, m.embedding.as_ref().ok().and_then(Option::as_ref))?;
        let counts = inventory(s, key.as_deref(), None, 0, true)?;
        let mut out = format!(
            "{state}\n向量：{}\n重排：{}\n有效经验 {}，已有当前向量 {}，待建或过期 {}，因敏感内容跳过 {}",
            describe(&m.embedding),
            describe(&m.rerank),
            counts.current + counts.pending + counts.sensitive,
            counts.current,
            counts.pending,
            counts.sensitive
        );
        if let Some(g) = grant {
            for (label, config, bound) in [
                ("向量", &m.embedding, Some(g.embedding.as_str())),
                ("重排", &m.rerank, g.rerank.as_deref()),
            ] {
                let same = config
                    .as_ref()
                    .is_ok_and(|c| c.as_ref().map(Service::origin) == bound);
                out.push_str(&format!(
                    "\n{label}同意地址：{}；{}",
                    bound.unwrap_or("无"),
                    if same { "一致" } else { CHANGED }
                ));
            }
        }
        Ok(out)
    })
}

#[derive(Default)]
pub struct Indexed {
    pub new: usize,
    pub updated: usize,
    pub skipped: usize,
    pub failed: usize,
    pub removed: usize,
    pub reason: Option<super::models::Failure>,
}
impl std::fmt::Display for Indexed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "新建 {}，更新 {}，跳过（敏感）{}，失败 {}，清理 {}",
            self.new, self.updated, self.skipped, self.failed, self.removed
        )?;
        if let Some(e) = self.reason {
            write!(f, "；{e}")?;
        }
        Ok(())
    }
}

pub fn index(
    s: &Store,
    models: &Models,
    limit: Option<usize>,
    mut progress: impl FnMut(&Indexed),
) -> Result<Indexed> {
    let model = embedding(models)?;
    let (grant, removed) = checked(s, || {
        let grant = consent(s)?.context("项目未启用，请先执行 loom semantic enable")?;
        ensure!(grant.embedding == model.origin(), CHANGED);
        let removed=s.conn.execute("DELETE FROM main.memory_vectors WHERE NOT EXISTS(SELECT 1 FROM main.memories m WHERE m.id=memory_vectors.memory_id AND m.status='active')",[])?;
        Ok((grant, removed))
    })?;
    let mut result = Indexed {
        removed,
        ..Default::default()
    };
    let mut left = limit.unwrap_or(usize::MAX);
    let mut cursor = None;
    while left > 0 {
        let (page, expected) = checked(s, || {
            ensure!(
                consent(s)?.as_ref() == Some(&grant),
                "项目授权已变化，已停止建向量"
            );
            let key = model_key(s, Some(model))?;
            Ok((
                inventory(
                    s,
                    key.as_deref(),
                    cursor.as_ref(),
                    left.min(model.batch()),
                    false,
                )?,
                key,
            ))
        })?;
        result.skipped += page.sensitive;
        if page.batch.is_empty() {
            break;
        }
        let input = page
            .batch
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>();
        ensure!(authorized(s, &grant)?, "项目授权已变化，未发送本批文本");
        let response = model.embed(&input);
        checked(s, || {
            ensure!(
                consent(s)?.as_ref() == Some(&grant),
                "项目授权已变化，已丢弃本批向量"
            );
            let vectors = response.and_then(|v| {
                let key = vectors::key(model, v[0].len());
                if expected.as_ref().is_some_and(|k| k != &key) {
                    Err(super::models::Failure::Format)
                } else {
                    Ok((key, v))
                }
            });
            match vectors {
                Ok((key, v)) => {
                    let (new, updated) = vectors::persist(s, &key, &page.batch, &v)?;
                    result.new += new;
                    result.updated += updated;
                    result.failed += page.batch.len() - new - updated;
                }
                Err(e) => {
                    result.failed += page.batch.len();
                    result.reason = Some(e);
                }
            }
            Ok(())
        })?;
        progress(&result);
        if result.reason.is_some() {
            break;
        }
        left -= page.batch.len();
        cursor = page.cursor;
    }
    Ok(result)
}
