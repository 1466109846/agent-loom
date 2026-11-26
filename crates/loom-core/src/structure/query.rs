//! 只读查询同一个已完成的快照；箭头统一为使用方 → 被使用方。
use std::collections::{BTreeSet, VecDeque};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};

use super::{
    Coverage, Direction, Issue, QueryReport, REVISION, Relation, Snapshot, Symbol, resolve, verify,
};
use crate::Store;

const MAX_RELATIONS: usize = 64;
const MAX_ISSUES: usize = 24;
const MAX_SYMBOLS: usize = 40;

pub(crate) fn target_key(path: &str) -> Result<String> {
    let path = path.replace('\\', "/");
    ensure!(
        path.len() <= 4096
            && !path.starts_with('/')
            && !path.contains(':')
            && !path.chars().any(char::is_control),
        "目标必须是项目内的相对文件路径"
    );
    let mut parts = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => (),
            ".." => anyhow::bail!("目标不能包含上级目录"),
            _ => parts.push(p),
        }
    }
    ensure!(!parts.is_empty(), "需要相对文件路径");
    Ok(parts.join("/"))
}

impl Store {
    pub fn query_structure(
        &self,
        target: &str,
        direction: Direction,
        depth: usize,
    ) -> Result<QueryReport> {
        let project = self.project().context("查询必须绑定到一个项目")?;
        project.verify_live_root()?;
        let tx = self.conn.unchecked_transaction()?;
        ensure!(
            crate::note::binding_matches(&tx, project)?,
            "项目绑定已改变；未加载结构快照"
        );
        let (mut out, basis) = read_snapshot(&tx, target, direction, depth)?;
        tx.commit()?;
        basis.verify(project, self.conn.path(), &mut out);
        out.conclude();
        Ok(out)
    }
}

pub(crate) fn read_snapshot(
    conn: &Connection,
    target: &str,
    direction: Direction,
    depth: usize,
) -> Result<(QueryReport, verify::Basis)> {
    ensure!((1..=3).contains(&depth), "查询深度须为 1 至 3");
    let target = target_key(target)?;
    let mut out = QueryReport {
        contract: 2,
        target,
        direction,
        depth,
        status: "not_indexed".into(),
        snapshot_at: None,
        coverage: Coverage::default(),
        symbols: vec![],
        relations: vec![],
        issues: vec![],
        truncated: false,
        snapshot: None,
        examined: Default::default(),
        verification: Default::default(),
        conclusion: Default::default(),
        limits: Default::default(),
    };
    let mut references = Vec::new();
    read_evidence(conn, &mut out, &mut references)?;
    let basis = verify::Basis::load(conn, &out, &references)?;
    if crate::redact::sensitive(&out.target) {
        out.target = crate::redact::redact(&out.target).0;
    }
    out.conclude();
    Ok((out, basis))
}

fn has_table(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        )?
        .query_row([name], |r| r.get(0))?)
}

pub(crate) fn parsed_in_snapshot(conn: &Connection, key: &str) -> Result<bool> {
    if !has_table(conn, "structure_snapshot")? {
        return Ok(false);
    }
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM structure_snapshot m JOIN files f JOIN structure_files s ON s.file_id=f.id WHERE m.singleton=1 AND m.revision=?1 AND f.path=?2 AND s.state='indexed')",params![REVISION,key],|r|r.get(0))?)
}

fn read_evidence(
    conn: &Connection,
    out: &mut QueryReport,
    references: &mut Vec<i64>,
) -> Result<()> {
    if !has_table(conn, "structure_snapshot")? {
        return Ok(());
    }
    let meta=conn.query_row("SELECT revision,finished_at,total,uncovered,unresolved FROM structure_snapshot WHERE singleton=1",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,Coverage{managed_files:r.get(2)?,uncovered_files:r.get(3)?,unresolved_refs:r.get(4)?, ..Default::default()}))).optional()?;
    let Some((version, stamp, coverage)) = meta else {
        return Ok(());
    };
    ensure!(
        version == REVISION,
        "结构快照版本不匹配，请用匹配版本更新或读取索引"
    );
    out.coverage = coverage;
    out.snapshot_at = Some(stamp);
    let mut snapshot = Snapshot {
        revision: version,
        finished_at: stamp,
        generation: None,
        scope: None,
        inputs: None,
    };
    if has_table(conn, "structure_generation")? {
        snapshot.generation = conn
            .query_row(
                "SELECT generation FROM structure_generation WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .optional()?;
    }
    if let Some(generation) = snapshot.generation
        && has_table(conn, "structure_publication")?
        && has_table(conn, "structure_coverage")?
        && has_table(conn, "structure_input_files")?
    {
        let mut q=conn.prepare_cached("SELECT scope,inputs,excluded_dirs,excluded_files,withheld,refs FROM structure_publication WHERE singleton=1 AND generation=?1")?;
        let mut rows = q.query([generation])?;
        if let Some(row) = rows.next()? {
            snapshot.scope = Some(row.get(0)?);
            snapshot.inputs = Some(row.get(1)?);
            let c = &mut out.coverage;
            c.known = true;
            c.excluded_dirs = Some(row.get(2)?);
            c.excluded_files = Some(row.get(3)?);
            c.withheld = Some(row.get(4)?);
            c.refs = Some(row.get(5)?);
            let mut uncovered = std::collections::BTreeMap::new();
            let mut unresolved = std::collections::BTreeMap::new();
            let mut q = conn.prepare_cached("SELECT state,count FROM structure_coverage")?;
            let mut rows = q.query([])?;
            while let Some(row) = rows.next()? {
                let state: String = row.get(0)?;
                if let Some(code) = state.strip_prefix("ref:") {
                    unresolved.insert(code.to_owned(), row.get(1)?);
                } else if state != "indexed" {
                    uncovered.insert(state, row.get(1)?);
                }
            }
            c.uncovered = Some(uncovered);
            c.unresolved = Some(unresolved);
        }
    }
    out.snapshot = Some(snapshot);
    let file=conn.query_row("SELECT f.id,s.state FROM files f JOIN structure_files s ON s.file_id=f.id WHERE f.path=?1",[&out.target],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?))).optional()?;
    let Some((id, state)) = file else {
        out.status = "absent".into();
        return Ok(());
    };
    out.status = state;
    if out.direction == Direction::Outgoing {
        // 没有公开的顶层声明只供同包文件按名字查找，不作为符号展示。
        let mut st=conn.prepare_cached("SELECT name,kind,line_start FROM symbols WHERE file_id=?1 AND exported=1 ORDER BY line_start,name LIMIT ?2")?;
        for s in st.query_map(params![id, (MAX_SYMBOLS + 1) as i64], |r| {
            Ok(Symbol {
                name: r.get(0)?,
                kind: r.get(1)?,
                line: r.get(2)?,
            })
        })? {
            if out.symbols.len() == MAX_SYMBOLS {
                out.truncated = true;
                out.limits.insert("符号 40");
                break;
            }
            out.symbols.push(s?);
        }
    }
    // 按名字找到的引用（经包名用到的成员、同包的名字）一个文件里可以有很多处指向同一个目标；
    // 同一对文件只留行号最小的一处，否则少数几个文件就会占满结果。
    // 只是提到名字、项目里又没有对应声明的，与项目无关，不列为缺口。
    let (relations, issues) = match out.direction {
        Direction::Outgoing => (
            "SELECT min(r.id),b.id,a.path,b.path,min(r.line),r.kind,r.resolution,count(*) FROM structure_refs r JOIN files a ON a.id=r.source_id JOIN files b ON b.id=r.target_id WHERE r.source_id=?1 GROUP BY r.target_id,r.kind,r.resolution,CASE WHEN r.kind IN ('use','mention') THEN 0 ELSE r.id END ORDER BY a.path,b.path,min(r.line),min(r.id) LIMIT ?2",
            "SELECT r.id,a.path,r.spelling,r.line,r.resolution FROM structure_refs r JOIN files a ON a.id=r.source_id WHERE r.source_id=?1 AND r.target_id IS NULL AND r.resolution!='none' ORDER BY r.line,r.id LIMIT ?2",
        ),
        Direction::Incoming => (
            "SELECT min(r.id),a.id,a.path,b.path,min(r.line),r.kind,r.resolution,count(*) FROM structure_refs r JOIN files a ON a.id=r.source_id JOIN files b ON b.id=r.target_id WHERE r.target_id=?1 GROUP BY r.source_id,r.kind,r.resolution,CASE WHEN r.kind IN ('use','mention') THEN 0 ELSE r.id END ORDER BY a.path,b.path,min(r.line),min(r.id) LIMIT ?2",
            // 把这个文件列为候选、却没能定下目标的引用：候选可以写它的路径、它提供的声明键，
            // 或“声明键里的某个顶层名字”（拼法与 loom_parse::member 一致）；
            // 还有经搜索键找到它某个公开名字的“提及”。
            "SELECT r.id,a.path,r.spelling,r.line,r.resolution FROM structure_refs r JOIN files a ON a.id=r.source_id WHERE r.target_id IS NULL AND r.resolution!='none' AND r.id IN (SELECT c.ref_id FROM files b JOIN structure_candidates c ON c.path=b.path WHERE b.id=?1 UNION SELECT c.ref_id FROM structure_provides p JOIN structure_candidates c ON c.path=p.key WHERE p.file_id=?1 UNION SELECT c.ref_id FROM structure_provides p JOIN structure_members y ON y.file_id=p.file_id JOIN structure_candidates c ON c.path='sym:'||y.name||':'||p.key WHERE p.file_id=?1 UNION SELECT m.id FROM structure_provides p JOIN structure_members y ON y.file_id=p.file_id AND y.exported=1 JOIN structure_search s ON s.key=p.key JOIN structure_refs m ON m.source_id=s.file_id AND m.kind='mention' AND m.spelling=y.name WHERE p.file_id=?1) ORDER BY a.path,r.line,r.id LIMIT ?2",
        ),
    };
    let mut steps = conn.prepare_cached(relations)?;
    let mut gaps = conn.prepare_cached(issues)?;
    let mut queue = VecDeque::from([(id, 1usize)]);
    let mut visited = BTreeSet::from([id]);
    let mut evidence = BTreeSet::new();
    while let Some((from, hop)) = queue.pop_front() {
        out.examined.files += 1;
        let room = MAX_ISSUES - out.issues.len();
        let mut pending = gaps.query(params![from, (room + 1) as i64])?;
        while let Some(row) = pending.next()? {
            out.examined.refs += 1;
            if out.issues.len() == MAX_ISSUES {
                out.truncated = true;
                out.limits.insert("未确定引用 24");
                break;
            }
            let state: String = row.get(4)?;
            let mut candidates: Vec<String> = Vec::new();
            if matches!(state.as_str(), "ambiguous" | "uncovered_target") {
                candidates = resolve::resolve(conn, row.get(0)?)?
                    .hits
                    .into_iter()
                    .map(|hit| hit.path)
                    .collect();
                if candidates.len() > 4 {
                    out.truncated = true;
                    out.limits.insert("每处候选 4");
                    candidates.truncate(4);
                }
            }
            out.issues.push(Issue {
                source: row.get(1)?,
                spelling: row.get(2)?,
                line: row.get(3)?,
                state,
                candidates,
            });
        }
        let mut rows = steps.query(params![from, (MAX_RELATIONS + 1) as i64])?;
        while let Some(row) = rows.next()? {
            out.examined.refs += row.get::<_, i64>(7)?;
            let reference: i64 = row.get(0)?;
            if !evidence.insert(reference) {
                continue;
            }
            if out.relations.len() == MAX_RELATIONS {
                out.truncated = true;
                out.limits.insert("关系 64");
                return Ok(());
            }
            let next: i64 = row.get(1)?;
            references.push(reference);
            out.relations.push(Relation {
                source: row.get(2)?,
                target: row.get(3)?,
                line: row.get(4)?,
                kind: row.get(5)?,
                confidence: row.get(6)?,
                hop,
            });
            if hop < out.depth && visited.insert(next) {
                queue.push_back((next, hop + 1));
            }
        }
    }
    Ok(())
}
