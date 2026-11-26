//! 文件归属来自模块声明，不是最长目录前缀。每次只读本事务内的提取事实。
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use anyhow::Result;
use loom_parse::Parsed;
use rusqlite::{Connection, OptionalExtension, params};

/// 仅承认能读懂的 Cargo 默认布局。未知表、目标覆盖、多行 TOML 等整包降级；
/// 这不是依赖解析器，绝不用依赖表中的名字给具体 use 分类。
fn default_package(text: &str) -> bool {
    let mut section = "";
    let mut named = false;
    for line in text
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
    {
        if line.starts_with('[') {
            section = match line {
                "[package]" | "[dependencies]" | "[dev-dependencies]" | "[build-dependencies]" => {
                    line
                }
                _ => return false,
            };
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        let key = key.trim();
        let value = value.trim();
        if section.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            || value.is_empty()
            || value.contains(['\'', '\\', '#'])
            || value.contains("\"\"\"")
            || value.bytes().filter(|&b| b == b'\"').count() % 2 != 0
        {
            return false;
        }
        if section == "[package]" {
            match key {
                "name" => {
                    let Some(name) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"'))
                    else {
                        return false;
                    };
                    if named
                        || name.is_empty()
                        || !name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                    {
                        return false;
                    }
                    named = true;
                }
                "version" | "edition" | "license" | "rust-version"
                    if value.starts_with('"') && value.ends_with('"') => {}
                "version.workspace"
                | "edition.workspace"
                | "license.workspace"
                | "rust-version.workspace"
                    if value == "true" => {}
                _ => return false,
            }
        }
    }
    named
}

pub(super) fn package_roots(
    inventory: &mut BTreeMap<String, PathBuf>,
    inputs: &mut super::scan::InputFiles,
) -> BTreeSet<String> {
    let mut roots = BTreeSet::new();
    inventory.retain(|key, real| {
        let prefix = if key == "Cargo.toml" {
            ""
        } else if let Some(dir) = key.strip_suffix("/Cargo.toml") {
            dir
        } else {
            return true;
        };
        let input = super::scan::read(real);
        inputs.insert(("rust", key.clone()), input.hash);
        if input.text.as_deref().is_some_and(default_package) {
            for target in ["src/lib.rs", "src/main.rs"] {
                roots.insert(if prefix.is_empty() {
                    target.to_owned()
                } else {
                    format!("{prefix}/{target}")
                });
            }
        }
        false
    });
    roots
}

/// 清单和被排除的候选也能改变唯一性，不记正文、不落盘私密路径，只比较摘要。
pub(super) fn inputs_changed(
    conn: &Connection,
    roots: &BTreeSet<String>,
    blocked: &BTreeSet<String>,
) -> Result<bool> {
    let digest = crate::util::sha256_hex(&serde_json::to_vec(&(roots, blocked))?);
    let old: Option<String> = conn
        .query_row(
            "SELECT digest FROM structure_rust_inputs WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if old.as_ref() == Some(&digest) {
        return Ok(false);
    }
    conn.execute("INSERT INTO structure_rust_inputs VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET digest=excluded.digest", [digest])?;
    Ok(true)
}

pub(super) fn replace_file(conn: &Connection, id: i64, facts: &Parsed) -> Result<()> {
    conn.prepare_cached("DELETE FROM structure_rust_scopes WHERE file_id=?1")?
        .execute([id])?;
    conn.prepare_cached("DELETE FROM structure_rust_modules WHERE file_id=?1")?
        .execute([id])?;
    conn.prepare_cached("DELETE FROM structure_rust_items WHERE file_id=?1")?
        .execute([id])?;
    if let Some(root) = facts.rust_scope {
        conn.prepare_cached("INSERT INTO structure_rust_scopes(file_id,is_root) VALUES(?1,?2)")?
            .execute(params![id, root])?;
    }
    let mut module = conn.prepare_cached("INSERT INTO structure_rust_modules(file_id,name,path,usable,crate_visible) VALUES(?1,?2,?3,?4,?5)")?;
    for m in &facts.rust_modules {
        for path in &m.candidates {
            module.execute(params![id, m.name, path, m.usable, m.crate_visible])?;
        }
    }
    let mut item = conn
        .prepare_cached("INSERT INTO structure_rust_items(file_id,name,public) VALUES(?1,?2,?3)")?;
    for i in &facts.rust_items {
        item.execute(params![id, i.name, i.public])?;
    }
    Ok(())
}

#[derive(Default)]
struct Scope {
    root: bool,
    modules: HashMap<String, (i64, bool)>,
    items: HashMap<String, bool>,
}

struct Graph {
    scopes: HashMap<i64, Scope>,
    // 无条目 = 没有入声明；None = 多条声明或单条声明本身不可靠。
    parents: HashMap<i64, Option<i64>>,
}

impl Graph {
    fn load(
        conn: &Connection,
        roots: &BTreeSet<String>,
        blocked: &BTreeSet<String>,
    ) -> Result<Self> {
        let mut scopes = HashMap::new();
        let mut st = conn.prepare("SELECT f.id,f.path,s.is_root FROM files f JOIN structure_rust_scopes s ON s.file_id=f.id")?;
        let mut rows = st.query([])?;
        while let Some(row) = rows.next()? {
            let root = row.get::<_, bool>(2)?;
            // 文件名只能提出入口候选；没有已确认的常规包目标，不授予 crate 根身份。
            if !root || roots.contains(&row.get::<_, String>(1)?) {
                scopes.insert(
                    row.get(0)?,
                    Scope {
                        root,
                        ..Scope::default()
                    },
                );
            }
        }
        let mut parents = HashMap::new();
        // 同时匹配 foo.rs 和 foo/mod.rs 时，即使一个未覆盖/被排除也没有唯一布局。
        // 条件、内联作用域或 #[path] 的声明参与排他性检查，但不授予归属。
        struct Candidate {
            file: Option<i64>,
            usable: bool,
            crate_visible: bool,
        }
        let mut candidates: HashMap<(i64, String), Vec<Candidate>> = HashMap::new();
        let mut st = conn.prepare("SELECT m.file_id,m.name,m.path,s.file_id,m.usable,m.crate_visible
            FROM structure_rust_modules m LEFT JOIN files f ON f.path=m.path LEFT JOIN structure_files s ON s.file_id=f.id")?;
        let mut rows = st.query([])?;
        while let Some(row) = rows.next()? {
            let child: Option<i64> = row.get(3)?;
            let path: String = row.get(2)?;
            if child.is_some()
                || std::iter::successors(Some(path.as_str()), |p| {
                    p.rsplit_once('/').map(|(dir, _)| dir)
                })
                .any(|p| blocked.contains(p))
            {
                candidates
                    .entry((row.get(0)?, row.get(1)?))
                    .or_default()
                    .push(Candidate {
                        file: child,
                        usable: row.get(4)?,
                        crate_visible: row.get(5)?,
                    });
            }
        }
        for ((parent, name), found) in candidates {
            let unique = found.len() == 1 && found[0].usable;
            for candidate in found {
                let Some(child) = candidate.file else {
                    continue;
                };
                parents
                    .entry(child)
                    .and_modify(|p| *p = None)
                    .or_insert(unique.then_some(parent));
                if unique && let Some(scope) = scopes.get_mut(&parent) {
                    scope
                        .modules
                        .insert(name.clone(), (child, candidate.crate_visible));
                }
            }
        }
        let mut st = conn.prepare("SELECT file_id,name,public FROM structure_rust_items")?;
        let mut rows = st.query([])?;
        while let Some(row) = rows.next()? {
            if let Some(scope) = scopes.get_mut(&row.get::<_, i64>(0)?) {
                scope.items.insert(row.get(1)?, row.get(2)?);
            }
        }
        Ok(Self { scopes, parents })
    }

    fn ancestry(&self, source: i64) -> Option<Vec<i64>> {
        let mut path = Vec::new();
        let mut file = source;
        // 不递归；重复/过深、断链、根自身还被别处包含都不能证明唯一归属。
        while path.len() <= 128 && !path.contains(&file) {
            path.push(file);
            match (self.scopes.get(&file)?.root, self.parents.get(&file)) {
                (true, None) => return Some(path),
                (false, Some(Some(parent))) => file = *parent,
                _ => break,
            }
        }
        None
    }

    fn provider(&self, ancestry: &[i64], path: &str, reexport: bool) -> Option<i64> {
        let (prefix, item) = path.rsplit_once("::")?;
        let (anchor, modules) = prefix.split_once("::")?;
        let mut file = match anchor {
            "crate" => *ancestry.last()?,
            "self" => *ancestry.first()?,
            _ => return None,
        };
        for name in modules.split("::") {
            let (child, crate_visible) = *self.scopes.get(&file)?.modules.get(name)?;
            if !crate_visible && !ancestry.contains(&file) {
                return None;
            }
            if self.parents.get(&child) != Some(&Some(file)) || self.scopes.get(&child)?.root {
                return None;
            }
            file = child;
        }
        if item == "self" {
            // 不从有限的可见性表示推断模块本身可对 crate 外再导出。
            return (!reexport).then_some(file);
        }
        let public = *self.scopes.get(&file)?.items.get(item)?;
        (public || !reexport).then_some(file)
    }
}

pub(super) fn chain(
    db: &Connection,
    id: i64,
    source: &str,
    add: &mut impl FnMut(&str),
) -> Result<()> {
    // 已连上的同一条 use 的各个叶子必须指向同一个文件；其唯一声明链也相同。
    let path: Option<String> = db
        .prepare_cached(
            "SELECT path FROM structure_rust_paths WHERE ref_id=?1 ORDER BY path LIMIT 1",
        )?
        .query_row([id], |r| r.get(0))
        .optional()?;
    let Some(path) = path else {
        return Ok(());
    };
    let Some((prefix, _)) = path.rsplit_once("::") else {
        return Ok(());
    };
    let mut parts = prefix.split("::");
    let anchor = parts.next();
    let mut parent = db.prepare_cached("SELECT min(f.path) FROM structure_rust_modules m JOIN files f ON f.id=m.file_id WHERE m.path=?1 AND m.usable=1 HAVING count(*)=1")?;
    let mut file = source.to_owned();
    for _ in 0..8 {
        let Some(p) = parent
            .query_row([&file], |r| r.get::<_, String>(0))
            .optional()?
        else {
            break;
        };
        add(&p);
        file = p;
    }
    if anchor==Some("self") {file=source.to_owned();} else if anchor!=Some("crate") || !db.prepare_cached("SELECT EXISTS(SELECT 1 FROM files f JOIN structure_rust_scopes s ON s.file_id=f.id WHERE f.path=?1 AND s.is_root=1)")?.query_row([&file], |r|r.get::<_,bool>(0))? {return Ok(());}
    let mut child=db.prepare_cached("SELECT min(m.path) FROM files f JOIN structure_rust_modules m ON m.file_id=f.id JOIN files c ON c.path=m.path JOIN structure_files s ON s.file_id=c.id WHERE f.path=?1 AND m.name=?2 AND m.usable=1 HAVING count(*)=1")?;
    for name in parts.take(8) {
        let Some(p) = child
            .query_row(params![file, name], |r| r.get::<_, String>(0))
            .optional()?
        else {
            break;
        };
        add(&p);
        file = p;
    }
    Ok(())
}

pub(super) fn relink(
    conn: &Connection,
    roots: &BTreeSet<String>,
    blocked: &BTreeSet<String>,
) -> Result<usize> {
    let any: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM structure_rust_paths)",
        [],
        |r| r.get(0),
    )?;
    if !any {
        return Ok(0);
    }
    let graph = Graph::load(conn, roots, blocked)?;
    let mut uses = conn.prepare(
        "SELECT id,source_id,kind FROM structure_refs WHERE EXISTS
        (SELECT 1 FROM structure_rust_paths WHERE ref_id=structure_refs.id)",
    )?;
    let mut leaves = conn.prepare("SELECT path FROM structure_rust_paths WHERE ref_id=?1")?;
    let mut update =
        conn.prepare_cached("UPDATE structure_refs SET target_id=?2,resolution=?3 WHERE id=?1")?;
    let mut count = 0;
    let mut rows = uses.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let source: i64 = row.get(1)?;
        let reexport = row.get::<_, String>(2)? == "reexport";
        let mut target = None;
        if let Some(ancestry) = graph.ancestry(source) {
            let mut paths = leaves.query([id])?;
            while let Some(p) = paths.next()? {
                let proved = graph.provider(&ancestry, &p.get::<_, String>(0)?, reexport);
                if proved.is_none() || (target.is_some() && target != proved) {
                    target = None;
                    break;
                }
                target = proved;
            }
        }
        update.execute(params![
            id,
            target,
            if target.is_some() {
                "exact"
            } else {
                "unsupported"
            }
        ])?;
        count += 1;
    }
    Ok(count)
}
