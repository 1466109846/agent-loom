use anyhow::{Result, ensure};
use rusqlite::params;

use super::Gaps;
use crate::{
    Memory, Store,
    admission::MAX_TITLE_CHARS,
    intent::{one_line, valid_id},
    memory::Drift,
    search::{Freshness, RecallQuery, freshness, marks},
    util::{fmt_date, now_ts},
};

const LIMIT: usize = 5;

pub(super) struct Experience {
    pub id: String,
    pub line: String,
    pub body: String,
    warning: String,
    long_title: bool,
}

impl Experience {
    /// `via` 说明这条是怎么找到的：有人把它锚在这个文件上，还是只是文字对上了。
    fn new(m: Memory, fresh: Freshness, drift: Drift, via: &str) -> Result<Self> {
        ensure!(valid_id(&m.id), "经验编号无效，不能按编号取回");
        let title = one_line(&m.title);
        let long_title = title.chars().count() > MAX_TITLE_CHARS;
        let title = if long_title {
            "（长标题已省略）"
        } else {
            &title
        };
        let mark = marks(fresh, drift, &m.status);
        let line = format!(
            "经验 [{}] {title} · {} · id={} · {via}{mark}",
            m.mem_type,
            fmt_date(m.updated_at),
            m.id
        );
        let mut warnings = Vec::new();
        match drift {
            Drift::Clean => (),
            Drift::Drift => warnings.push("源码已变"),
            Drift::Orphaned => warnings.push("锚点已消失"),
            Drift::Unverified => warnings.push("锚点未校验"),
        }
        if fresh != Freshness::Fresh {
            warnings.push("时间久了");
        }
        Ok(Self {
            id: m.id,
            line,
            body: one_line(&m.body),
            long_title,
            warning: warnings.join("、"),
        })
    }
}

fn anchored(s: &Store, target: &str) -> Result<(Vec<Experience>, usize)> {
    // 窗口只过定位键与时间，取定前五之后才取正文。
    let sql = format!("WITH picked AS (SELECT id, count(*) OVER () AS total FROM memories WHERE status='active' AND id IN
        (SELECT memory_id FROM anchors WHERE path=?1) ORDER BY updated_at DESC,id LIMIT ?2)
        SELECT {},total FROM picked JOIN memories USING(id) ORDER BY updated_at DESC,id", Memory::COLUMNS);
    let mut stmt = s.conn.prepare_cached(&sql)?;
    let mut rows = stmt.query(params![target, LIMIT as i64])?;
    let mut out = Vec::new();
    let mut total = 0;
    let now = now_ts();
    while let Some(row) = rows.next()? {
        total = row.get::<_, i64>(12)? as usize;
        let m = Memory::from_row(row)?;
        let drift = s.drift(&m.id)?;
        let fresh = freshness(m.mem_type, m.updated_at, now).0;
        out.push(Experience::new(m, fresh, drift, ANCHORED)?);
    }
    let omitted = total - out.len();
    Ok((out, omitted))
}

const ANCHORED: &str = "锚定本文件";

/// `asked` 表示关键词来自调用方的 question；否则是按文件名补查。
pub(super) fn read(
    s: &Store,
    target: &str,
    keywords: Option<&str>,
    asked: bool,
    gaps: &mut Gaps,
) -> Vec<Experience> {
    let anchors = gaps.source("anchors_failed", "锚定经验", anchored(s, target));
    let anchors_ok = anchors.is_some();
    let (mut out, limited) = anchors.unwrap_or_default();
    gaps.limited("anchors", "锚定经验（前 5 条之外）", limited);
    if let Some(words) = keywords {
        let result = if words.trim().is_empty() {
            Ok((vec![], false))
        } else {
            s.recall_in_snapshot(
                &RecallQuery {
                    query: words.into(),
                    top_k: LIMIT,
                    ..Default::default()
                },
                true,
            )
        };
        if let Some((hits, window)) = gaps.source("keywords_failed", "关键词经验", result) {
            if window {
                gaps.add(
                    "keyword_window",
                    "关键词候选超过 50 条检索窗口，窗口外条数未知。".into(),
                );
            }
            // 按文件名补查落空是常态，不算缺口；调用方给的关键词落空才提示写法。
            if hits.is_empty() && asked {
                gaps.add(
                    "keyword_miss",
                    "question 的关键词没有命中经验；它按空格分词，不带空格的整句算一个词。".into(),
                );
            }
            let mut extra = Vec::new();
            let mut eligible = 0;
            let mut folded = 0;
            for h in hits {
                let anchored = h.anchors.iter().any(|path| path == target);
                if anchored && anchors_ok {
                    continue;
                }
                folded += h.folded_count;
                eligible += 1;
                let via = if anchored {
                    ANCHORED
                } else if asked {
                    "关键词命中"
                } else {
                    "提到本文件名"
                };
                if eligible <= LIMIT
                    && let Some(m) = gaps.source(
                        "keywords_failed",
                        "关键词经验",
                        Experience::new(h.memory, h.freshness, h.drift, via),
                    )
                {
                    extra.push(m);
                }
            }
            gaps.limited(
                "keywords",
                "补充经验（前 5 条之外）",
                eligible.saturating_sub(LIMIT),
            );
            gaps.limited("folded", "关键词折叠的重复经验", folded);
            out.extend(extra);
        }
    } else {
        gaps.add(
            "short_name",
            "文件名不足 3 个字符，没有按文件名补查经验。".into(),
        );
    }
    let long = out.iter().filter(|m| m.long_title).count();
    gaps.limited("long_titles", "经验长标题", long);
    for m in &out {
        if !m.warning.is_empty() {
            gaps.add(
                "memory_review",
                format!("经验需复核：{} · {}", m.id, m.warning),
            );
        }
    }
    out
}
