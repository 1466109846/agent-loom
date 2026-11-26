use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use loom_parse::{Extractor, Parsed};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::{IndexReport, REVISION, SCHEMA, resolve, rust_declarations, scan};
use crate::{
    Store,
    redact::{redact, sensitive},
    util::now_ts,
};

struct Old {
    id: i64,
    hash: String,
    state: String,
    size: i64,
}
fn previous(conn: &Connection) -> Result<BTreeMap<String, Old>> {
    let mut st=conn.prepare("SELECT f.path,f.id,f.sha256,s.state,f.size FROM structure_files s JOIN files f ON f.id=s.file_id")?;
    Ok(st
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                Old {
                    id: r.get(1)?,
                    hash: r.get(2)?,
                    state: r.get(3)?,
                    size: r.get(4)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// 项目里每个 go.mod 所在的目录和它声明的模块路径。清单不是源码，从待索引的文件里拿掉。
/// 读不出模块路径的也保留：它仍是模块边界。
fn go_modules(
    inventory: &mut BTreeMap<String, PathBuf>,
    inputs: &mut scan::InputFiles,
) -> Vec<(String, Option<String>)> {
    let mut modules = Vec::new();
    inventory.retain(|key, real| {
        let dir = if key == "go.mod" {
            ""
        } else if let Some(dir) = key.strip_suffix("/go.mod") {
            dir
        } else {
            return true;
        };
        let input = scan::read(real);
        inputs.insert(("go", key.clone()), input.hash);
        let module = input.text.as_deref().and_then(loom_parse::go_module);
        modules.push((dir.to_owned(), module));
        false
    });
    modules
}

/// 项目里的 tsconfig / jsconfig：路径和文本，别名导入靠它们解释。配置不是源码，
/// 从待索引的文件里拿掉。读不出文本的也保留：它照样管着所在的目录。
fn ecma_configs(
    inventory: &mut BTreeMap<String, PathBuf>,
    inputs: &mut scan::InputFiles,
) -> Vec<(String, Option<String>)> {
    let mut configs = Vec::new();
    inventory.retain(|key, real| {
        if !loom_parse::ecma_config(key.rsplit('/').next().unwrap_or(key)) {
            return true;
        }
        let input = scan::read(real);
        inputs.insert(("ecma", key.clone()), input.hash);
        configs.push((key.clone(), input.text));
        false
    });
    configs
}

/// 导入要按 tsconfig / jsconfig 解释的源码。
fn aliased(path: &str) -> bool {
    matches!(
        loom_parse::language(path),
        Some("javascript" | "typescript" | "vue")
    )
}

fn python_packages<'a>(paths: impl Iterator<Item = &'a String>) -> HashSet<String> {
    paths
        .filter_map(|path| {
            let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
            (name == "__init__.py").then(|| dir.to_owned())
        })
        .collect()
}

fn python_package_changed(path: &str, changed: &HashSet<String>) -> bool {
    !changed.is_empty()
        && path.ends_with(".py")
        && (changed.contains("")
            || path
                .match_indices('/')
                .any(|(i, _)| changed.contains(&path[..i])))
}

/// 源码之外、却能改变解析结论的输入（如各模块的路径）。只比较摘要。
fn input_changed(conn: &Connection, name: &str, value: &impl serde::Serialize) -> Result<bool> {
    let digest = crate::util::sha256_hex(&serde_json::to_vec(value)?);
    let old: Option<String> = conn
        .query_row(
            "SELECT digest FROM structure_inputs WHERE name=?1",
            [name],
            |r| r.get(0),
        )
        .optional()?;
    if old.as_ref() == Some(&digest) {
        return Ok(false);
    }
    conn.execute("INSERT INTO structure_inputs(name,digest) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET digest=excluded.digest", params![name, digest])?;
    Ok(true)
}

/// 别的文件能用来找到这个文件的全部写法（它提供的声明键，以及“声明键里的某个顶层名字”），
/// 外加它所属的根：根变了，指向它的引用算不算确定也跟着变。
fn reach(conn: &Connection, file_id: i64) -> Result<(BTreeSet<String>, Option<String>)> {
    let mut keys = conn.prepare_cached("SELECT key FROM structure_provides WHERE file_id=?1")?;
    let keys: Vec<String> = keys
        .query_map([file_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut names = conn.prepare_cached("SELECT name FROM structure_members WHERE file_id=?1")?;
    let names: Vec<String> = names
        .query_map([file_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut all = BTreeSet::new();
    for key in &keys {
        all.extend(names.iter().map(|name| loom_parse::member(name, key)));
    }
    all.extend(keys);
    let domain = conn
        .prepare_cached("SELECT domain FROM structure_domains WHERE file_id=?1")?
        .query_row([file_id], |r| r.get(0))
        .optional()?;
    Ok((all, domain))
}

/// 名字像不像凭据。索引自己拼出来的键以冒号分段（`sym:名字:键`、`crate::a::b`），要逐段去对：
/// 凭据里没有冒号，而整串去对的话 `sym:GetToken:go:…` 会被当成“token: 值”。
fn secret(key: &str) -> bool {
    key.split(':').any(sensitive)
}

fn replace_evidence(
    conn: &Connection,
    file_id: i64,
    mut parsed: Parsed,
    mut keys: Vec<String>,
) -> Result<()> {
    // 解析器还会从声明、清单和别名派生内部定位键。这些键也不能携带明文或用假名连边。
    parsed
        .rust_modules
        .retain(|m| !secret(&m.name) && !m.candidates.iter().any(|p| secret(p)));
    parsed.rust_items.retain(|i| !secret(&i.name));
    keys.retain(|k| !secret(k));
    parsed.provides.retain(|k| !secret(k));
    rust_declarations::replace_file(conn, file_id, &parsed)?;
    conn.execute("DELETE FROM structure_refs WHERE source_id=?1", [file_id])?;
    conn.execute("DELETE FROM symbols WHERE file_id=?1", [file_id])?;
    conn.execute("DELETE FROM structure_members WHERE file_id=?1", [file_id])?;
    conn.execute("DELETE FROM structure_provides WHERE file_id=?1", [file_id])?;
    conn.execute("DELETE FROM structure_domains WHERE file_id=?1", [file_id])?;
    conn.execute("DELETE FROM structure_search WHERE file_id=?1", [file_id])?;
    let mut symbol = conn.prepare_cached(
        "INSERT INTO symbols(file_id,name,kind,line_start,exported) VALUES(?1,?2,?3,?4,?5)",
    )?;
    // 只有提供声明键的文件才会被别处按“键里的名字”找到，按名字的查找只为它们准备。
    // 给每种语言的每个符号都建名字索引的话，改一个文件就要改写与符号数相当的页。
    let named = !keys.is_empty() || !parsed.provides.is_empty();
    let mut member = conn.prepare_cached("INSERT INTO structure_members(name,file_id,exported) VALUES(?1,?2,?3) ON CONFLICT(name,file_id) DO UPDATE SET exported=max(exported,excluded.exported)")?;
    for mut s in parsed.symbols {
        let hidden = secret(&s.name);
        if hidden {
            s.name = redact(&s.name).0;
        }
        symbol.execute(params![
            file_id,
            s.name,
            s.kind,
            i64::try_from(s.line)?,
            s.exported
        ])?;
        if named && !hidden {
            member.execute(params![s.name, file_id, s.exported])?;
        }
    }
    let mut provide =
        conn.prepare_cached("INSERT OR IGNORE INTO structure_provides(key,file_id) VALUES(?1,?2)")?;
    for key in keys.into_iter().chain(parsed.provides) {
        provide.execute(params![key, file_id])?;
    }
    if let Some(domain) = parsed.domain.filter(|d| !secret(d)) {
        conn.prepare_cached("INSERT INTO structure_domains(file_id,domain) VALUES(?1,?2)")?
            .execute(params![file_id, domain])?;
    }
    let mut search =
        conn.prepare_cached("INSERT OR IGNORE INTO structure_search(file_id,key) VALUES(?1,?2)")?;
    for key in parsed.search.into_iter().filter(|k| !secret(k)) {
        search.execute(params![file_id, key])?;
    }
    let mut reference = conn.prepare_cached(
        "INSERT INTO structure_refs(source_id,line,spelling,kind,certain) VALUES(?1,?2,?3,?4,?5)",
    )?;
    // 同一写法出现在两组里时，先写入的较强一组生效。
    let mut candidate = conn.prepare_cached(
        "INSERT OR IGNORE INTO structure_candidates(ref_id,path,substitute) VALUES(?1,?2,?3)",
    )?;
    for r in parsed.refs {
        if secret(&r.spelling)
            || r.rust_paths
                .iter()
                .chain(&r.candidates)
                .chain(&r.substitutes)
                .chain(&r.fallbacks)
                .any(|p| secret(p))
        {
            reference.execute(params![
                file_id,
                i64::try_from(r.line)?,
                redact(&r.spelling).0,
                "unsupported",
                false
            ])?;
            continue;
        }
        reference.execute(params![
            file_id,
            i64::try_from(r.line)?,
            r.spelling,
            r.kind,
            r.exact
        ])?;
        let id = conn.last_insert_rowid();
        for path in r.rust_paths {
            conn.prepare_cached("INSERT INTO structure_rust_paths(ref_id,path) VALUES(?1,?2)")?
                .execute(params![id, path])?;
        }
        for (tier, group) in [r.candidates, r.substitutes, r.fallbacks]
            .into_iter()
            .enumerate()
        {
            for path in group {
                candidate.execute(params![id, path, i64::try_from(tier)?])?;
            }
        }
    }
    Ok(())
}

fn relink(conn: &Connection, touched: &BTreeSet<String>, changed: &BTreeSet<i64>) -> Result<usize> {
    let mut targets = BTreeSet::<i64>::new();
    let mut possible =
        conn.prepare_cached("SELECT ref_id FROM structure_candidates WHERE path=?1")?;
    // 没把这个名字写进候选、却会经搜索键找到它的引用：只是提到同一个名字的那些。
    let mut searching = conn.prepare_cached("SELECT r.id FROM structure_search s JOIN structure_refs r ON r.source_id=s.file_id WHERE s.key=?1 AND r.kind='mention' AND r.spelling=?2")?;
    for path in touched {
        for id in possible.query_map([path], |r| r.get::<_, i64>(0))? {
            targets.insert(id?);
        }
        if let Some((name, key)) = loom_parse::member_parts(path) {
            for id in searching.query_map([key, name], |r| r.get::<_, i64>(0))? {
                targets.insert(id?);
            }
        }
    }
    let mut source = conn.prepare_cached("SELECT id FROM structure_refs WHERE source_id=?1 AND NOT EXISTS(SELECT 1 FROM structure_rust_paths p WHERE p.ref_id=structure_refs.id)")?;
    for file_id in changed {
        for id in source.query_map([file_id], |r| r.get::<_, i64>(0))? {
            targets.insert(id?);
        }
    }
    let mut set =
        conn.prepare_cached("UPDATE structure_refs SET target_id=?2,resolution=?3 WHERE id=?1")?;
    for &id in &targets {
        let found = resolve::resolve(conn, id)?;
        let (resolution, target) = match (found.kind.as_str(), found.hits.as_slice()) {
            ("external", _) => ("external", None),
            ("unsupported", _) => ("unsupported", None),
            // 只是提到一个名字：项目里没有对应的声明就与项目无关，不算缺口。
            ("mention", []) => ("none", None),
            (_, []) => ("missing", None),
            (_, [hit]) if hit.state != "indexed" => ("uncovered_target", None),
            // 只有书写的目标本身才可能算“静态确定”；按名字找到的还要与使用方同属一个根。
            (_, [hit]) => (
                if found.exact && hit.tier == 0 && hit.proven {
                    "exact"
                } else {
                    "likely"
                },
                Some(hit.file),
            ),
            _ => ("ambiguous", None),
        };
        set.execute(params![id, target, resolution])?;
    }
    Ok(targets.len())
}

impl Store {
    pub fn index_structure(&self) -> Result<IndexReport> {
        let project = self.project().context("索引必须绑定到一个项目")?;
        project.verify_live_root()?;
        // 同一库两次并发索引必须先取得写锁再观察目录，防止旧扫描覆盖较新的快照。
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut blocked = BTreeSet::new();
        let scan::Found {
            files: mut inventory,
            scope,
            withheld,
            mut inputs,
            excluded_dirs,
            excluded_files,
        } = scan::discover(project.root(), &mut blocked)?;
        // 项目库可位于根内任意位置，甚至带源码扩展名。库永远不是源码。
        let database = std::path::Path::new(self.conn.path().context("未能核对项目库的位置")?)
            .canonicalize()?;
        inventory.retain(|key, path| {
            if path == &database {
                blocked.insert(key.clone());
            }
            path != &database
        });
        let roots = rust_declarations::package_roots(&mut inventory, &mut inputs);
        let modules = go_modules(&mut inventory, &mut inputs);
        let configs = ecma_configs(&mut inventory, &mut inputs);
        let packages = python_packages(inventory.keys());
        tx.execute_batch(SCHEMA)?;
        let stored = tx
            .query_row(
                "SELECT revision FROM structure_snapshot WHERE singleton=1",
                [],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        ensure!(
            stored.is_none_or(|v| v <= REVISION),
            "该结构索引由较新版本生成，未修改"
        );
        // 修订 4 之前建的候选表没有替代标记。候选随修订递增整体重算，这里只需补列。
        if tx
            .prepare("SELECT substitute FROM structure_candidates LIMIT 0")
            .is_err()
        {
            tx.execute_batch(
                "ALTER TABLE structure_candidates ADD COLUMN substitute INTEGER NOT NULL DEFAULT 0",
            )?;
        }
        // 修订不同时每个文件都会重算、每条引用都会重连，不必再按键去找受影响的引用。
        let rebuild = stored != Some(REVISION);
        let mut old = previous(&tx)?;
        let mut touched = BTreeSet::new();
        let mut changed = BTreeSet::new();
        let mut rust_changed = rust_declarations::inputs_changed(&tx, &roots, &blocked)?;
        // Go 文件提供的键和导入的归类都取决于模块路径；go.mod 变了，它们整体重算。
        let go_changed = input_changed(&tx, "go", &modules)?;
        // 别名指向哪里取决于 tsconfig / jsconfig；哪一份变了，JS、TS 和 Vue 文件都整体重算。
        let ecma_changed = input_changed(&tx, "ecma", &configs)?;
        let mut report = IndexReport {
            scope,
            withheld,
            excluded_dirs,
            excluded_files,
            inputs: crate::util::sha256_hex(&serde_json::to_vec(&(
                scope,
                inputs
                    .iter()
                    .map(|((kind, path), hash)| (kind, path, hash))
                    .collect::<Vec<_>>(),
            ))?),
            seen: i64::try_from(inventory.len())?,
            ..Default::default()
        };
        let mut extractor = Extractor::default();
        extractor.set_go_modules(modules);
        extractor.set_ecma_configs(configs);
        // __init__.py 仍是受管源码。只有标记的增删会改变目录的包状态，改内容不会。
        let changed_packages = packages
            .symmetric_difference(&python_packages(old.keys()))
            .cloned()
            .collect();
        extractor.set_python_packages(packages);
        let stamp = now_ts();
        for (path, real) in inventory {
            let before = old.remove(&path);
            let input = scan::read(&real);
            let size = i64::try_from(input.size)?;
            let same = !rebuild
                && !(go_changed && path.ends_with(".go"))
                && !(ecma_changed && aliased(&path))
                && !python_package_changed(&path, &changed_packages)
                && before.as_ref().is_some_and(|o| {
                    o.hash == input.hash
                        && o.size == size
                        && if input.state == "read" {
                            matches!(
                                o.state.as_str(),
                                "indexed" | "syntax_error" | "too_many_facts"
                            )
                        } else {
                            o.state == input.state
                        }
                });
            if same {
                report.unchanged += 1;
                continue;
            }
            rust_changed |= path.ends_with(".rs");
            let mut facts = Parsed::default();
            let mut state = input.state;
            let mut loc = None;
            if let Some(text) = input.text {
                loc = Some(i64::try_from(text.lines().count())?);
                facts = extractor.extract(&path, &text)?;
                report.parsed += 1;
                state = if facts.syntax_error {
                    "syntax_error"
                } else if facts.fact_count() > 4096 {
                    "too_many_facts"
                } else {
                    "indexed"
                };
                if state != "indexed" {
                    facts = Parsed::default();
                }
            }
            let id:i64=tx.prepare_cached("INSERT INTO files(path,lang,size,loc,sha256,indexed_at) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(path) DO UPDATE SET lang=excluded.lang,size=excluded.size,loc=excluded.loc,sha256=excluded.sha256,indexed_at=excluded.indexed_at RETURNING id")?.query_row(params![path,loom_parse::language(&path),size,loc,input.hash,stamp],|r|r.get(0))?;
            tx.prepare_cached("INSERT INTO structure_files(file_id,state) VALUES(?1,?2) ON CONFLICT(file_id) DO UPDATE SET state=excluded.state")?.execute(params![id,state])?;
            let reshaped = before.as_ref().is_none_or(|o| o.state != state);
            // 修订变了，或模块路径变了而这是 Go 文件：按这些键找目标的引用本来就全部重连。
            let earlier = if rebuild || (go_changed && path.ends_with(".go")) {
                None
            } else {
                Some(reach(&tx, id)?)
            };
            replace_evidence(&tx, id, facts, extractor.path_keys(&path))?;
            changed.insert(id);
            if let Some((names, root)) = earlier {
                let (now, now_root) = reach(&tx, id)?;
                if reshaped || root != now_root {
                    // 是否覆盖、属于哪个根变了：指向它的引用无论按什么找到它，结论都可能不同。
                    touched.extend(names.union(&now).cloned());
                } else {
                    touched.extend(names.symmetric_difference(&now).cloned());
                }
            }
            // 路径型引用只受候选是否存在/覆盖影响；声明型引用在下方单独处理。
            if reshaped {
                touched.insert(path);
            }
        }
        for (path, file) in old {
            rust_changed |= path.ends_with(".rs");
            if !rebuild {
                touched.extend(reach(&tx, file.id)?.0);
            }
            tx.execute("DELETE FROM files WHERE id=?1", [file.id])?;
            touched.insert(path);
            report.removed += 1;
        }
        report.relinked = relink(&tx, &touched, &changed)?;
        // 任一 Rust 声明都可能改变中间链或引入另一个根。只复核声明型引用，
        // 不重新解析未变文件；空更新以及仅改其他语言均跳过。
        if rust_changed {
            report.relinked += rust_declarations::relink(&tx, &roots, &blocked)?;
        }
        (report.refs, report.relations) = tx.query_row("SELECT (SELECT count(*) FROM structure_refs),(SELECT count(*) FROM structure_refs WHERE target_id IS NOT NULL)", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        // 有目标的引用占多数；只沿目标索引找缺口，不为一轮空刷新排序全部引用。
        let mut counts = tx.prepare("SELECT state,count(*) FROM structure_files WHERE state!='indexed' GROUP BY state
            UNION ALL SELECT 'ref:'||resolution,count(*) FROM structure_refs WHERE target_id IS NULL AND resolution NOT IN ('exact','likely','external','none') GROUP BY resolution")?;
        report.coverage = counts
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        drop(counts);
        for (state, n) in &report.coverage {
            if state.starts_with("ref:") {
                report.unresolved += n;
            } else {
                report.uncovered += n;
            }
        }
        let indexed = report.seen - report.uncovered;
        if indexed > 0 {
            report.coverage.insert("indexed".into(), indexed);
        }
        report.finished_at = now_ts();
        tx.execute("INSERT INTO structure_snapshot(singleton,revision,finished_at,total,uncovered,unresolved) VALUES(1,?1,?2,?3,?4,?5) ON CONFLICT(singleton) DO UPDATE SET revision=excluded.revision,finished_at=excluded.finished_at,total=excluded.total,uncovered=excluded.uncovered,unresolved=excluded.unresolved",params![REVISION,report.finished_at,report.seen,report.uncovered,report.unresolved])?;
        report.generation = tx.query_row(
            "SELECT generation FROM structure_generation WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO structure_publication VALUES(1,?1,?2,?3,?4,?5,?6,?7)",
            params![
                report.generation,
                scope,
                report.inputs,
                i64::try_from(excluded_dirs)?,
                i64::try_from(excluded_files)?,
                i64::try_from(withheld)?,
                report.refs
            ],
        )?;
        tx.execute_batch("DELETE FROM structure_coverage; DELETE FROM structure_input_files;")?;
        for (state, count) in &report.coverage {
            tx.prepare_cached("INSERT INTO structure_coverage VALUES(?1,?2)")?
                .execute(params![state, count])?;
        }
        for ((kind, path), digest) in inputs {
            if !sensitive(&path) {
                tx.prepare_cached("INSERT INTO structure_input_files VALUES(?1,?2,?3)")?
                    .execute(params![kind, path, digest])?;
            }
        }
        project.verify_live_root()?;
        tx.commit()?;
        Ok(report)
    }
}
