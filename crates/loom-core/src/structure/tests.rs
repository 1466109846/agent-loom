mod scope;

use std::{fs, path::Path};

use anyhow::Result;

use super::{Direction, QueryReport};
use crate::{
    Store,
    project::{Bind, ProjectLocation},
};

fn write(root: &Path, name: &str, text: &str) -> Result<()> {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(path, text)?;
    Ok(())
}

fn project(root: &Path, files: &[(&str, &str)]) -> Result<Store> {
    let location = ProjectLocation::select(root, Some(root), None)?;
    location.bind(Bind::Initialize)?;
    for (name, text) in files {
        write(root, name, text)?;
    }
    location.open(true)
}

fn outgoing(s: &Store, path: &str) -> Result<Vec<(String, String)>> {
    Ok(s.query_structure(path, Direction::Outgoing, 1)?
        .relations
        .into_iter()
        .map(|r| (r.target, r.confidence))
        .collect())
}

/// 使用方 → 目标的各条关系：目标、确定程度、使用方行号。
fn edges(s: &Store, path: &str) -> Result<Vec<(String, String, i64)>> {
    Ok(s.query_structure(path, Direction::Outgoing, 1)?
        .relations
        .into_iter()
        .map(|r| (r.target, r.confidence, r.line))
        .collect())
}

fn edge(target: &str, confidence: &str, line: i64) -> (String, String, i64) {
    (target.to_owned(), confidence.to_owned(), line)
}

#[test]
fn generation_and_evidence_are_published_together_or_rolled_back() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let writer = project(
        root,
        &[("entry.ts", "import './target.ts';"), ("target.ts", "")],
    )?;
    let first = writer.index_structure()?.generation;
    assert_eq!(first, 1);
    let reader = ProjectLocation::select(root, Some(root), None)?.open(false)?;
    let check = |out: &QueryReport, g, edges| {
        let snapshot = out.snapshot.as_ref().unwrap();
        assert_eq!((snapshot.generation, snapshot.revision), (Some(g), 8));
        assert_eq!(
            (out.relations.len(), out.coverage.refs),
            (edges, Some(edges as i64))
        );
        assert!(out.coverage.known);
        if edges > 0 {
            assert_eq!(
                (&*out.relations[0].source, &*out.relations[0].target),
                ("entry.ts", "target.ts")
            );
        }
    };
    let old = reader.conn.unchecked_transaction()?;
    let read = |conn: &rusqlite::Connection| {
        super::query::read_snapshot(conn, "target.ts", Direction::Incoming, 1).map(|(out, _)| out)
    };
    check(&read(&old)?, first, 1);
    write(root, "entry.ts", "")?;
    let (writer, next) = std::thread::spawn(move || -> Result<_> {
        let next = writer.index_structure()?.generation;
        Ok((writer, next))
    })
    .join()
    .unwrap()?;
    assert_eq!(next, first + 1);
    check(&read(&old)?, first, 1);
    old.commit()?;
    check(
        &reader.query_structure("target.ts", Direction::Incoming, 1)?,
        next,
        0,
    );
    writer.conn.execute_batch("CREATE TRIGGER reject_publication BEFORE INSERT ON structure_publication BEGIN SELECT RAISE(ABORT,'late failure'); END;")?;
    write(root, "entry.ts", "import './target.ts';")?;
    assert!(writer.index_structure().is_err());
    check(
        &reader.query_structure("target.ts", Direction::Incoming, 1)?,
        next,
        0,
    );
    Ok(())
}

#[test]
fn legacy_snapshot_rebuild_preserves_human_knowledge() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let s = project(temp.path(), &[("leaf.ts", "export const leaf=1;")])?;
    s.index_structure()?;
    let note = s.note_set(
        "leaf.ts",
        &crate::util::sha256_hex(b"export const leaf=1;"),
        "提供叶节点",
    )?;
    let intent = s.intent_propose("constraint", "保持离线", "拒绝向网络查询文件内容。")?;
    s.intent_confirm(&intent)?;
    let confirmed = s.intent_get(&intent)?;
    let saved = serde_json::to_value(s.remember(crate::NewMemory {
        mem_type: Some(crate::MemoryType::Decision),
        title: "离线工作".into(),
        body: "只使用受管文件和本机索引。".into(),
        bypass_admission: true,
        ..Default::default()
    })?)?;
    let id = saved["id"].as_str().unwrap();
    let memory = serde_json::to_value(s.get(id)?)?;
    s.conn.execute_batch("DROP TRIGGER structure_generation_insert; DROP TRIGGER structure_generation_update; DROP TABLE structure_generation; DROP TABLE structure_publication; DROP TABLE structure_coverage; DROP TABLE structure_input_files; DROP INDEX structure_rust_module_path;")?;
    let old = s.query_structure("leaf.ts", Direction::Incoming, 1)?;
    assert_eq!(old.status, "indexed");
    assert_eq!(old.snapshot.as_ref().unwrap().generation, None);
    assert!(!old.coverage.known && old.coverage.uncovered.is_none() && old.coverage.refs.is_none());
    assert!(old.verification.inputs.iter().all(|i| i.state == "unknown"));
    assert_eq!(old.conclusion.gaps, ["coverage_unknown"]);
    assert!(old.to_string().contains("旧版") && old.to_string().contains("loom index"));
    for rebuilt in [false, true] {
        if rebuilt {
            assert_eq!(s.index_structure()?.unchanged, 1);
            let out = s.query_structure("leaf.ts", Direction::Incoming, 1)?;
            assert!(out.coverage.known && out.coverage.uncovered.unwrap().is_empty());
            assert_eq!(out.conclusion.verdict, "none_in_snapshot");
        }
        assert_eq!(s.note_get("leaf.ts")?, note);
        assert_eq!(s.intent_get(&intent)?, confirmed);
        assert_eq!(serde_json::to_value(s.get(id)?)?, memory);
    }
    Ok(())
}

#[test]
fn changed_rust_declaration_chain_is_verified_without_refresh() -> Result<()> {
    let d = tempfile::tempdir()?;
    let s = project(
        d.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname=\"demo\"\nversion=\"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub mod inner; pub mod users;"),
            ("src/inner.rs", "pub mod leaf;"),
            ("src/inner/leaf.rs", "pub struct Leaf;"),
            ("src/users.rs", "pub mod read;"),
            ("src/users/read.rs", "use crate::inner::leaf::Leaf;"),
        ],
    )?;
    s.index_structure()?;
    write(d.path(), "src/inner.rs", "// removed declaration")?;
    let out = s.query_structure("src/users/read.rs", Direction::Outgoing, 1)?;
    assert_eq!(out.conclusion.verdict, "found");
    assert_eq!(out.conclusion.gaps, ["changed"]);
    assert_eq!(
        out.verification
            .files
            .iter()
            .map(|f| (&*f.path, f.state))
            .collect::<Vec<_>>(),
        [
            ("src/users/read.rs", "same"),
            ("src/inner/leaf.rs", "same"),
            ("src/users.rs", "same"),
            ("src/lib.rs", "same"),
            ("src/inner.rs", "changed")
        ]
    );
    assert!(
        out.to_string().contains("已变化：src/inner.rs")
            && out.to_string().contains("其余文件未核对")
    );
    Ok(())
}

#[test]
fn older_writer_advances_generation_without_reusing_coverage() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let s = project(temp.path(), &[("leaf.ts", ""), ("bad.ts", "import {")])?;
    let g = s.index_structure()?.generation;
    s.conn
        .execute_batch("UPDATE structure_snapshot SET finished_at=finished_at WHERE singleton=1")?;
    let out = s.query_structure("leaf.ts", Direction::Incoming, 1)?;
    let snapshot = out.snapshot.as_ref().unwrap();
    assert_eq!(snapshot.generation, Some(g + 1));
    assert!(snapshot.inputs.is_none() && snapshot.scope.is_none());
    assert_eq!(out.coverage.uncovered_files, 1);
    assert!(
        !out.coverage.known
            && out.coverage.uncovered.is_none()
            && out.coverage.excluded_dirs.is_none()
    );
    assert_eq!(out.conclusion.gaps, ["uncovered_files", "coverage_unknown"]);
    assert!(out.verification.inputs.iter().all(|i| i.state == "unknown"));
    assert!(out.to_string().contains("loom index"));
    Ok(())
}

/// 文本上限是硬保证：任何上限下都不超、不留半行；空间不够时先丢证据，结论留到最后。
#[test]
fn report_text_never_exceeds_any_limit_or_cuts_a_line() -> Result<()> {
    let d = tempfile::tempdir()?;
    let r = d.path();
    let mut hub = String::new();
    for i in 0..10 {
        let name = format!("很长的目录名用来撑大每一行/模块{i:02}.ts");
        write(r, &name, "import '../hub.ts';\nexport const v = 1;\n")?;
        hub.push_str(&format!("import './{name}';\n"));
    }
    hub.push_str("import './没有这个文件.ts';\nexport function 入口() {}\n");
    let s = project(r, &[("hub.ts", &hub), ("坏.ts", "import {")])?;
    s.index_structure()?;
    write(
        r,
        "很长的目录名用来撑大每一行/模块00.ts",
        "export const v = 2;\n",
    )?;
    for direction in [Direction::Outgoing, Direction::Incoming] {
        let out = s.query_structure("hub.ts", direction, 2)?;
        assert_eq!(out.conclusion.verdict, "found");
        let full = out.text(usize::MAX);
        let last = full.lines().last().unwrap();
        assert!(last.starts_with("结论：") && full.contains("已变化："));
        for limit in 0..=full.len() + 1 {
            let text = out.text(limit);
            assert!(text.len() <= limit, "上限 {limit}，实际 {}", text.len());
            assert!(
                text.is_empty() || text.ends_with('\n'),
                "上限 {limit} 留了半行"
            );
            for row in text.lines() {
                assert!(
                    full.lines().any(|l| l == row)
                        || row.starts_with("预览省略 ")
                        || row.contains("（路径省略）")
                        || row.contains("（目标路径省略）"),
                    "上限 {limit}：{row}"
                );
            }
            // 放得下结论就必须有结论；证据行只在结论之后才有资格占地方。
            if text.contains(" → ") || limit > last.len() {
                assert!(text.lines().any(|l| l == last), "上限 {limit} 丢了结论");
            }
        }
    }
    Ok(())
}

/// 没能定下目标的引用：使用方、写法、状态、列出的候选。
fn gaps(report: &QueryReport) -> Vec<(&str, &str, &str, Vec<&str>)> {
    report
        .issues
        .iter()
        .map(|i| {
            (
                i.source.as_str(),
                i.spelling.as_str(),
                i.state.as_str(),
                i.candidates.iter().map(String::as_str).collect(),
            )
        })
        .collect()
}

/// 修订 4 之前的候选表没有替代标记，修订 6 之前没有声明键这几张表。升级后的首次索引要
/// 补齐并整体重算，不能报错，也不能沿用旧结论；表结构已是新的、只有修订号落后时同样整体重算。
#[test]
fn an_index_from_an_older_revision_is_upgraded_in_place() -> Result<()> {
    let d = tempfile::tempdir()?;
    let s = project(
        d.path(),
        &[
            ("a.ts", "import './store.js';\n"),
            ("store.ts", "export const s = 1;\n"),
        ],
    )?;
    s.index_structure()?;
    s.conn.execute_batch(
        "DROP TABLE structure_candidates;
         CREATE TABLE structure_candidates(
          ref_id INTEGER NOT NULL REFERENCES structure_refs(id) ON DELETE CASCADE, path TEXT NOT NULL,
          PRIMARY KEY(ref_id,path)) WITHOUT ROWID;
         CREATE INDEX structure_candidate_path ON structure_candidates(path,ref_id);
         INSERT INTO structure_candidates SELECT id,'store.js' FROM structure_refs;
         UPDATE structure_refs SET target_id=NULL,resolution='missing';
         UPDATE structure_snapshot SET revision=3;",
    )?;
    assert!(
        s.query_structure("a.ts", Direction::Outgoing, 1).is_err(),
        "旧快照不能被当作当前结论读取"
    );
    let report = s.index_structure()?;
    assert_eq!((report.parsed, report.unchanged), (2, 0));
    assert_eq!(
        outgoing(&s, "a.ts")?,
        [("store.ts".to_owned(), "likely".to_owned())]
    );
    let again = s.index_structure()?;
    assert_eq!((again.parsed, again.unchanged), (0, 2));
    s.conn.execute_batch(
        "UPDATE structure_refs SET target_id=NULL,resolution='missing';
         UPDATE structure_snapshot SET revision=4;",
    )?;
    assert!(s.query_structure("a.ts", Direction::Outgoing, 1).is_err());
    let report = s.index_structure()?;
    assert_eq!((report.parsed, report.unchanged), (2, 0));
    assert_eq!(
        outgoing(&s, "a.ts")?,
        [("store.ts".to_owned(), "likely".to_owned())]
    );
    // 修订 5 的库：加入一个按名字找目标的文件对，确认新表建起来就用得上。
    write(d.path(), "p/A.java", "package p;\nclass A { B b; }\n")?;
    write(d.path(), "p/B.java", "package p;\nclass B {}\n")?;
    s.conn.execute_batch(
        "DROP TABLE structure_provides;
         DROP TABLE structure_domains;
         DROP TABLE structure_search;
         DROP TABLE structure_inputs;
         DROP TABLE structure_members;
         UPDATE structure_refs SET target_id=NULL,resolution='missing';
         UPDATE structure_snapshot SET revision=5;",
    )?;
    assert!(s.query_structure("a.ts", Direction::Outgoing, 1).is_err());
    let report = s.index_structure()?;
    assert_eq!((report.parsed, report.unchanged), (4, 0));
    assert_eq!(
        outgoing(&s, "a.ts")?,
        [("store.ts".to_owned(), "likely".to_owned())]
    );
    assert_eq!(
        outgoing(&s, "p/A.java")?,
        [("p/B.java".to_owned(), "likely".to_owned())]
    );
    let again = s.index_structure()?;
    assert_eq!((again.parsed, again.unchanged), (0, 4));
    // 按名字的查找只为提供声明键的文件准备；脚本的符号不进这张表，改它们不必动名字索引。
    let named: Vec<String> = s
        .conn
        .prepare(
            "SELECT f.path||' '||m.name FROM structure_members m JOIN files f ON f.id=m.file_id ORDER BY 1",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(named, ["p/A.java A", "p/B.java B"]);
    // 修订 6 的库：表结构没变，只是还没按 tsconfig 解释过别名。
    s.conn.execute_batch(
        "UPDATE structure_refs SET target_id=NULL,resolution='missing';
         UPDATE structure_snapshot SET revision=6;",
    )?;
    assert!(s.query_structure("a.ts", Direction::Outgoing, 1).is_err());
    let report = s.index_structure()?;
    assert_eq!((report.parsed, report.unchanged), (4, 0));
    assert_eq!(
        outgoing(&s, "a.ts")?,
        [("store.ts".to_owned(), "likely".to_owned())]
    );
    // 修订 7 的库：内容指纹没变，动态导入的旧证据没有候选。
    write(d.path(), "a.ts", "import('./store');\n")?;
    s.index_structure()?;
    s.conn.execute_batch(
        "DELETE FROM structure_candidates WHERE ref_id IN (SELECT id FROM structure_refs WHERE spelling='./store');
         UPDATE structure_refs SET kind='unsupported', spelling='import', target_id=NULL, resolution='unsupported' WHERE spelling='./store';
         UPDATE structure_snapshot SET revision=7;",
    )?;
    assert!(s.query_structure("a.ts", Direction::Outgoing, 1).is_err());
    let report = s.index_structure()?;
    assert_eq!((report.parsed, report.unchanged), (4, 0));
    assert_eq!(edges(&s, "a.ts")?, [edge("store.ts", "likely", 1)]);
    let again = s.index_structure()?;
    assert_eq!((again.parsed, again.unchanged), (0, 4));
    Ok(())
}

/// Java 的引用按“包 + 顶层类型”找文件。声明改名、另一处出现同名声明、多余的声明删除，
/// 都要让没有重新解析的引用方跟着改结论。
#[test]
fn names_are_found_through_declarations_and_follow_them_when_they_change() -> Result<()> {
    let d = tempfile::tempdir()?;
    let r = d.path();
    let s = project(
        r,
        &[
            ("src/p/A.java", "package p;\nclass A { B b; }\n"),
            ("src/p/B.java", "package p;\npublic class B {}\n"),
            (
                "src/q/C.java",
                "package q;\nimport p.B;\nclass C { B b; String s; }\n",
            ),
        ],
    )?;
    let first = s.index_structure()?;
    assert_eq!((first.parsed, first.relations, first.unresolved), (3, 2, 0));
    // 同包的名字只算可能；同一个源根里的导入是确定的。
    assert_eq!(
        edges(&s, "src/p/A.java")?,
        [edge("src/p/B.java", "likely", 2)]
    );
    assert_eq!(
        edges(&s, "src/q/C.java")?,
        [edge("src/p/B.java", "exact", 2)]
    );
    // `String` 这样对不上任何声明的名字与项目无关，不列为缺口。
    assert!(
        s.query_structure("src/q/C.java", Direction::Outgoing, 1)?
            .issues
            .is_empty()
    );

    write(r, "src/p/B.java", "package p;\npublic class Renamed {}\n")?;
    let renamed = s.index_structure()?;
    assert_eq!((renamed.parsed, renamed.unchanged), (1, 2));
    assert_eq!((renamed.relations, renamed.unresolved), (0, 1));
    let c = s.query_structure("src/q/C.java", Direction::Outgoing, 1)?;
    assert!(c.relations.is_empty());
    assert_eq!(gaps(&c), [("src/q/C.java", "p.B", "missing", vec![])]);
    let a = s.query_structure("src/p/A.java", Direction::Outgoing, 1)?;
    assert!(a.relations.is_empty() && a.issues.is_empty());

    // 另一个源根里出现同名声明：按名字找到两个，谁也不选，两边都查得到这处缺口。
    write(r, "src/p/B.java", "package p;\npublic class B {}\n")?;
    write(r, "gen/p/B.java", "package p;\npublic class B {}\n")?;
    let doubled = s.index_structure()?;
    assert_eq!((doubled.parsed, doubled.unchanged), (2, 2));
    assert_eq!((doubled.relations, doubled.unresolved), (0, 2));
    let both = vec!["gen/p/B.java", "src/p/B.java"];
    for target in &both {
        let report = s.query_structure(target, Direction::Incoming, 1)?;
        assert!(report.relations.is_empty());
        assert_eq!(
            gaps(&report),
            [
                ("src/p/A.java", "B", "ambiguous", both.clone()),
                ("src/q/C.java", "p.B", "ambiguous", both.clone()),
            ],
            "{target}"
        );
    }

    fs::remove_file(r.join("gen/p/B.java"))?;
    let restored = s.index_structure()?;
    assert_eq!((restored.parsed, restored.removed), (0, 1));
    assert_eq!((restored.relations, restored.unresolved), (2, 0));
    assert_eq!(
        edges(&s, "src/q/C.java")?,
        [edge("src/p/B.java", "exact", 2)]
    );

    // 另一个源根里的使用方：按名字找到了，但证明不了两个根一起编译，只算可能。
    write(
        r,
        "test/q/CTest.java",
        "package q;\nimport p.B;\nclass CTest { C c; }\n",
    )?;
    s.index_structure()?;
    assert_eq!(
        edges(&s, "test/q/CTest.java")?,
        [
            edge("src/p/B.java", "likely", 2),
            edge("src/q/C.java", "likely", 3)
        ]
    );
    Ok(())
}

/// 按需导入和点导入不把名字写进候选：同包里找不到时，才到这些包里找公开的同名声明。
/// 这样找到的只算可能；声明改名、同包里出现同名声明，都要让没有重新解析的使用方改结论。
#[test]
fn names_fall_back_to_packages_imported_wholesale() -> Result<()> {
    let d = tempfile::tempdir()?;
    let r = d.path();
    let s = project(
        r,
        &[
            (
                "src/web/Handler.java",
                "package web;\nimport spi.*;\nclass Handler { Plugin p; Hidden h; Local l; }\n",
            ),
            (
                "src/spi/Plugin.java",
                "package spi;\npublic interface Plugin {}\nclass Hidden {}\n",
            ),
            ("src/web/Local.java", "package web;\nclass Local {}\n"),
        ],
    )?;
    let first = s.index_structure()?;
    // 包私有的 Hidden 在别的包里看不见；它和按需导入本身都与项目里的文件对不上，不算缺口。
    assert_eq!((first.relations, first.unresolved), (2, 0));
    let via_import = [
        edge("src/spi/Plugin.java", "likely", 3),
        edge("src/web/Local.java", "likely", 3),
    ];
    assert_eq!(edges(&s, "src/web/Handler.java")?, via_import);
    let users = s.query_structure("src/spi/Plugin.java", Direction::Incoming, 1)?;
    assert_eq!(users.relations.len(), 1);
    assert_eq!(users.relations[0].source, "src/web/Handler.java");

    // 同包里出现同名类型：它遮住按需导入的那个。
    write(r, "src/web/Plugin.java", "package web;\nclass Plugin {}\n")?;
    let shadowed = s.index_structure()?;
    assert_eq!((shadowed.parsed, shadowed.unchanged), (1, 3));
    assert_eq!(
        edges(&s, "src/web/Handler.java")?,
        [
            edge("src/web/Local.java", "likely", 3),
            edge("src/web/Plugin.java", "likely", 3)
        ]
    );
    fs::remove_file(r.join("src/web/Plugin.java"))?;
    let restored = s.index_structure()?;
    assert_eq!((restored.parsed, restored.removed), (0, 1));
    assert_eq!(edges(&s, "src/web/Handler.java")?, via_import);

    // 被按需导入的包里，声明改了名。
    write(
        r,
        "src/spi/Plugin.java",
        "package spi;\npublic interface Renamed {}\n",
    )?;
    let renamed = s.index_structure()?;
    assert_eq!((renamed.parsed, renamed.unchanged), (1, 2));
    assert_eq!(
        edges(&s, "src/web/Handler.java")?,
        [edge("src/web/Local.java", "likely", 3)]
    );
    // 按需导入的两个包里都有：不选，被引用的两边都查得到这处缺口。
    write(
        r,
        "src/spi/Plugin.java",
        "package spi;\npublic interface Plugin {}\n",
    )?;
    write(
        r,
        "src/alt/Plugin.java",
        "package alt;\npublic class Plugin {}\n",
    )?;
    write(
        r,
        "src/web/Handler.java",
        "package web;\nimport spi.*;\nimport alt.*;\nclass Handler { Plugin p; }\n",
    )?;
    s.index_structure()?;
    let both = vec!["src/alt/Plugin.java", "src/spi/Plugin.java"];
    for target in &both {
        let report = s.query_structure(target, Direction::Incoming, 1)?;
        assert!(report.relations.is_empty());
        assert_eq!(
            gaps(&report),
            [("src/web/Handler.java", "Plugin", "ambiguous", both.clone())],
            "{target}"
        );
    }
    Ok(())
}

/// Go 的导入指向包。经包名用到的导出名落到声明它的文件上；模块路径来自 go.mod，
/// 它变了，一个字没动的 Go 文件也要整体重算。
#[test]
fn go_packages_are_placed_by_the_module_path_and_replaced_when_it_changes() -> Result<()> {
    let d = tempfile::tempdir()?;
    let r = d.path();
    let s = project(
        r,
        &[
            ("go.mod", "module example.com/app\n\ngo 1.22\n"),
            (
                "cmd/main.go",
                "package main\n\nimport (\n\t\"example.com/app/store\"\n\t_ \"example.com/app/side\"\n)\n\nfunc main() {\n\tstore.Open()\n\tstore.Reopen()\n\tstore.Close()\n\trun()\n}\n",
            ),
            ("cmd/run.go", "package main\n\nfunc run() {}\n"),
            (
                "store/open.go",
                "package store\n\nfunc Open() { prepare() }\nfunc Reopen() {}\n",
            ),
            (
                "store/close.go",
                "package store\n\nfunc Close() {}\nfunc prepare() {}\n",
            ),
            ("side/side.go", "package side\n"),
        ],
    )?;
    let first = s.index_structure()?;
    // go.mod 是解析的输入，不是受管源码。
    assert_eq!((first.seen, first.parsed), (5, 5));
    assert_eq!((first.relations, first.unresolved), (6, 0));
    // 同一对文件之间只列行号最小的一处：Open 和 Reopen 都在 open.go。
    // 同包的名字和经包名用到的导出名一样，对上唯一的声明就是确定的。
    assert_eq!(
        edges(&s, "cmd/main.go")?,
        [
            edge("cmd/run.go", "exact", 12),
            edge("side/side.go", "exact", 5),
            edge("store/close.go", "exact", 11),
            edge("store/open.go", "exact", 9),
        ]
    );
    assert_eq!(
        edges(&s, "store/open.go")?,
        [edge("store/close.go", "exact", 3)]
    );
    let users = s.query_structure("store/open.go", Direction::Incoming, 1)?;
    assert_eq!(users.relations.len(), 1);
    assert_eq!(
        (users.relations[0].source.as_str(), users.relations[0].line),
        ("cmd/main.go", 9)
    );
    // 没有导出的顶层名字只供同包查找，不作为符号展示。
    let shown: Vec<_> = s
        .query_structure("store/close.go", Direction::Outgoing, 1)?
        .symbols
        .into_iter()
        .map(|symbol| symbol.name)
        .collect();
    assert_eq!(shown, ["Close"]);

    // 包里又多了一个文件：只为副作用的导入不再落得到单个文件上。
    write(r, "side/more.go", "package side\n")?;
    let more = s.index_structure()?;
    assert_eq!((more.parsed, more.unchanged), (1, 5));
    let main = s.query_structure("cmd/main.go", Direction::Outgoing, 1)?;
    assert_eq!(
        gaps(&main),
        [(
            "cmd/main.go",
            "example.com/app/side",
            "ambiguous",
            vec!["side/more.go", "side/side.go"]
        )]
    );

    // 模块改了路径：原来的导入路径不再指向本项目，只剩同包的名字还连得上。
    write(r, "go.mod", "module example.com/renamed\n")?;
    let moved = s.index_structure()?;
    assert_eq!((moved.parsed, moved.unchanged), (6, 0));
    assert_eq!(edges(&s, "cmd/main.go")?, [edge("cmd/run.go", "exact", 12)]);
    let main = s.query_structure("cmd/main.go", Direction::Outgoing, 1)?;
    assert_eq!(
        gaps(&main),
        [
            ("cmd/main.go", "example.com/app/store", "external", vec![]),
            ("cmd/main.go", "example.com/app/side", "external", vec![]),
        ]
    );
    let idle = s.index_structure()?;
    assert_eq!((idle.parsed, idle.unchanged), (0, 6));

    // 没有 go.mod：导入路径对不上目录，不能说它是外部的；同包的名字也证明不了属于哪个模块。
    fs::remove_file(r.join("go.mod"))?;
    let bare = s.index_structure()?;
    assert_eq!((bare.parsed, bare.unresolved), (6, 2));
    let main = s.query_structure("cmd/main.go", Direction::Outgoing, 1)?;
    assert!(gaps(&main).iter().all(|gap| gap.2 == "unsupported"));
    assert_eq!(
        edges(&s, "store/open.go")?,
        [edge("store/close.go", "likely", 3)]
    );
    Ok(())
}

/// 受构建约束的 Go 文件不一定参与构建：它声明的名字找得到，但算不得确定；
/// 约束去掉以后，一个字没动的使用方要跟着改结论。点导入的名字到被导入的包里去找。
#[test]
fn go_build_constraints_and_dot_imports_never_yield_certain_targets() -> Result<()> {
    let d = tempfile::tempdir()?;
    let r = d.path();
    let s = project(
        r,
        &[
            ("go.mod", "module example.com/app\n"),
            (
                "cmd/main.go",
                "package main\n\nimport \"example.com/app/sys\"\n\nfunc main() { sys.Open(); sys.Close() }\n",
            ),
            ("sys/close.go", "package sys\n\nfunc Close() {}\n"),
            ("sys/open_linux.go", "package sys\n\nfunc Open() {}\n"),
            (
                "sys/dot_test.go",
                "package sys_test\n\nimport . \"example.com/app/sys\"\n\nvar _ = Close\nvar _ = Exported\n",
            ),
            (
                "sys/export_test.go",
                "package sys\n\nvar Exported = Close\n",
            ),
        ],
    )?;
    let first = s.index_structure()?;
    assert_eq!(first.unresolved, 0);
    assert_eq!(
        edges(&s, "cmd/main.go")?,
        [
            edge("sys/close.go", "exact", 5),
            edge("sys/open_linux.go", "likely", 5)
        ]
    );
    // 点导入：被测包的导出名，和它的包内测试文件导出的名字。按名字到别的包里找，只算可能。
    assert_eq!(
        edges(&s, "sys/dot_test.go")?,
        [
            edge("sys/close.go", "likely", 5),
            edge("sys/export_test.go", "likely", 6)
        ]
    );
    assert_eq!(
        edges(&s, "sys/export_test.go")?,
        [edge("sys/close.go", "exact", 3)]
    );

    // 另一个平台的实现出现：两个候选，不选。
    write(r, "sys/open_windows.go", "package sys\n\nfunc Open() {}\n")?;
    let doubled = s.index_structure()?;
    assert_eq!((doubled.parsed, doubled.unresolved), (1, 1));
    let main = s.query_structure("cmd/main.go", Direction::Outgoing, 1)?;
    assert_eq!(
        gaps(&main),
        [(
            "cmd/main.go",
            "sys.Open",
            "ambiguous",
            vec!["sys/open_linux.go", "sys/open_windows.go"]
        )]
    );
    fs::remove_file(r.join("sys/open_windows.go"))?;

    // 约束写在构建注释里也一样；去掉以后才算确定。内容变了、声明的名字没变。
    fs::remove_file(r.join("sys/open_linux.go"))?;
    write(
        r,
        "sys/open.go",
        "//go:build linux\n\npackage sys\n\nfunc Open() {}\n",
    )?;
    s.index_structure()?;
    assert_eq!(
        edges(&s, "cmd/main.go")?,
        [
            edge("sys/close.go", "exact", 5),
            edge("sys/open.go", "likely", 5)
        ]
    );
    write(r, "sys/open.go", "package sys\n\nfunc Open() {}\n")?;
    let freed = s.index_structure()?;
    assert_eq!((freed.parsed, freed.unchanged), (1, 4));
    assert_eq!(
        edges(&s, "cmd/main.go")?,
        [
            edge("sys/close.go", "exact", 5),
            edge("sys/open.go", "exact", 5)
        ]
    );
    Ok(())
}

/// C 的引号包含先找使用方旁边的文件，找不到才看包含路径；后者这里读不到，只能按路径后缀去对。
#[test]
fn includes_prefer_the_neighbouring_file_and_fall_back_to_the_include_path() -> Result<()> {
    let d = tempfile::tempdir()?;
    let r = d.path();
    let s = project(
        r,
        &[
            (
                "src/a.c",
                "#include \"x.h\"\n#include <lib/y.h>\n#include <stdio.h>\n",
            ),
            ("src/x.h", "#pragma once\n"),
            ("include/x.h", "#pragma once\n"),
            ("include/lib/y.h", "#pragma once\n"),
        ],
    )?;
    let first = s.index_structure()?;
    assert_eq!((first.relations, first.unresolved), (2, 1));
    // 旁边就有：别处的同名头文件不参与。
    assert_eq!(
        edges(&s, "src/a.c")?,
        [
            edge("include/lib/y.h", "likely", 2),
            edge("src/x.h", "exact", 1)
        ]
    );
    fs::remove_file(r.join("src/x.h"))?;
    let gone = s.index_structure()?;
    assert_eq!((gone.parsed, gone.removed), (0, 1));
    assert_eq!(
        edges(&s, "src/a.c")?,
        [
            edge("include/lib/y.h", "likely", 2),
            edge("include/x.h", "likely", 1)
        ]
    );
    // 包含路径上有两个同名头文件：不选。
    write(r, "third/x.h", "#pragma once\n")?;
    let doubled = s.index_structure()?;
    assert_eq!((doubled.parsed, doubled.unchanged), (1, 3));
    let a = s.query_structure("src/a.c", Direction::Outgoing, 1)?;
    assert_eq!(
        gaps(&a),
        [
            (
                "src/a.c",
                "x.h",
                "ambiguous",
                vec!["include/x.h", "third/x.h"]
            ),
            ("src/a.c", "stdio.h", "missing", vec![]),
        ]
    );
    // 头文件读不了时仍按路径认得它，只是不能当作已覆盖的目标。
    fs::remove_file(r.join("third/x.h"))?;
    fs::write(r.join("include/x.h"), [0xff, 0xfe, 0x00])?;
    s.index_structure()?;
    let a = s.query_structure("src/a.c", Direction::Outgoing, 1)?;
    assert_eq!(
        gaps(&a)[0],
        ("src/a.c", "x.h", "uncovered_target", vec!["include/x.h"])
    );
    assert_eq!(
        edges(&s, "src/a.c")?,
        [edge("include/lib/y.h", "likely", 2)]
    );
    Ok(())
}

/// tsconfig / jsconfig 是解析的输入，不是受管源码。经别名找到的目标只算可能；配置一改，
/// 一个字没动的脚本也要按新的别名重新找目标，别的语言的文件不受影响。
#[test]
fn aliases_follow_the_project_configuration_and_are_only_likely() -> Result<()> {
    let d = tempfile::tempdir()?;
    let r = d.path();
    let s = project(
        r,
        &[
            (
                "tsconfig.json",
                "{\n  // 注释和末尾多余的逗号照 TypeScript 的规矩允许\n  \"compilerOptions\": { \"paths\": { \"@/*\": [\"./src/*\"], }, },\n}\n",
            ),
            (
                "src/app.ts",
                "import { save } from '@/lib/store'\nimport { view } from '@/ui'\nimport x from 'react'\n",
            ),
            ("src/lib/store.ts", "export function save() {}\n"),
            ("src/ui/index.ts", "export const view = 1\n"),
            ("tool.py", "import os\n"),
        ],
    )?;
    let first = s.index_structure()?;
    assert_eq!((first.seen, first.parsed), (4, 4));
    assert_eq!((first.relations, first.unresolved), (2, 2));
    assert_eq!(
        edges(&s, "src/app.ts")?,
        [
            edge("src/lib/store.ts", "likely", 1),
            edge("src/ui/index.ts", "likely", 2)
        ]
    );
    let app = s.query_structure("src/app.ts", Direction::Outgoing, 1)?;
    assert_eq!(gaps(&app), [("src/app.ts", "react", "unsupported", vec![])]);
    let users = s.query_structure("src/lib/store.ts", Direction::Incoming, 1)?;
    assert_eq!(users.relations.len(), 1);
    assert_eq!(users.relations[0].source, "src/app.ts");
    let idle = s.index_structure()?;
    assert_eq!((idle.parsed, idle.unchanged), (0, 4));

    // 别名改了指向：脚本一个字没动也要重新找目标，Python 文件不受影响。
    write(
        r,
        "tsconfig.json",
        "{ \"compilerOptions\": { \"paths\": { \"@/*\": [\"./lib/*\"] } } }\n",
    )?;
    write(r, "lib/ui.ts", "export const view = 2\n")?;
    let moved = s.index_structure()?;
    assert_eq!((moved.parsed, moved.unchanged), (4, 1));
    assert_eq!(edges(&s, "src/app.ts")?, [edge("lib/ui.ts", "likely", 2)]);
    let app = s.query_structure("src/app.ts", Direction::Outgoing, 1)?;
    assert_eq!(
        gaps(&app),
        [
            ("src/app.ts", "@/lib/store", "missing", vec![]),
            ("src/app.ts", "react", "unsupported", vec![]),
        ]
    );
    // 两个候选同时在：不择一。
    write(r, "lib/ui/index.ts", "export const view = 3\n")?;
    let doubled = s.index_structure()?;
    assert_eq!((doubled.parsed, doubled.unchanged), (1, 5));
    let app = s.query_structure("src/app.ts", Direction::Outgoing, 1)?;
    assert_eq!(
        gaps(&app)[1],
        (
            "src/app.ts",
            "@/ui",
            "ambiguous",
            vec!["lib/ui.ts", "lib/ui/index.ts"]
        )
    );

    // 配置删了：别名不再成立，裸名回到无法静态确定。
    fs::remove_file(r.join("tsconfig.json"))?;
    let bare = s.index_structure()?;
    assert_eq!((bare.parsed, bare.unchanged), (5, 1));
    assert!(edges(&s, "src/app.ts")?.is_empty());
    let app = s.query_structure("src/app.ts", Direction::Outgoing, 1)?;
    assert!(gaps(&app).iter().all(|gap| gap.2 == "unsupported"));
    assert_eq!(gaps(&app).len(), 3);
    Ok(())
}

#[test]
fn python_absolute_imports_find_script_roots_not_package_siblings() -> Result<()> {
    let d = tempfile::tempdir()?;
    let r = d.path();
    let s = project(
        r,
        &[
            ("scripts/run.py", "def load():\n    import helper\n"),
            ("scripts/helper.py", ""),
            ("pkg/__init__.py", ""),
            ("pkg/a.py", "import openai\n"),
            ("pkg/openai.py", ""),
            ("tools/handlers/__init__.py", ""),
            ("tools/handlers/x.py", "import config\n"),
            ("tools/config.py", ""),
        ],
    )?;
    s.index_structure()?;
    assert_eq!(
        edges(&s, "scripts/run.py")?,
        [edge("scripts/helper.py", "likely", 2)]
    );
    assert_eq!(
        edges(&s, "tools/handlers/x.py")?,
        [edge("tools/config.py", "likely", 1)]
    );
    let pkg = s.query_structure("pkg/a.py", Direction::Outgoing, 1)?;
    assert!(pkg.relations.is_empty());
    assert_eq!(gaps(&pkg), [("pkg/a.py", "openai", "missing", vec![])]);

    write(r, "helper.py", "")?;
    let report = s.index_structure()?;
    assert_eq!((report.parsed, report.unchanged), (1, 8));
    let script = s.query_structure("scripts/run.py", Direction::Outgoing, 1)?;
    assert!(script.relations.is_empty());
    assert_eq!(
        gaps(&script),
        [(
            "scripts/run.py",
            "helper",
            "ambiguous",
            vec!["helper.py", "scripts/helper.py"]
        )]
    );
    Ok(())
}

#[test]
fn python_package_marker_changes_reparse_only_python_files_below_the_marker() -> Result<()> {
    let d = tempfile::tempdir()?;
    let r = d.path();
    let user = "scripts/sub/run.py";
    let target = "scripts/helper.py";
    let marker = "scripts/__init__.py";
    let s = project(
        r,
        &[
            (user, "from helper import run\n"),
            (target, ""),
            ("scripts/sub/__init__.py", ""),
            ("scripts/ui.ts", ""),
            ("scripts-other/keep.py", ""),
        ],
    )?;
    s.index_structure()?;
    assert_eq!(edges(&s, user)?, [edge(target, "likely", 1)]);
    write(r, marker, "")?;
    let added = s.index_structure()?;
    assert_eq!((added.parsed, added.unchanged), (4, 2));
    assert_eq!(
        s.query_structure(marker, Direction::Outgoing, 1)?.status,
        "indexed"
    );
    let missing = s.query_structure(user, Direction::Outgoing, 1)?;
    assert!(missing.relations.is_empty());
    assert_eq!(gaps(&missing), [(user, "helper", "missing", vec![])]);
    assert!(
        s.query_structure(target, Direction::Incoming, 1)?
            .relations
            .is_empty()
    );

    write(r, marker, "VALUE = 1\n")?;
    let edited = s.index_structure()?;
    assert_eq!((edited.parsed, edited.unchanged), (1, 5));
    fs::remove_file(r.join(marker))?;
    let removed = s.index_structure()?;
    assert_eq!(
        (removed.parsed, removed.unchanged, removed.removed),
        (3, 2, 1)
    );
    assert_eq!(edges(&s, user)?, [edge(target, "likely", 1)]);
    Ok(())
}

#[test]
fn literal_loader_edges_are_only_likely_and_retract_in_both_directions() -> Result<()> {
    let d = tempfile::tempdir()?;
    let user = "main.ts";
    let s = project(
        d.path(),
        &[
            (
                user,
                "import('./page');\nrequire('./a.js');\nrequire('./x');\n",
            ),
            ("page.ts", ""),
            ("a.js", ""),
            ("x.ts", ""),
            ("x/index.js", ""),
        ],
    )?;
    s.index_structure()?;
    assert_eq!(
        edges(&s, user)?,
        [edge("a.js", "likely", 2), edge("page.ts", "likely", 1)]
    );
    let main = s.query_structure(user, Direction::Outgoing, 1)?;
    assert_eq!(
        gaps(&main),
        [(user, "./x", "ambiguous", vec!["x.ts", "x/index.js"])]
    );
    for (target, kind, line) in [("a.js", "require", 2), ("page.ts", "import", 1)] {
        let impact = s.query_structure(target, Direction::Incoming, 1)?;
        assert_eq!(impact.relations.len(), 1);
        let hit = &impact.relations[0];
        assert_eq!(
            (
                hit.source.as_str(),
                hit.kind.as_str(),
                hit.confidence.as_str(),
                hit.line
            ),
            (user, kind, "likely", line)
        );
    }
    write(d.path(), user, "")?;
    let report = s.index_structure()?;
    assert_eq!(
        (
            report.parsed,
            report.unchanged,
            report.relations,
            report.unresolved
        ),
        (1, 4, 0, 0)
    );
    assert!(edges(&s, user)?.is_empty());
    for target in ["a.js", "page.ts"] {
        assert!(
            s.query_structure(target, Direction::Incoming, 1)?
                .relations
                .is_empty()
        );
    }
    Ok(())
}
