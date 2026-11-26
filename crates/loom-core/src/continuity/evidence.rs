use std::collections::BTreeSet;

use anyhow::{Result, ensure};
use rusqlite::{Connection, params};

use super::text::{MAX_REPORT, Report, field};
use super::{check_id, has_table, with_project};
use crate::memory::Drift;
use crate::search::{Freshness, RecallQuery, freshness};
use crate::{MemoryType, Store, util::now_ts};

// 这张表只接受 evidence_link 的明确输入，不将通用图中的推断边升级为人工证据。
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS evidence_links (
    src TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    dst TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('supports','conflicts')),
    CHECK(src<>dst), PRIMARY KEY(src,kind,dst)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS evidence_links_in ON evidence_links(dst,kind,src);";
const DIRECT: usize = 4;
const PER_DIRECTION: usize = 2;
const QUERY_BYTES: usize = 256;
const FOOTER: &str = "按 ID 取原文；项目红线请另核对已确认意图。";

impl Store {
    /// 明确声明 from 支持或反对 to；不写反向边，不从措辞或生命周期推断关系。
    pub fn evidence_link(&self, from: &str, to: &str, kind: &str) -> Result<String> {
        check_id(from)?;
        check_id(to)?;
        ensure!(from != to, "关系两端不能是同一条记忆");
        ensure!(
            matches!(kind, "supports" | "conflicts"),
            "关系仅接受 supports 或 conflicts"
        );
        with_project(self, |tx| {
            let valid: i64 = tx.query_row(
                "SELECT count(*) FROM memories WHERE id IN (?1,?2) AND status='active'",
                params![from, to],
                |r| r.get(0),
            )?;
            ensure!(valid == 2, "关系两端必须是本项目不同的现存有效记忆");
            tx.execute_batch(SCHEMA)?;
            tx.execute(
                "INSERT INTO evidence_links(src,dst,kind) VALUES (?1,?2,?3)
                ON CONFLICT(src,kind,dst) DO NOTHING",
                params![from, to, kind],
            )?;
            Ok(())
        })?;
        Ok(format!("{from} --{kind}--> {to}"))
    }

    /// 仅在提供具体问题时调用 recall；输出标题/引用而非正文。一次查询读同一个库快照。
    pub fn evidence_pack(&self, query: &str) -> Result<String> {
        let query = field(query, QUERY_BYTES)?;
        ensure!(
            query.split_whitespace().count() <= 16,
            "请用至多 16 个关键词缩小问题"
        );
        with_project(self, |tx| {
            let stored_links = has_table(tx, "evidence_links")?;
            let mut pack = Pack::new(&query);
            let recall = RecallQuery {
                query,
                top_k: DIRECT + 1,
                include_archived: true,
                ..Default::default()
            };
            let ranked = self.recall(&recall)?;
            if ranked.len() > DIRECT {
                pack.report.omit();
            }
            for m in ranked
                .iter()
                .map(|h| &h.memory)
                .filter(|m| m.status != "active")
                .take(DIRECT)
            {
                pack.history(&m.id, &m.status);
            }
            // 历史命中占满窗口时才补一次仅有效状态的 recall，不能让旧史挤掉当前证据。
            let hits = if ranked.len() > DIRECT
                && ranked
                    .iter()
                    .filter(|h| h.memory.status == "active")
                    .count()
                    < DIRECT
            {
                self.recall(&RecallQuery {
                    include_archived: false,
                    ..recall
                })?
            } else {
                ranked
            };
            for hit in hits
                .iter()
                .filter(|h| h.memory.status == "active")
                .take(DIRECT)
            {
                let m = &hit.memory;
                let title = pack.report.brief(&m.title, 68);
                let origin = pack.report.brief(&m.origin, 30);
                let anchor = anchor_label(
                    &mut pack.report,
                    hit.anchors.first().map(String::as_str),
                    hit.anchors.len() > 1,
                );
                pack.direct
                    .push(format!("- {} {title} · {origin} · {anchor}", m.id));
                pack.conditions(&m.id, hit.freshness, hit.drift, hit.anchors.is_empty());
                if stored_links {
                    links_for(self, &mut pack, &m.id)?;
                }
            }
            Ok(pack.finish())
        })
    }
}

struct Pack {
    report: Report,
    direct: Vec<String>,
    supports: Vec<String>,
    conflicts: Vec<String>,
    review: Vec<String>,
    missing: Vec<String>,
    // 边的两个端点都可能直接命中；邻居永远不作为下一跳的起点。
    seen_edges: BTreeSet<(String, String, String)>,
    checked: BTreeSet<String>,
}

impl Pack {
    fn new(query: &str) -> Self {
        let mut report = Report::new(MAX_REPORT, FOOTER);
        let q = report.brief(query, 64);
        report.line(&format!("问题：{q}"));
        Self {
            report,
            direct: vec![],
            supports: vec![],
            conflicts: vec![],
            review: vec![],
            missing: vec![],
            seen_edges: BTreeSet::new(),
            checked: BTreeSet::new(),
        }
    }

    fn history(&mut self, id: &str, status: &str) {
        let label = match status {
            "archived" => "已归档",
            "superseded" => "已取代",
            _ => "非有效状态",
        };
        let line = format!("- {id} {label}，仅作历史");
        if !self.review.contains(&line) {
            self.review.push(line);
        }
    }

    fn conditions(&mut self, id: &str, age: Freshness, drift: Drift, no_anchor: bool) {
        if !self.checked.insert(id.into()) {
            return;
        }
        let mut reasons = Vec::new();
        match age {
            Freshness::Stale => reasons.push("时间较久"),
            Freshness::Expired => reasons.push("时间过期"),
            Freshness::Fresh => (),
        }
        match drift {
            Drift::Drift => reasons.push("源码已变化"),
            Drift::Orphaned => reasons.push("锚点文件不存在"),
            Drift::Unverified => reasons.push("源码未校验"),
            Drift::Clean => (),
        }
        if !reasons.is_empty() {
            self.review.push(format!("- {id} {}", reasons.join("；")));
        }
        if no_anchor {
            self.missing.push(format!("- {id} 未登记锚点"));
        }
    }

    fn finish(mut self) -> String {
        let mut gaps = Vec::new();
        if self.direct.is_empty() {
            gaps.push("无有效直接命中");
        }
        if self.supports.is_empty() {
            gaps.push("缺人工支持");
        }
        if self.conflicts.is_empty() {
            gaps.push("缺人工冲突记录");
        }
        if !gaps.is_empty() {
            self.missing
                .insert(0, format!("- 本次范围：{}", gaps.join("；")));
        }
        // 问题、所有段标题及固定尾注最多 295 字节；余下 1205 字节分段使用。
        self.report.section("直接命中（有效）", &self.direct, 300);
        self.report.section("明确支持", &self.supports, 252);
        self.report.section("明确冲突", &self.conflicts, 252);
        self.report
            .section("历史 / 时间 / 源码复核", &self.review, 211);
        self.report
            .section("缺失（未查到不等于证实）", &self.missing, 190);
        self.report.finish()
    }
}

fn anchor_label(report: &mut Report, anchor: Option<&str>, more: bool) -> String {
    if more {
        report.omit();
    }
    anchor
        .map(|a| report.brief(a, 64))
        .unwrap_or_else(|| "无锚点".into())
}

// 出边、入边分别走 (src,kind,dst) / (dst,kind,src) 索引，每侧只多读一行作省略哨兵。
const OUTGOING: &str =
    "SELECT e.src,e.dst,m.status,m.title,m.type,m.updated_at FROM evidence_links e
    JOIN memories m ON m.id=e.dst WHERE e.src=?1 AND e.kind=?2 ORDER BY e.dst LIMIT ?3";
const INCOMING: &str =
    "SELECT e.src,e.dst,m.status,m.title,m.type,m.updated_at FROM evidence_links e
    JOIN memories m ON m.id=e.src WHERE e.dst=?1 AND e.kind=?2 ORDER BY e.src LIMIT ?3";

fn links_for(store: &Store, pack: &mut Pack, root: &str) -> Result<()> {
    for kind in ["supports", "conflicts"] {
        for sql in [OUTGOING, INCOMING] {
            let mut st = store.conn.prepare_cached(sql)?;
            let mut rows = st.query(params![root, kind, (PER_DIRECTION + 1) as i64])?;
            let mut n = 0;
            while let Some(row) = rows.next()? {
                if n == PER_DIRECTION {
                    pack.report.omit();
                    break;
                }
                n += 1;
                let from: String = row.get(0)?;
                let to: String = row.get(1)?;
                if !pack
                    .seen_edges
                    .insert((from.clone(), to.clone(), kind.into()))
                {
                    continue;
                }
                let neighbor = if from == root { &to } else { &from };
                let status: String = row.get(2)?;
                if status != "active" {
                    pack.history(neighbor, &status);
                    continue;
                }
                let title = pack.report.brief(&row.get::<_, String>(3)?, 44);
                let (anchor, more) = first_anchor(&store.conn, neighbor)?;
                let label = anchor_label(&mut pack.report, anchor.as_deref(), more);
                let line = format!("- {from} --{kind}--> {to} · {title} · {label}");
                if kind == "supports" {
                    pack.supports.push(line);
                } else {
                    pack.conflicts.push(line);
                }
                if !pack.checked.contains(neighbor) {
                    let ty: MemoryType = row.get::<_, String>(4)?.parse()?;
                    let age = freshness(ty, row.get(5)?, now_ts()).0;
                    let drift = if anchor.is_some() {
                        store.drift(neighbor)?
                    } else {
                        Drift::Clean
                    };
                    pack.conditions(neighbor, age, drift, anchor.is_none());
                }
            }
        }
    }
    Ok(())
}

fn first_anchor(conn: &Connection, id: &str) -> Result<(Option<String>, bool)> {
    let mut st = conn.prepare_cached(
        "SELECT path,symbol FROM anchors WHERE memory_id=?1 ORDER BY path,symbol LIMIT 2",
    )?;
    let mut rows = st.query([id])?;
    let Some(first) = rows.next()? else {
        return Ok((None, false));
    };
    let path: String = first.get(0)?;
    let sym: String = first.get(1)?;
    let anchor = if sym.is_empty() {
        path
    } else {
        format!("{path}#{sym}")
    };
    Ok((Some(anchor), rows.next()?.is_some()))
}
