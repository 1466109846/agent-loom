//! 一条引用的候选怎样对上文件。索引时据此下结论，查询时据此列出没能消歧的候选。
//!
//! 候选分三组，存在候选表的 `substitute` 列里：0 是书写的目标；1 是与它并列、只按工具约定
//! 成立的替代；2 是前两组都落空才看的。候选可以写成项目内的路径、某个文件提供的声明键，
//! 或“声明键里的某个顶层名字”（见 `loom_parse::Reference`）。
use anyhow::Result;
use rusqlite::{Connection, params};

pub(super) struct Hit {
    pub file: i64,
    pub path: String,
    pub state: String,
    pub tier: i64,
    /// 按路径命中，或与使用方同属一个根；按名字跨根命中的不能算确定。
    pub proven: bool,
}

pub(super) struct Resolved {
    pub kind: String,
    /// 由解析器判定；动态加载和按 Python 包根推断的目标不会因唯一命中就升级为确定。
    pub exact: bool,
    /// 命中的文件，按路径排序。
    pub hits: Vec<Hit>,
}

// 每个候选只取这么多行：足以判断是否唯一，也够列出几个候选。
const PER_CANDIDATE: i64 = 6;

/// 同一文件只留最强的一次命中。
fn keep(found: &mut Vec<Hit>, hit: Hit) {
    match found.iter_mut().find(|h| h.file == hit.file) {
        Some(h) if hit.tier < h.tier => *h = hit,
        Some(h) if hit.tier == h.tier => h.proven |= hit.proven,
        Some(_) => (),
        None => found.push(hit),
    }
}

/// 按名字找不会找回使用方自己。从名字出发再核对声明键：同名的声明远少于一个包里的文件。
const BY_MEMBER: &str = "SELECT f.id,f.path,s.state,d.domain FROM structure_members y CROSS JOIN structure_provides p ON p.key=?1 AND p.file_id=y.file_id JOIN files f ON f.id=y.file_id JOIN structure_files s ON s.file_id=f.id LEFT JOIN structure_domains d ON d.file_id=f.id WHERE y.name=?4 AND y.file_id!=?2 AND (?5=0 OR y.exported=1) ORDER BY f.path LIMIT ?3";
const BY_KEY: &str = "SELECT f.id,f.path,s.state,d.domain FROM structure_provides p JOIN files f ON f.id=p.file_id JOIN structure_files s ON s.file_id=f.id LEFT JOIN structure_domains d ON d.file_id=f.id WHERE p.key=?1 AND f.id!=?2 ORDER BY f.path LIMIT ?3";

type Row = (i64, String, String, Option<String>);
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
}

/// 当前快照里这条引用命中的文件。
pub(super) fn resolve(conn: &Connection, reference: i64) -> Result<Resolved> {
    let (from, kind, spelling, exact, home): (i64, String, String, bool, Option<String>) = conn
        .prepare_cached("SELECT r.source_id,r.kind,r.spelling,r.certain,d.domain FROM structure_refs r LEFT JOIN structure_domains d ON d.file_id=r.source_id WHERE r.id=?1")?
        .query_row([reference], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
    let mut found = Vec::new();
    // 写成路径的候选：一次连接取齐。
    let mut by_path = conn.prepare_cached("SELECT f.id,f.path,s.state,c.substitute FROM structure_candidates c JOIN files f ON f.path=c.path JOIN structure_files s ON s.file_id=f.id WHERE c.ref_id=?1")?;
    let mut rows = by_path.query([reference])?;
    while let Some(r) = rows.next()? {
        keep(
            &mut found,
            Hit {
                file: r.get(0)?,
                path: r.get(1)?,
                state: r.get(2)?,
                tier: r.get(3)?,
                proven: true,
            },
        );
    }
    // 写成声明键的候选。键里必有冒号，路径里没有。
    let mut named = conn.prepare_cached(
        "SELECT path,substitute FROM structure_candidates WHERE ref_id=?1 AND instr(path,':')>0",
    )?;
    let listed: Vec<(String, i64)> = named
        .query_map([reference], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut by_member = conn.prepare_cached(BY_MEMBER)?;
    let mut by_key = conn.prepare_cached(BY_KEY)?;
    for (candidate, tier) in listed {
        let rows: Vec<Row> = match loom_parse::member_parts(&candidate) {
            Some((name, key)) => by_member
                .query_map(params![key, from, PER_CANDIDATE, name, false], row)?
                .collect::<rusqlite::Result<_>>()?,
            None => by_key
                .query_map(params![candidate, from, PER_CANDIDATE], row)?
                .collect::<rusqlite::Result<_>>()?,
        };
        for (file, path, state, domain) in rows {
            let proven = domain.is_some() && domain == home;
            keep(
                &mut found,
                Hit {
                    file,
                    path,
                    state,
                    tier,
                    proven,
                },
            );
        }
    }
    // 前两组有命中时第三组不参与：语言规定前者优先。
    if found.iter().any(|h| h.tier < 2) {
        found.retain(|h| h.tier < 2);
    }
    // 只是提到的名字在候选里没有着落时，再到使用方的搜索键（Java 的按需导入、Go 的点导入）
    // 里找公开的同名声明。这样找到的永远只算可能。
    if found.is_empty() && kind == "mention" {
        let mut search =
            conn.prepare_cached("SELECT key FROM structure_search WHERE file_id=?1 ORDER BY key")?;
        let keys: Vec<String> = search
            .query_map([from], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for key in keys {
            let rows: Vec<Row> = by_member
                .query_map(params![key, from, PER_CANDIDATE, spelling, true], row)?
                .collect::<rusqlite::Result<_>>()?;
            for (file, path, state, _) in rows {
                keep(
                    &mut found,
                    Hit {
                        file,
                        path,
                        state,
                        tier: 2,
                        proven: false,
                    },
                );
            }
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Resolved {
        kind,
        exact,
        hits: found,
    })
}
