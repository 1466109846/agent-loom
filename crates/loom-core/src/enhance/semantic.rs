//! 本地人工授权集内的跨措辞查找。不依赖全库关键词召回，也不保存向量。
use super::{CANDIDATES as LIMIT, MODEL_TIMEOUT, Model, checked, embed_scores, text};
use crate::Store;
use anyhow::Result;
use rusqlite::{Connection, params};
use std::time::{Duration, Instant};

mod grants;
use grants::members;
pub use grants::{allow, deny};

const TOP: usize = 5;
const EMPTY: &str = "来源：本项目语义授权集。无授权的有效记忆，未调用模型。";
const CHANGED: &str = "本项目语义授权集或标题已变化；旧结果已丢弃，请重新查询。";

#[derive(PartialEq, Eq)]
struct Member {
    id: String,
    title: Option<String>,
}

fn append(out: &mut String, m: &Member) {
    out.push_str(&format!(
        "\n- {} {}",
        m.id,
        m.title.as_deref().unwrap_or("（标题已省略）")
    ));
}

/// 只读展示本项目有效授权，不创建表或启动模型。
pub fn list(store: &Store) -> Result<String> {
    checked(store, || {
        let rows = members(&store.conn)?;
        if rows.is_empty() {
            return Ok(EMPTY.into());
        }
        let mut out = format!("来源：本项目语义授权集（{}/{LIMIT}）。", rows.len());
        for row in &rows {
            append(&mut out, row);
        }
        Ok(out)
    })
}

/// 只在授权编号上做本地字面匹配。先按编号查一条，再查其标题、正文和标签；
/// 正文不离开 SQLite，集外条目的关键词排名不参与。只绑定参数，% 和 _ 也按字面处理。
fn lexical(conn: &Connection, rows: &[Member], query: &str, why: &str) -> Result<String> {
    let mut st = conn.prepare_cached("SELECT CASE WHEN instr(lower(title), lower(?2))>0 THEN 3
        WHEN instr(lower(tags),lower(?2))>0 THEN 2 WHEN instr(lower(body),lower(?2))>0 THEN 1 ELSE 0 END
        FROM main.memories WHERE id=?1 AND status='active'")?;
    let mut hits = Vec::new();
    let terms: std::collections::BTreeSet<_> = query.split_whitespace().collect();
    for m in rows {
        let mut score = 0i64;
        for term in &terms {
            score += st.query_row(params![m.id, term], |r| r.get::<_, i64>(0))?;
        }
        if score > 0 {
            hits.push((score, m));
        }
    }
    hits.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    let mut out = format!(
        "关键词回退：{why}。来源：本项目授权集（{} 条）的本地关键词匹配，最多 {TOP} 条。",
        rows.len()
    );
    if hits.is_empty() {
        out.push_str("\n授权集内无关键词命中。");
    }
    for (_, m) in hits.into_iter().take(TOP) {
        append(&mut out, m);
    }
    Ok(out)
}

enum Search<'a> {
    Report(String),
    Batch(Vec<Member>, &'a str, std::num::NonZeroU16),
}

/// 不把关键词命中作为前提；无授权、模型未启用时不联网。只返回候选，不代表结论已证实。
pub fn find(store: &Store, query: &str) -> Result<String> {
    find_with(
        store,
        query,
        Model::from_env("LOOM_EMBED_MODEL"),
        MODEL_TIMEOUT,
    )
}

pub(super) fn find_with(
    store: &Store,
    query: &str,
    model: Model,
    wait: Duration,
) -> Result<String> {
    let end = Instant::now() + wait;
    let masked = text::input(query, 512)?;
    let initial = checked(store, || {
        let rows = members(&store.conn)?;
        if rows.is_empty() {
            return Ok(Search::Report(EMPTY.into()));
        }
        match model.endpoint() {
            Ok((name, port)) if rows.iter().all(|r| r.title.is_some()) => {
                Ok(Search::Batch(rows, name, port))
            }
            endpoint => Ok(Search::Report(lexical(
                &store.conn,
                &rows,
                query,
                endpoint.err().unwrap_or("标题无效或过长，未发送"),
            )?)),
        }
    })?;
    let (rows, name, port) = match initial {
        Search::Report(report) => return Ok(report),
        Search::Batch(rows, name, port) => (rows, name, port),
    };
    // 不持有 SQLite 事务或写锁等待模型。等待中撤权、归档、改标题均使整批旧响应失效。
    let scores = embed_scores(
        name,
        port,
        &masked,
        rows.iter().filter_map(|r| r.title.as_deref()),
        end,
    );
    checked(store, || {
        if members(&store.conn)? != rows {
            return Ok(CHANGED.into());
        }
        let Some(scores) = scores.filter(|_| Instant::now() < end) else {
            return lexical(&store.conn, &rows, query, "模型不可用或返回无效");
        };
        let mut hits: Vec<_> = scores
            .into_iter()
            .zip(&rows)
            .filter(|(s, _)| *s > 0.0)
            .collect();
        hits.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut out = format!(
            "授权集内语义召回。来源：本项目 {} 条已授权记忆的脱敏标题，按正余弦相似度列出最多 {TOP} 个候选。",
            rows.len()
        );
        if hits.is_empty() {
            out.push_str("\n没有正相似度候选。");
        }
        for (_, m) in hits.into_iter().take(TOP) {
            append(&mut out, m);
        }
        Ok(out)
    })
}
