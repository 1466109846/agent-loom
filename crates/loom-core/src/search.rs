//! 检索：FTS5（trigram）召回 + 重排（相关度 · 重要度 · 新鲜度 · 漂移）。

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::params;
use serde::Serialize;

use crate::db::Store;
use crate::memory::{Drift, Memory, MemoryType};
use crate::util::now_ts;

/// 相似标题判定阈值（字符 trigram 的 Jaccard 系数）。
pub const SIMILAR_THRESHOLD: f64 = 0.6;

#[derive(Debug, Clone, Default)]
pub struct RecallQuery {
    pub query: String,
    pub types: Vec<MemoryType>,
    pub top_k: usize,
    pub include_archived: bool,
}

impl RecallQuery {
    fn limit(&self) -> usize {
        if self.top_k == 0 {
            10
        } else {
            self.top_k.min(50)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Fresh,
    Stale,
    Expired,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub memory: Memory,
    pub score: f64,
    pub freshness: Freshness,
    pub drift: Drift,
    pub anchors: Vec<String>,
    /// 按原排序列出被折叠的本地编号，不包含代表本身。
    pub folded_ids: Vec<String>,
    /// 仅统计本次候选中的其它命中；超过编号预算也不占用另一个 top_k 名额。
    pub folded_count: usize,
}

const MAX_FOLDED_IDS: usize = 8;
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Selection {
    Recall,
    Context,
    Candidates,
}
const MAX_FOLD_ID_BYTES: usize = 64;

impl Hit {
    pub fn marks(&self) -> String {
        marks(self.freshness, self.drift, &self.memory.status)
    }

    /// 两个文本入口共用。只列有限个可直接复制的完整编号，不拼接标题或正文。
    pub fn fold_note(&self) -> Option<String> {
        if self.folded_count == 0 {
            return None;
        }
        let listed: Vec<_> = self.folded_ids.iter().take(MAX_FOLDED_IDS)
            .map(String::as_str).filter(|id| previewable_id(id)).collect();
        let mut note = format!("折叠 {} 条（可按编号取原文）：{}", self.folded_count, listed.join(", "));
        if self.folded_count > listed.len() {
            note.push_str("（其余编号已省略）");
        }
        Some(note)
    }
}

pub(crate) fn marks(freshness: Freshness, drift: Drift, status: &str) -> String {
    let mut marks = Vec::new();
    match freshness {
        Freshness::Fresh => (),
        Freshness::Stale => marks.push("stale"),
        Freshness::Expired => marks.push("expired"),
    }
    match drift {
        Drift::Clean => (),
        Drift::Drift => marks.push("code_drift"),
        Drift::Orphaned => marks.push("orphaned"),
        Drift::Unverified => marks.push("unverified"),
    }
    if status != "active" {
        marks.push(status);
    }
    if marks.is_empty() {
        String::new()
    } else {
        format!(" ⚠{}", marks.join(","))
    }
}

fn previewable_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_FOLD_ID_BYTES
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

// 哈希仅用于查找桶，键以完整字段判等，不凭摘要或 trigram 分数合并。
#[derive(PartialEq, Eq, Hash)]
struct FoldKey {
    title: String,
    body: String,
    ty: MemoryType,
    status: String,
    origin: String,
    tags: Vec<String>,
    baselines: Vec<(String, String, Option<String>)>,
}

/// 只为普通散文标题松动排版，连续半角空格变为一个，不消除或移动词间边界。
/// 含路径、数字、引号、运算符等的技术标题逐字比较；ASCII 点只在汉字后当作句号。
fn layout_title(raw: &str) -> String {
    let han = |c: char| matches!(c, '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}');
    let title = raw.trim_matches(' ');
    let stem = title.strip_suffix('。')
        .or_else(|| title.strip_suffix('.').filter(|s| s.ends_with(han)))
        .unwrap_or(title);
    if !stem.chars().all(|c| c == ' ' || han(c) || c.is_ascii_alphabetic()) {
        return raw.to_owned();
    }
    stem.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

impl FoldKey {
    fn new(m: &Memory, baselines: Vec<(String, String, Option<String>)>) -> Option<Self> {
        // legacy_id 在库中唯一；有旧页编号的一律独立，没有旧编号的导入也不猜来源。
        // 超长旧文本可照常检索，但不进入规范化/相似比对或编号预览。
        if m.legacy_id.is_some() || m.origin.trim().is_empty() || m.origin.starts_with("import:")
            || !previewable_id(&m.id)
            || !matches!(m.status.as_str(), "active" | "archived" | "superseded")
            || m.title.len() > 4 * crate::admission::MAX_TITLE_CHARS
            || m.body.len() > 4 * crate::admission::MAX_BODY_CHARS
            || !m.title.chars().any(char::is_alphanumeric) || !m.body.chars().any(char::is_alphanumeric)
        {
            return None;
        }
        Some(Self {
            title: layout_title(&m.title), body: m.body.clone(), ty: m.mem_type,
            status: m.status.clone(), origin: m.origin.clone(), tags: m.tags.clone(), baselines,
        })
    }
}

/// 按类型半衰期计算新鲜度与衰减系数。
pub fn freshness(ty: MemoryType, updated_at: i64, now: i64) -> (Freshness, f64) {
    let Some(hl) = ty.half_life_days() else {
        return (Freshness::Fresh, 1.0);
    };
    let age_days = ((now - updated_at).max(0) as f64) / 86_400.0;
    let ratio = age_days / hl;
    let label = if ratio <= 1.0 {
        Freshness::Fresh
    } else if ratio <= 3.0 {
        Freshness::Stale
    } else {
        Freshness::Expired
    };
    (label, 0.5f64.powf(ratio).max(0.3))
}

fn drift_penalty(d: Drift) -> f64 {
    match d {
        Drift::Clean => 1.0,
        Drift::Unverified => 0.95,
        Drift::Drift => 0.8,
        Drift::Orphaned => 0.6,
    }
}

/// 字符级 trigram 集合（忽略大小写与空白）。
fn trigrams(s: &str) -> HashSet<String> {
    let chars: Vec<char> = s
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect();
    if chars.len() < 3 {
        return std::iter::once(chars.iter().collect()).collect();
    }
    chars.windows(3).map(|w| w.iter().collect()).collect()
}

pub fn similarity(a: &str, b: &str) -> f64 {
    let (ta, tb) = (trigrams(a), trigrams(b));
    let inter = ta.intersection(&tb).count() as f64;
    let union = ta.union(&tb).count() as f64;
    if union == 0.0 { 0.0 } else { inter / union }
}

/// 把用户查询拆成 FTS5 表达式。trigram 分词要求每个词 ≥ 3 个字符，
/// 更短的词返回到 `short` 里走 LIKE。
fn build_fts_query(q: &str) -> (Option<String>, Vec<String>) {
    let mut long = Vec::new();
    let mut short = Vec::new();
    for term in q.split_whitespace() {
        let term = term.trim_matches(|c: char| "\"'`,，。;；:：()（）[]".contains(c));
        if term.is_empty() {
            continue;
        }
        if term.chars().count() >= 3 {
            long.push(format!("\"{}\"", term.replace('"', "\"\"")));
        } else {
            short.push(term.to_string());
        }
    }
    let expr = if long.is_empty() { None } else { Some(long.join(" OR ")) };
    (expr, short)
}

impl Store {
    /// 同类型、未删除的记忆中，标题与 `title` 相似的条目（id, title）。
    pub fn similar_titles(
        &self,
        title: &str,
        ty: MemoryType,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, title FROM memories WHERE type = ?1 AND status = 'active'",
        )?;
        let rows = st.query_map(params![ty.as_str()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut scored: Vec<(f64, String, String)> = Vec::new();
        for row in rows {
            let (id, t) = row?;
            let s = similarity(title, &t);
            if s >= SIMILAR_THRESHOLD {
                scored.push((s, id, t));
            }
        }
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        Ok(scored.into_iter().take(limit).map(|(_, id, t)| (id, t)).collect())
    }

    /// 检索记忆。同一次只读快照内核对正文、生命周期、来源和证据。
    pub fn recall(&self, q: &RecallQuery) -> Result<Vec<Hit>> {
        self.savepoint(|_| Ok(self.recall_in_snapshot(q, false)?.0))
    }

    pub(crate) fn recall_in_snapshot(
        &self,
        q: &RecallQuery,
        context: bool,
    ) -> Result<(Vec<Hit>, bool)> {
        let selection = if context {
            Selection::Context
        } else {
            Selection::Recall
        };
        self.keyword_in_snapshot(q, selection)
    }

    pub(crate) fn keyword_candidates(&self, q: &RecallQuery) -> Result<Vec<Hit>> {
        Ok(self.keyword_in_snapshot(q, Selection::Candidates)?.0)
    }

    fn keyword_in_snapshot(
        &self,
        q: &RecallQuery,
        selection: Selection,
    ) -> Result<(Vec<Hit>, bool)> {
        let context = selection == Selection::Context;
        let top_k = q.limit();
        let pool = (top_k * 5).max(50) as i64;
        let probe = pool + i64::from(context);
        let mut limited = false;
        let mut rel: HashMap<String, f64> = HashMap::new();

        let (expr, short) = build_fts_query(&q.query);
        if let Some(expr) = expr {
            let mut st = self.conn.prepare_cached(
                "SELECT key, bm25(fts, 0.0, 3.0, 1.0, 2.0) AS r FROM fts WHERE fts MATCH ?1 ORDER BY r, key LIMIT ?2",
            )?;
            let rows: Vec<(String, f64)> = st
                .query_map(params![expr, probe], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            limited |= rows.len() > pool as usize;
            // bm25 越小越相关（负数）；按最好的一条归一化到 (0, 1]。
            let best = rows.iter().map(|(_, s)| *s).fold(0.0f64, f64::min);
            for (key, s) in rows.into_iter().take(pool as usize) {
                if let Some(id) = key.strip_prefix("mem:") {
                    let v = if best < 0.0 { (s / best).clamp(0.05, 1.0) } else { 0.5 };
                    rel.insert(id.to_string(), v);
                }
            }
        }
        for term in &short {
            let pat = format!("%{}%", term.replace('%', "\\%").replace('_', "\\_"));
            let mut st = self.conn.prepare_cached(
                "SELECT id FROM memories WHERE title LIKE ?1 ESCAPE '\\' OR body LIKE ?1 ESCAPE '\\' ORDER BY id LIMIT ?2",
            )?;
            let ids: Vec<String> = st
                .query_map(params![pat, probe], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            limited |= ids.len() > pool as usize;
            for id in ids.into_iter().take(pool as usize) {
                *rel.entry(id).or_insert(0.0) += 0.4;
            }
        }

        let memories: Vec<(Memory, f64)> = if q.query.trim().is_empty() {
            // 与 recent 相同的有效状态、时间和窗口；在 LIMIT 之前明确并列顺序。
            let sql = format!("SELECT {} FROM memories WHERE status='active' AND updated_at>=0
                ORDER BY updated_at DESC, id LIMIT ?1", Memory::COLUMNS);
            let mut st = self.conn.prepare_cached(&sql)?;
            st.query_map([pool], |r| Ok((Memory::from_row(r)?, 1.0)))?
                .collect::<rusqlite::Result<_>>()?
        } else {
            let mut out = Vec::with_capacity(rel.len());
            for (id, r) in &rel {
                if let Some(m) = self.get(id)? {
                    out.push((m, r.min(1.0)));
                }
            }
            out
        };

        Ok((self.score_candidates(q, memories, selection)?, limited))
    }

    pub(crate) fn score_candidates(
        &self,
        q: &RecallQuery,
        memories: Vec<(Memory, f64)>,
        selection: Selection,
    ) -> Result<Vec<Hit>> {
        let context = selection == Selection::Context;
        let now = now_ts();
        let mut hits = Vec::new();
        for (m, r) in memories {
            if m.status == "deleted" || (!q.include_archived && m.status != "active") {
                continue;
            }
            if !q.types.is_empty() && !q.types.contains(&m.mem_type) {
                continue;
            }
            let (fresh, decay) = freshness(m.mem_type, m.updated_at, now);
            let drift = self.drift(&m.id)?;
            let score = r * (0.6 + 0.1 * m.importance as f64) * decay * drift_penalty(drift);
            let mut baselines: Vec<_> = self.anchors(&m.id)?.into_iter()
                .map(|a| (a.path, a.symbol, a.sha256_at_save)).collect();
            baselines.sort_unstable();
            // context 只比对原路径；path#symbol 是给 recall 看的展示串，不是定位键。
            let anchors = baselines
                .iter()
                .map(|(path, symbol, _)| {
                    if context || symbol.is_empty() {
                        path.clone()
                    } else {
                        format!("{path}#{symbol}")
                    }
                })
                .collect();
            let key = if selection == Selection::Candidates {
                None
            } else {
                FoldKey::new(&m, baselines)
            };
            let hit = Hit { memory: m, score, freshness: fresh, drift, anchors,
                folded_ids: Vec::new(), folded_count: 0 };
            hits.push((hit, key));
        }
        hits.sort_unstable_by(|(a, _), (b, _)| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| b.memory.updated_at.cmp(&a.memory.updated_at))
                .then_with(|| a.memory.id.cmp(&b.memory.id))
        });
        // 组合查询在去掉目标锚点后才取前五；不扩大任一关键词的候选窗口。
        let keep = if context { hits.len() } else { q.limit() };
        if selection == Selection::Candidates {
            return Ok(hits.into_iter().take(keep).map(|(h, _)| h).collect());
        }
        self.fold_ranked(hits, keep)
    }

    fn fold_ranked(&self, ranked: Vec<(Hit, Option<FoldKey>)>, top_k: usize) -> Result<Vec<Hit>> {
        let declared: bool = self.conn.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE type='table' AND name='evidence_links')"
        )?.query_row([], |r| r.get(0))?;
        let mut leaders: HashMap<FoldKey, usize> = HashMap::new();
        let mut result: Vec<Hit> = Vec::with_capacity(top_k);
        for (hit, key) in ranked {
            if let Some(key) = key {
                if let Some(&i) = leaders.get(&key) {
                    let rep = &mut result[i];
                    // 只比本组首项，不沿相似标题传递；分数绝不替代严格的字段判等。
                    if rep.freshness == hit.freshness && rep.drift == hit.drift
                        && similarity(&rep.memory.title, &hit.memory.title) >= SIMILAR_THRESHOLD
                        && self.can_represent(&hit.memory, declared)?
                    {
                        rep.folded_count += 1;
                        if rep.folded_ids.len() < MAX_FOLDED_IDS {
                            rep.folded_ids.push(hit.memory.id);
                        }
                        continue;
                    }
                } else if result.len() < top_k && self.can_represent(&hit.memory, declared)? {
                    leaders.insert(key, result.len());
                }
            }
            // 只保留 top_k 个代表；窗口满后仍处理候选中属于已选代表的低分项。
            if result.len() < top_k {
                result.push(hit);
            }
        }
        Ok(result)
    }

    /// 不隐藏带专属证据的记忆；也不依赖旧行解析失败时用于展示的默认类型/标签。
    /// 全部按当前编号查主键或 src / dst 索引，不遍历关系图，不为旧库建表。
    fn can_represent(&self, m: &Memory, declared: bool) -> Result<bool> {
        let expected_tags = serde_json::to_string(&m.tags)?;
        let eligible: bool = self.conn.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM main.memories WHERE id=?1 AND type=?2 AND tags=?3)
                AND NOT EXISTS(SELECT 1 FROM edges WHERE dst=?4)
                AND NOT EXISTS(SELECT 1 FROM edges WHERE src=?4 AND (kind!='anchors' OR provenance!='agent'))"
        )?.query_row(params![m.id, m.mem_type.as_str(), expected_tags, format!("mem:{}", m.id)], |r| r.get(0))?;
        if !eligible || !declared {
            return Ok(eligible);
        }
        Ok(self.conn.prepare_cached(
            "SELECT NOT EXISTS(SELECT 1 FROM main.evidence_links WHERE src=?1)
                AND NOT EXISTS(SELECT 1 FROM main.evidence_links WHERE dst=?1)"
        )?.query_row([&m.id], |r| r.get(0))?)
    }
}

#[cfg(test)]
mod folding_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{NewMemory, RememberOutcome};

    fn add(s: &Store, ty: MemoryType, title: &str, body: &str) -> String {
        let r = NewMemory { mem_type: Some(ty), title: title.into(), body: body.into(), force: true, ..Default::default() };
        match s.remember(r).unwrap() {
            RememberOutcome::Created { id, .. } => id,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn recall_finds_chinese_and_english() {
        let s = Store::open_in_memory().unwrap();
        add(&s, MemoryType::Gotcha, "PowerShell 空设备写法", "Windows 下不认 /dev/null，必须写 $null，否则 Out-File 报错。");
        add(&s, MemoryType::Decision, "用 SQLite 替代 Neo4j", "Neo4j 常驻约 2GB 内存，记忆规模下递归 CTE 足够。");
        let hits = s.recall(&RecallQuery { query: "空设备".into(), ..Default::default() }).unwrap();
        assert_eq!(hits.first().map(|h| h.memory.title.as_str()), Some("PowerShell 空设备写法"));
        let hits = s.recall(&RecallQuery { query: "neo4j 内存".into(), ..Default::default() }).unwrap();
        assert_eq!(hits.first().map(|h| h.memory.title.as_str()), Some("用 SQLite 替代 Neo4j"));
    }

    #[test]
    fn recall_short_terms_use_like() {
        let s = Store::open_in_memory().unwrap();
        add(&s, MemoryType::Gotcha, "正则边界陷阱", "下划线属于单词字符，加 \\b 会漏匹配，这是个坑。");
        let hits = s.recall(&RecallQuery { query: "坑".into(), ..Default::default() }).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn recall_filters_type_and_archived() {
        let s = Store::open_in_memory().unwrap();
        let a = add(&s, MemoryType::Bugfix, "修复导入重复", "按 legacy_id 去重，避免重复导入同一页。");
        add(&s, MemoryType::Decision, "导入策略", "导入默认 dry-run，确认后才写入，避免误导入。");
        let q = RecallQuery { query: "导入".into(), types: vec![MemoryType::Bugfix], ..Default::default() };
        assert_eq!(s.recall(&q).unwrap().len(), 1);
        s.set_status(&[a], "archived").unwrap();
        assert_eq!(s.recall(&q).unwrap().len(), 0);
    }

    #[test]
    fn freshness_by_type() {
        let now = 100 * 86_400;
        assert_eq!(freshness(MemoryType::Decision, 0, now).0, Freshness::Fresh);
        assert_eq!(freshness(MemoryType::Progress, now - 10 * 86_400, now).0, Freshness::Stale);
        assert_eq!(freshness(MemoryType::Progress, 0, now).0, Freshness::Expired);
    }

    #[test]
    fn similarity_basics() {
        assert!(similarity("修复 cube 自动注册串行导致启动慢", "修复 cube 自动注册串行导致启动慢的问题") > SIMILAR_THRESHOLD);
        assert!(similarity("PowerShell 空设备", "SQLite 替代 Neo4j") < 0.2);
    }
}
