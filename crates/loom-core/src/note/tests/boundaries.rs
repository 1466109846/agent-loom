use super::*;

fn opaque(s: &Store, key: &str, expected: &str) -> Result<()> {
    let actual = s.note_get(key)?;
    assert_eq!(
        field(&actual, "状态"),
        "未覆盖或不可校验",
        "{key}: {actual}"
    );
    assert_eq!(
        field(&actual, "当前指纹"),
        "不可校验",
        "不能读取 {key} 再隐去指纹"
    );
    refuse_both(s, key, expected);
    Ok(())
}

#[test]
fn changed_ignore_rules_are_applied_before_source_even_without_refresh() -> Result<()> {
    let d = tempfile::tempdir()?;
    let root = d.path();
    let s = fixture(root)?;
    let a = sha256_hex(FIRST.as_bytes());
    s.note_set(FILE, &a, ROLE)?;
    put(root, ".gitignore", "*.ts\n")?;
    opaque(&s, FILE, &a)?;
    assert_eq!(field(&s.note_get(FILE)?, "说明"), "当前忽略规则排除此路径");
    put(root, "src/.gitignore", "!writer.ts\n")?;
    check(&s, "当前", &a, &a)?;
    put(root, ".loom/ignore", "\u{feff}src/writer.ts\n")?;
    opaque(&s, FILE, &a)?; // .loom/ignore 优先于最深 git 放行。
    s.index_structure()?;
    opaque(&s, FILE, &a)?;
    put(root, ".loom/ignore", b"")?;
    opaque(&s, FILE, &a)?; // 放行不等于重新解析。
    s.index_structure()?;
    check(&s, "当前", &a, &a)?;
    put(root, ".gitignore", "src/\n")?;
    opaque(&s, FILE, &a)?; // 忽略父目录后不得再读取其子规则。
    put(root, ".gitignore", b"")?;
    put(root, "src/.gitignore", [0xff, 0xff])?;
    opaque(&s, FILE, &a)?;
    put(root, "src/.gitignore", vec![b' '; 262_145])?;
    opaque(&s, FILE, &a)?;
    put(root, "src/.gitignore", b"")?;
    for marker in ["pyvenv.cfg", "CACHEDIR.TAG"] {
        let marker = root.join("src").join(marker);
        fs::write(&marker, b"")?;
        opaque(&s, FILE, &a)?; // 快照仍授权，现场标记必须立即撤销读取资格。
        assert_eq!(field(&s.note_get(FILE)?, "说明"), "虚拟环境或缓存目录");
        fs::remove_file(marker)?;
        check(&s, "当前", &a, &a)?;
    }
    fs::create_dir(root.join("src/.loom"))?; // 即便还没有 project.id，scan 也不下探。
    opaque(&s, FILE, &a)?;
    assert_eq!(field(&s.note_get(FILE)?, "说明"), "路径已属于嵌套项目");
    s.index_structure()?;
    opaque(&s, FILE, &a)?;
    fs::remove_dir(root.join("src/.loom"))?;
    s.index_structure()?;
    check(&s, "当前", &a, &a)?;
    Ok(())
}

#[test]
fn unparsed_large_generated_non_utf8_and_non_source_are_not_evidence() -> Result<()> {
    let d = tempfile::tempdir()?;
    let s = fixture(d.path())?;
    let a = sha256_hex(FIRST.as_bytes());
    s.note_set(FILE, &a, ROLE)?;
    for bytes in [
        vec![b' '; 1_048_577],
        vec![0xff],
        b"// @generated\nexport const v = 1;\n".to_vec(),
    ] {
        put(d.path(), FILE, &bytes)?;
        opaque(&s, FILE, &sha256_hex(&bytes))?;
        assert_eq!(s.index_structure()?.uncovered, 1);
        opaque(&s, FILE, &sha256_hex(&bytes))?;
        put(d.path(), FILE, FIRST)?;
        s.index_structure()?;
        check(&s, "当前", &a, &a)?;
    }
    let invalid = "export function broken(\n";
    put(d.path(), FILE, invalid)?;
    assert_eq!(s.index_structure()?.uncovered, 1);
    opaque(&s, FILE, &sha256_hex(invalid.as_bytes()))?;
    // 系统临时目录不在任何 Git 工作区里，因此这里仍按内置名单排除。
    for denied in [
        "new.ts",
        "docs.md",
        "vendor/v.ts",
        ".hidden/v.ts",
        "credentials.ts",
        "packed.min.js",
        "data/p.py",
    ] {
        put(d.path(), denied, FIRST)?;
        opaque(&s, denied, &a)?;
    }
    s.index_structure()?;
    for denied in [
        "docs.md",
        "vendor/v.ts",
        ".hidden/v.ts",
        "credentials.ts",
        "packed.min.js",
        "data/p.py",
    ] {
        opaque(&s, denied, &a)?;
    }
    Ok(())
}

#[test]
fn traversal_and_rooted_keys_are_rejected_before_lookup() -> Result<()> {
    let d = tempfile::tempdir()?;
    let s = fixture(d.path())?;
    let a = sha256_hex(FIRST.as_bytes());
    for bad in [
        "",
        "..",
        "src/../../other.ts",
        "/other/a.ts",
        "C:/a.ts",
        "C:a.ts",
        "\\\\server\\share\\a.ts",
        "src/writer.ts:extra",
        "a\0b.ts",
        "a\nb.ts",
    ] {
        assert!(s.note_get(bad).is_err(), "{bad:?}");
        refuse_both(&s, bad, &a);
    }
    Ok(())
}

#[test]
fn same_named_roots_unbound_stores_and_long_lived_handles_do_not_cross() -> Result<()> {
    let d = tempfile::tempdir()?;
    let a_root = d.path().join("first/same");
    let b_root = d.path().join("second/same");
    fs::create_dir_all(&a_root)?;
    let external = d.path().join("independent.db");
    let loc = ProjectLocation::select(&a_root, Some(&a_root), Some(&external))?;
    loc.bind(Bind::Initialize)?;
    let a = loc.open(true)?;
    put(&a_root, FILE, FIRST)?;
    a.index_structure()?;
    let hash = sha256_hex(FIRST.as_bytes());
    a.note_set(FILE, &hash, ROLE)?;
    let b = fixture(&b_root)?;
    b.note_set(FILE, &hash, "另一个项目的唯一职责")?;
    assert!(!b.note_get(FILE)?.contains(ROLE));
    assert!(!a.note_get(FILE)?.contains("另一个项目"));
    assert!(
        ProjectLocation::select(&b_root, Some(&b_root), Some(&external))?
            .open(false)
            .is_err()
    );
    let detached = Store::open_in_memory()?;
    assert!(detached.note_get(FILE).is_err());
    refuse_both(&detached, FILE, &hash);
    fs::remove_file(a_root.join(".loom/project.id"))?;
    assert!(a.note_get(FILE).is_err());
    refuse_both(&a, FILE, &hash);
    fs::rename(&a_root, d.path().join("moved"))?;
    assert!(a.note_get(FILE).is_err());
    refuse_both(&a, FILE, &hash);
    fs::create_dir_all(&a_root)?;
    put(
        &a_root,
        ".loom/project.id",
        fs::read(b_root.join(".loom/project.id"))?,
    )?;
    put(&a_root, FILE, FIRST)?;
    assert!(a.note_get(FILE).is_err());
    refuse_both(&a, FILE, &hash);
    Ok(())
}

#[test]
fn live_db_binding_not_just_cached_marker_must_still_match() -> Result<()> {
    let d = tempfile::tempdir()?;
    let s = fixture(d.path())?;
    let a = sha256_hex(FIRST.as_bytes());
    s.note_set(FILE, &a, ROLE)?;
    s.conn.execute(
        "UPDATE loom_identity SET canonical_root=?1",
        [d.path().join("new-location").to_str().unwrap()],
    )?;
    assert!(s.note_get(FILE).is_err());
    refuse_both(&s, FILE, &a);
    Ok(())
}

#[test]
fn outdated_snapshot_cannot_authorize_reading_current_bytes() -> Result<()> {
    let d = tempfile::tempdir()?;
    let s = fixture(d.path())?;
    let a = sha256_hex(FIRST.as_bytes());
    s.note_set(FILE, &a, ROLE)?;
    s.conn
        .execute_batch("UPDATE structure_snapshot SET revision=revision+1")?;
    opaque(&s, FILE, &a)?;
    Ok(())
}

#[cfg(windows)]
#[test]
fn exclusive_file_lock_is_unverifiable_not_missing() -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    let d = tempfile::tempdir()?;
    let s = fixture(d.path())?;
    let a = sha256_hex(FIRST.as_bytes());
    s.note_set(FILE, &a, ROLE)?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(d.path().join(FILE))?;
    opaque(&s, FILE, &a)?;
    assert_eq!(s.index_structure()?.uncovered, 1);
    drop(lock);
    opaque(&s, FILE, &a)?;
    s.index_structure()?;
    check(&s, "当前", &a, &a)?;
    Ok(())
}

#[test]
fn multiply_linked_source_and_rules_are_not_read() -> Result<()> {
    let d = tempfile::tempdir()?;
    let root = d.path().join("root");
    let s = fixture(&root)?;
    let a = sha256_hex(FIRST.as_bytes());
    s.note_set(FILE, &a, ROLE)?;
    put(d.path(), "outside.ts", FIRST)?;
    fs::remove_file(root.join(FILE))?;
    fs::hard_link(d.path().join("outside.ts"), root.join(FILE))?;
    opaque(&s, FILE, &a)?;
    fs::remove_file(root.join(FILE))?;
    put(&root, FILE, FIRST)?;
    put(d.path(), "outside-rules", b"")?;
    fs::hard_link(d.path().join("outside-rules"), root.join(".gitignore"))?;
    opaque(&s, FILE, &a)?;
    Ok(())
}

#[cfg(any(windows, unix))]
#[test]
#[cfg_attr(
    windows,
    ignore = "文件符号链接需 Windows 开发者模式或相应权限；可用 --ignored 显式运行"
)]
fn replaced_leaf_and_ignore_file_symlinks_are_never_followed() -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_file as symlink;
    let d = tempfile::tempdir()?;
    let root = d.path().join("root");
    let s = fixture(&root)?;
    let hash = sha256_hex(FIRST.as_bytes());
    s.note_set(FILE, &hash, ROLE)?;
    put(d.path(), "foreign.ts", SECOND)?;
    fs::remove_file(root.join(FILE))?;
    symlink(d.path().join("foreign.ts"), root.join(FILE))?;
    opaque(&s, FILE, &sha256_hex(SECOND.as_bytes()))?;
    fs::remove_file(root.join(FILE))?;
    put(&root, FILE, FIRST)?;
    put(d.path(), "outside-rules", b"")?;
    symlink(d.path().join("outside-rules"), root.join(".gitignore"))?;
    opaque(&s, FILE, &hash)?;
    fs::remove_file(root.join(".gitignore"))?;
    check(&s, "当前", &hash, &hash)?;
    Ok(())
}

#[cfg(any(windows, unix))]
#[test]
fn directory_redirection_does_not_authorize_identical_foreign_bytes() -> Result<()> {
    let d = tempfile::tempdir()?;
    let root = d.path().join("root");
    let s = fixture(&root)?;
    let hash = sha256_hex(FIRST.as_bytes());
    s.note_set(FILE, &hash, ROLE)?;
    let foreign = d.path().join("foreign");
    put(&foreign, "writer.ts", FIRST)?;
    fs::rename(root.join("src"), root.join("before"))?;
    #[cfg(windows)]
    {
        // 目录联接不需要符号链接特权，且同样是必须拒绝的 reparse point。
        let out = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(root.join("src"))
            .arg(&foreign)
            .output()?;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&foreign, root.join("src"))?;
    opaque(&s, FILE, &hash)?;
    Ok(())
}
