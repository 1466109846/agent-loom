use super::*;
use crate::util::sha256_hex;

const SOURCE: &str = "export const value = 1;\n";

#[test]
fn unchanged_head_does_not_hide_changed_inputs_or_prove_new_files_absent() -> Result<()> {
    let d = tempfile::tempdir()?;
    let r = d.path();
    let s = project(
        r,
        &[
            (".git/HEAD", "ref: refs/heads/main\n"),
            (".git/info/exclude", ""),
            (".gitignore", ""),
            ("tsconfig.json", "{}"),
            ("src/a.ts", SOURCE),
            ("go.mod", "module old.test/app\n"),
            ("x.go", "package app\n"),
            ("pkg/main.py", "pass\n"),
        ],
    )?;
    s.index_structure()?;
    assert_eq!(s.conn.query_row("SELECT digest FROM structure_input_files WHERE kind='rules' AND path='.git/info/exclude'",[], |r|r.get::<_,String>(0))?,sha256_hex(b""));
    for (path, config, contents, kind) in [
        (
            "src/a.ts",
            "tsconfig.json",
            r#"{"compilerOptions":{"paths":{"x":["./src/a"]}}}"#,
            "ecma",
        ),
        ("x.go", "go.mod", "module new.test/app\n", "go"),
        ("src/a.ts", ".gitignore", "new/\n", "rules"),
        ("src/a.ts", ".git/info/exclude", "vendor/\n", "rules"),
        ("pkg/main.py", "pkg/__init__.py", "", "python"),
    ] {
        let old = fs::read(r.join(config)).ok();
        write(r, config, contents)?;
        let out = s.query_structure(path, Direction::Incoming, 1)?;
        assert_eq!(out.conclusion.verdict, "gap");
        assert_eq!(out.conclusion.gaps, ["inputs_changed"]);
        assert!(
            out.verification
                .inputs
                .iter()
                .any(|i| i.kind == kind && i.state == "changed")
        );
        if let Some(old) = old {
            fs::write(r.join(config), old)?;
        } else {
            fs::remove_file(r.join(config))?;
        }
    }
    write(r, "new.ts", "import './src/a.ts';")?;
    let out = s.query_structure("src/a.ts", Direction::Incoming, 1)?;
    assert_eq!(out.conclusion.verdict, "none_in_snapshot");
    let text = out.to_string();
    assert!(text.contains("新增的文件查不出") && text.contains("不代表当前项目里没有"));
    for n in 0..17 {
        write(r, &format!("conf{n}/tsconfig.json"), "{}")?;
    }
    s.index_structure()?;
    write(r, "conf0/tsconfig.json", "{ } ")?;
    let check = || s.query_structure("src/a.ts", Direction::Outgoing, 1);
    assert!(
        check()?
            .verification
            .inputs
            .iter()
            .any(|i| i.kind == "ecma" && i.state == "unverifiable")
    );
    fs::write(r.join("src/jsconfig.json"), vec![b' '; 1_048_577])?;
    assert_eq!(check()?.conclusion.gaps, ["inputs_changed"]);
    Ok(())
}

#[test]
fn excluded_paths_remain_private_and_verification_does_not_write() -> Result<()> {
    let d = tempfile::tempdir()?;
    let root = d.path();
    let private = "personal-drafts";
    let s = project(
        root,
        &[
            (".gitignore", "personal-drafts/\nsingle.ts\n"),
            ("personal-drafts/private.ts", "import './secret';"),
            ("personal-drafts/Cargo.toml", ""),
            ("single.ts", SOURCE),
            ("ok.ts", SOURCE),
        ],
    )?;
    fs::write(root.join("personal-drafts/.gitignore"), [0xff])?;
    let index = s.index_structure()?;
    assert_eq!((index.excluded_dirs, index.excluded_files), (1, 1));
    let reader = ProjectLocation::select(root, Some(root), None)?.open(false)?;
    let version = || {
        s.conn
            .pragma_query_value(None, "data_version", |r| r.get::<_, i64>(0))
    };
    let before = version()?;
    write(root, "ok.ts", "export const value=2;")?;
    let report = reader.query_structure("ok.ts", Direction::Incoming, 1)?;
    assert_eq!(report.verification.files[0].state, "changed");
    assert_eq!(version()?, before);
    for text in [
        index.to_string(),
        serde_json::to_string(&index)?,
        report.to_string(),
        serde_json::to_string(&report)?,
    ] {
        assert!(!text.contains(private));
    }
    for table in [
        "structure_publication",
        "structure_generation",
        "structure_coverage",
        "structure_input_files",
    ] {
        let mut q = s.conn.prepare(&format!("SELECT * FROM {table}"))?;
        let n = q.column_count();
        let mut rows = q.query([])?;
        while let Some(row) = rows.next()? {
            for i in 0..n {
                if let rusqlite::types::ValueRef::Text(v) = row.get_ref(i)? {
                    assert!(!std::str::from_utf8(v)?.contains(private), "{table}");
                }
            }
        }
    }
    Ok(())
}

#[test]
fn empty_scope_and_coverage_reasons_are_distinct() -> Result<()> {
    let d = tempfile::tempdir()?;
    let r = d.path();
    let s = project(r, &[(".gitignore", "")])?;
    let none = s.query_structure("a.ts", Direction::Incoming, 1)?;
    assert!(none.snapshot.is_none() && none.verification.files.is_empty());
    assert_eq!(none.conclusion.gaps, ["not_indexed"]);
    fs::create_dir(r.join(".git"))?;
    assert_eq!(s.index_structure()?.excluded_dirs, 0);
    let empty = s.query_structure("a.ts", Direction::Incoming, 1)?;
    assert_eq!(empty.conclusion.gaps, ["absent", "empty_scope"]);
    assert!(empty.verification.inputs.is_empty() && empty.to_string().contains("空范围"));
    fs::write(r.join("large.ts"), vec![b' '; 1_048_577])?;
    fs::write(r.join("bad.ts"), [0xff])?;
    write(r, "invalid.ts", "import {")?;
    write(r, "a.generated.ts", SOURCE)?;
    write(r, "main.ts", "import './gone.ts';")?;
    let index = s.index_structure()?;
    let out = s.query_structure("main.ts", Direction::Incoming, 1)?;
    for state in ["too_large", "invalid_utf8", "syntax_error", "generated"] {
        assert_eq!(index.coverage[state], 1);
        assert_eq!(out.coverage.uncovered.as_ref().unwrap()[state], 1);
    }
    assert_eq!(index.coverage["ref:missing"], 1);
    assert_eq!(out.coverage.unresolved.as_ref().unwrap()["missing"], 1);
    assert_eq!(out.conclusion.gaps, ["unresolved_refs", "uncovered_files"]);
    assert!(out.to_string().contains("不能判断"));
    s.conn.execute_batch("UPDATE structure_publication SET scope='git'||char(27), inputs=char(10)||'unsafe12'; UPDATE structure_coverage SET state='custom'||char(8232)||'code' WHERE state='generated';")?;
    let text = s
        .query_structure("main.ts", Direction::Incoming, 1)?
        .text(1500);
    assert!(
        !text.contains(['\u{1b}', '\u{2028}'])
            && text.contains("范围git ；深度")
            && text.contains("custom code")
    );
    Ok(())
}

fn check_file(s: &Store, path: &str, state: &str) -> Result<()> {
    assert_eq!(
        s.query_structure(path, Direction::Outgoing, 1)?.status,
        state,
        "{path}"
    );
    let hash = sha256_hex(SOURCE.as_bytes());
    if state == "indexed" {
        s.note_set(path, &hash, "提供配置")?;
        assert!(s.note_get(path)?.starts_with("状态：当前\n"), "{path}");
    } else {
        assert!(s.note_set(path, &hash, "不可采信").is_err(), "{path}");
        assert!(
            s.note_get(path)?.starts_with("状态：未覆盖或不可校验\n"),
            "{path}"
        );
    }
    Ok(())
}

#[test]
fn git_scope_admits_source_names_but_not_ignored_or_marked_trees() -> Result<()> {
    let d = tempfile::tempdir()?;
    let r = d.path();
    let s = project(
        r,
        &[
            (".gitignore", "/ignored/\n"),
            (".loom/ignore", "!ignored/\n"),
        ],
    )?;
    let included = [
        "build/x.ts",
        "src/vendor/x.ts",
        "data/x.ts",
        "credentials/x.ts",
        ".config/x.ts",
        "secrets.ts",
        ".eslintrc.js",
        "task-scheduler-configuration.ts",
    ];
    let excluded = [
        "ignored/x.ts",
        "node_modules/x.ts",
        "python-env/x.ts",
        "rust-output/x.ts",
        "nested/x.ts",
        ".git/x.ts",
        ".loom/x.ts",
    ];
    for path in included.into_iter().chain(excluded) {
        write(r, path, SOURCE)?;
    }
    for marker in [
        "pyvenv.cfg",
        "CACHEDIR.TAG",
        "python-env/pyvenv.cfg",
        "rust-output/CACHEDIR.TAG",
        "nested/.loom/project.id",
    ] {
        write(r, marker, "")?;
    }
    // 标记目录必须在其下的规则文件打开前丢弃。
    fs::write(r.join("python-env/.gitignore"), [0xff])?;
    s.index_structure()?;
    for path in included {
        check_file(&s, path, "indexed")?;
    }
    for path in excluded {
        check_file(&s, path, "absent")?;
    }
    Ok(())
}

#[test]
fn only_a_root_ignore_file_switches_a_non_repository_out_of_builtin_mode() -> Result<()> {
    // 系统临时目录不在任何 Git 工作区里，子目录自己的规则不能切换整个项目的范围。
    let d = tempfile::tempdir()?;
    let r = d.path();
    let s = project(
        r,
        &[
            ("src/.gitignore", "!vendor/\nhide.ts\n"),
            ("src/vendor/x.ts", SOURCE),
            ("src/hide.ts", SOURCE),
            ("env/x.ts", SOURCE),
            ("env/pyvenv.cfg", ""),
        ],
    )?;
    assert_eq!(
        serde_json::to_value(s.index_structure()?)?["scope"],
        "builtin"
    );
    check_file(&s, "src/vendor/x.ts", "absent")?;
    check_file(&s, "src/hide.ts", "absent")?;
    check_file(&s, "env/x.ts", "indexed")?;
    write(r, ".gitignore", "")?;
    assert_eq!(serde_json::to_value(s.index_structure()?)?["scope"], "git");
    check_file(&s, "src/vendor/x.ts", "indexed")?;
    check_file(&s, "src/hide.ts", "absent")?;
    check_file(&s, "env/x.ts", "absent")?;
    Ok(())
}

#[test]
fn reachable_project_inherits_rules_in_git_precedence_order() -> Result<()> {
    let d = tempfile::tempdir()?;
    let top = d.path();
    let root = top.join("tmp/sample");
    fs::create_dir_all(&root)?;
    let s = project(&root, &[])?;
    let denied = ["info.ts", "top.ts", "middle.ts", "private.ts"];
    let allowed = [
        "normal.ts",
        "info-pass.ts",
        "middle-pass.ts",
        "root-pass.ts",
        "src/deep-pass.ts",
    ];
    for path in denied.into_iter().chain(allowed) {
        write(&root, path, SOURCE)?;
    }
    s.index_structure()?;
    for path in denied {
        check_file(&s, path, "indexed")?;
    }
    write(
        top,
        ".git/info/exclude",
        "/tmp/sample/\n/tmp/sample/info.ts\ninfo-pass.ts\n",
    )?;
    write(
        top,
        ".gitignore",
        "/tmp/sample/top.ts\n!info-pass.ts\nmiddle-pass.ts\n",
    )?;
    write(
        top,
        "tmp/.gitignore",
        "!/sample/\n/sample/middle.ts\n!middle-pass.ts\nroot-pass.ts\n",
    )?;
    write(
        &root,
        ".gitignore",
        "!root-pass.ts\n!private.ts\ndeep-pass.ts\n",
    )?;
    write(&root, "src/.gitignore", "!deep-pass.ts\n")?;
    write(&root, ".loom/ignore", "private.ts\n!top.ts\n")?;
    for path in denied {
        assert!(
            s.note_get(path)?.starts_with("状态：未覆盖或不可校验\n"),
            "{path}"
        );
    }
    s.index_structure()?;
    for path in denied {
        check_file(&s, path, "absent")?;
    }
    for path in allowed {
        check_file(&s, path, "indexed")?;
    }
    Ok(())
}

#[test]
fn ignored_project_discards_outer_rules_and_uses_its_own_scope() -> Result<()> {
    let d = tempfile::tempdir()?;
    let top = d.path().join("repo");
    let root = top.join("tmp/sample");
    fs::create_dir_all(&root)?;
    let s = project(&root, &[])?;
    let outer_only = ["info.ts", "top.ts", "middle.ts"];
    for path in outer_only.into_iter().chain(["vendor/x.ts", "local.ts"]) {
        write(&root, path, SOURCE)?;
    }
    // 最近的仓库不管这个项目，也不能再去继承更外一层仓库的规则。
    write(d.path(), ".git/info/exclude", "top.ts\n")?;
    write(&top, ".git/info/exclude", "info.ts\n")?;
    write(&top, "tmp/.gitignore", "middle.ts\n")?;
    for pattern in ["/tmp/", "tmp/**", "*"] {
        write(&top, ".gitignore", &format!("{pattern}\ntop.ts\n"))?;
        for (scope, vendor, local) in [
            ("builtin", "absent", "indexed"),
            ("git", "indexed", "absent"),
        ] {
            if scope == "git" {
                write(&root, ".gitignore", "local.ts\n")?;
            }
            assert_eq!(s.index_structure()?.scope, scope, "{pattern}");
            for path in outer_only {
                check_file(&s, path, "indexed")?;
            }
            check_file(&s, "vendor/x.ts", vendor)?;
            check_file(&s, "local.ts", local)?;
        }
        fs::remove_file(root.join(".gitignore"))?;
    }
    Ok(())
}

#[test]
fn linked_worktree_reads_common_excludes_and_tolerates_broken_pointers() -> Result<()> {
    let d = tempfile::tempdir()?;
    let r = d.path().join("work");
    fs::create_dir(&r)?;
    let s = project(
        &r,
        &[
            (".git", "gitdir: ../admin/worktrees/w\r\n"),
            ("credentials/x.ts", SOURCE),
            ("info.ts", SOURCE),
        ],
    )?;
    write(d.path(), "admin/worktrees/w/commondir", "../..\n")?;
    write(d.path(), "admin/worktrees/w/info/exclude", "credentials/\n")?;
    write(d.path(), "admin/info/exclude", "\u{feff}/info.ts\n")?;
    assert_eq!(serde_json::to_value(s.index_structure()?)?["scope"], "git");
    check_file(&s, "credentials/x.ts", "indexed")?;
    check_file(&s, "info.ts", "absent")?;
    fs::write(d.path().join("admin/info/exclude"), [0xff])?;
    assert!(s.index_structure().is_err(), "规则编码错误不能当作空规则");
    for (file, invalid) in [
        ("admin/worktrees/w/commondir", "missing"),
        ("work/.git", "gitdir: ../gone"),
        ("work/.git", "not a gitdir"),
    ] {
        write(d.path(), file, invalid)?;
        assert_eq!(serde_json::to_value(s.index_structure()?)?["scope"], "git");
        check_file(&s, "info.ts", "indexed")?;
        check_file(&s, "credentials/x.ts", "indexed")?;
    }
    Ok(())
}

#[test]
fn generated_filenames_are_managed_but_cannot_prove_imports() -> Result<()> {
    let d = tempfile::tempdir()?;
    let r = d.path();
    let names = ["packed.min.js", "types.generated.ts", "bundle.generated.js"];
    let s = project(r, &[(".gitignore", "")])?;
    for name in names {
        write(r, name, SOURCE)?;
    }
    fs::write(r.join(names[0]), vec![b' '; 1_048_577])?;
    write(
        r,
        "entry.ts",
        &names.map(|n| format!("import './{n}';\n")).concat(),
    )?;
    let report = s.index_structure()?;
    assert_eq!((report.parsed, report.uncovered), (1, 3));
    for name in names {
        check_file(&s, name, "generated")?;
    }
    let entry = s.query_structure("entry.ts", Direction::Outgoing, 1)?;
    assert_eq!(entry.issues.len(), names.len());
    for (issue, name) in entry.issues.iter().zip(names) {
        assert_eq!(issue.state, "uncovered_target");
        assert_eq!(issue.candidates, [name]);
    }
    Ok(())
}

#[test]
fn sensitive_paths_symbols_and_reference_keys_never_enter_the_database() -> Result<()> {
    // 系统临时目录不在任何 Git 工作区里；同一批证据先按内置名单，再按 Git 规则索引。
    let d = tempfile::tempdir()?;
    let r = d.path();
    let s = project(r, &[])?;
    let key = "AKIAIOSFODNN7EXAMPLE";
    let path = format!("{key}.ts");
    write(r, &path, SOURCE)?;
    write(
        r,
        "src/Names.java",
        &format!("package p;\npublic class {key} {{}}\n"),
    )?;
    write(
        r,
        "entry.ts",
        &format!("import './{key}';\nimport 'alias';\n"),
    )?;
    write(
        r,
        "tsconfig.json",
        &serde_json::json!({"compilerOptions": {"paths": {"alias": [format!("./{key}")]}}})
            .to_string(),
    )?;
    write(
        r,
        "lib.rs",
        &format!("pub mod {key};\npub fn {key}() {{}}\nuse crate::other::{key};\n"),
    )?;
    for scope in ["builtin", "git"] {
        if scope == "git" {
            fs::create_dir(r.join(".git"))?;
            // 旧修订的快照即使源码指纹没变，也要重建符号证据。
            s.conn.execute_batch("UPDATE structure_snapshot SET revision=revision-1; UPDATE symbols SET name='legacy-name';")?;
        }
        let report = s.index_structure()?;
        // 路径像凭据的那一个文件不纳入，但要让人知道少了一个。
        assert_eq!((report.parsed, report.withheld), (3, 1));
        assert!(
            report
                .to_string()
                .ends_with("另有 1 个路径的名字像凭据，没有纳入。")
        );
        assert_eq!(serde_json::to_value(report)?["scope"], scope);
        assert_eq!(
            s.query_structure(&path, Direction::Outgoing, 1)?.status,
            "absent"
        );
        assert!(s.note_get(&path).is_err());
        let symbols = s
            .query_structure("src/Names.java", Direction::Outgoing, 1)?
            .symbols;
        assert_eq!(symbols[0].name, "AKIA***");
        assert_eq!(
            s.conn
                .query_row("SELECT count(*) FROM structure_members", [], |r| r
                    .get::<_, i64>(0))?,
            0
        );
        for file in ["entry.ts", "lib.rs"] {
            let out = s.query_structure(file, Direction::Outgoing, 1)?;
            assert!(
                out.issues.iter().any(|i| i.spelling.contains("AKIA***")),
                "{file}"
            );
            if file == "entry.ts" {
                assert!(out.issues.iter().any(|i| i.spelling == "alias"));
            }
            for issue in out.issues {
                assert_eq!(issue.state, "unsupported");
            }
        }
        for table in [
            "structure_candidates",
            "structure_rust_paths",
            "structure_rust_modules",
            "structure_rust_items",
        ] {
            assert_eq!(
                s.conn
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))?,
                0,
                "{table}"
            );
        }
    }
    assert_eq!(s.index_structure()?.unchanged, 3);
    Ok(())
}

/// 索引自己拼的键里有冒号：`sym:GetToken:go:…`、`crate::compass::Direction` 都不是“token: 值”。
/// 名字以 Token、Password、pass 结尾的声明照常连得上。
#[test]
fn names_ending_in_credential_words_are_not_mistaken_for_credentials() -> Result<()> {
    let d = tempfile::tempdir()?;
    let r = d.path();
    let s = project(
        r,
        &[
            ("go.mod", "module example.com/app\n"),
            (
                "cmd/main.go",
                "package main\n\nimport \"example.com/app/token\"\n\nfunc main() { token.GetToken() }\n",
            ),
            ("token/token.go", "package token\n\nfunc GetToken() {}\n"),
            ("token/more.go", "package token\n"),
            (
                "src/p/A.java",
                "package p;\nimport q.AccessToken;\nclass A { AccessToken t; Password p; }\n",
            ),
            (
                "src/q/AccessToken.java",
                "package q;\npublic class AccessToken {}\n",
            ),
            ("src/p/Password.java", "package p;\nclass Password {}\n"),
            (
                "src/lib.rs",
                "mod compass;\nuse crate::compass::Direction;\n",
            ),
            ("src/compass.rs", "pub struct Direction;\n"),
        ],
    )?;
    let report = s.index_structure()?;
    assert_eq!(report.withheld, 0);
    assert_eq!(
        edges(&s, "cmd/main.go")?,
        [edge("token/token.go", "exact", 5)]
    );
    assert_eq!(
        edges(&s, "src/p/A.java")?,
        [
            edge("src/p/Password.java", "likely", 3),
            edge("src/q/AccessToken.java", "exact", 2)
        ]
    );
    let kept: i64 = s.conn.query_row(
        "SELECT count(*) FROM structure_rust_paths WHERE path='crate::compass::Direction'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(kept, 1);
    for file in ["cmd/main.go", "src/p/A.java", "src/lib.rs"] {
        let out = s.query_structure(file, Direction::Outgoing, 1)?;
        assert!(
            out.issues.iter().all(|i| !i.spelling.contains("***")),
            "{file}"
        );
    }
    Ok(())
}
