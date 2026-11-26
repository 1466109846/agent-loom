use super::*;
use crate::memory::{MemoryType, NewMemory, RememberOutcome};
use crate::search::RecallQuery;

fn locations(dir: &Path) -> ProjectLocation {
    ProjectLocation::select(dir, Some(dir), None).unwrap()
}

#[test]
fn pointer_must_be_a_small_absolute_utf8_line_in_the_real_metadata_directory() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    locations(root).bind(Bind::Initialize)?;
    let path = root.join(POINTER);
    for invalid in [
        vec![b'x'; 4097],
        b"relative.db".to_vec(),
        format!("{}\nmore", root.display()).into_bytes(),
        format!("{}\tbad", root.display()).into_bytes(),
        vec![0xff],
    ] {
        fs::write(&path, invalid)?;
        assert!(ProjectLocation::select(root, None, None).is_err());
    }
    fs::remove_file(&path)?;
    let outside = root.join("outside");
    fs::write(&outside, root.join(DEFAULT_DB).to_str().unwrap())?;
    #[cfg(unix)]
    use std::os::unix::fs::symlink as link;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_file as link;
    #[cfg(unix)]
    link(&outside, &path)?;
    #[cfg(windows)]
    if let Err(e) = link(&outside, &path) {
        if e.raw_os_error() != Some(1314) {
            return Err(e.into());
        }
        // 没有文件符号链接特权时仍实际建立重解析点，不跳过链接测试。
        let target = root.join("elsewhere");
        fs::create_dir(&target)?;
        let out = std::process::Command::new("cmd")
            .current_dir(root.join(".loom"))
            .args(["/D", "/C", "mklink", "/J", "database.path"])
            .arg(&target)
            .output()?;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(ProjectLocation::select(root, None, None).is_err());
    Ok(())
}

#[test]
fn move_rejects_open_connection_and_existing_target_without_switching() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let project = locations(dir.path());
    project.bind(Bind::Initialize)?;
    let target = dir.path().join("new.db");
    let session = project.open(true)?;
    let err = project
        .move_database(&target, true)
        .err()
        .context("不得移动打开的库")?;
    assert!(err.to_string().contains("还有会话"));
    assert!(!target.exists() && !dir.path().join(POINTER).exists());
    assert_eq!(session.count()?, 0);
    drop(session);
    let original = fs::read(project.database())?;
    fs::write(&target, b"occupied")?;
    for yes in [false, true] {
        assert!(project.move_database(&target, yes).is_err());
        assert!(project.move_database(project.database(), yes).is_err());
    }
    assert_eq!(fs::read(&target)?, b"occupied");
    assert_eq!(fs::read(project.database())?, original);
    Ok(())
}

#[cfg(windows)]
#[test]
fn move_reports_undeletable_source_and_rolls_back_a_locked_pointer() -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let old = root.join("old.db");
    let project = ProjectLocation::select(root, Some(root), Some(&old))?;
    project.bind(Bind::Initialize)?;
    let target = root.join("new.db");
    let keep = |p: &Path| OpenOptions::new().read(true).share_mode(3).open(p);
    let pointer_lock = keep(&root.join(POINTER))?;
    assert!(project.move_database(&target, true).is_err());
    assert!(!target.exists());
    assert_eq!(locations(root).database(), old);
    assert_eq!(locations(root).open(false)?.count()?, 0);
    drop(pointer_lock);
    let old_lock = keep(&old)?;
    let report = project.move_database(&target, true)?;
    assert_eq!(report.retained, vec![old.clone()]);
    assert_eq!(locations(root).database(), target);
    assert_eq!(locations(root).open(false)?.count()?, 0);
    drop(old_lock);
    Ok(())
}

fn files(dir: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let mut v: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

#[test]
fn legacy_library_is_untouched_until_explicitly_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let project = locations(dir.path());
    let meta = dir.path().join(".loom");
    fs::create_dir(&meta).unwrap();
    let conn = Connection::open(project.database()).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(SCHEMA_V1).unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    let legacy = Store::from_connection(conn, None).unwrap();
    let created = legacy
        .remember(NewMemory {
            mem_type: Some(MemoryType::Decision),
            title: "保留历史修订".into(),
            body: "保留原依据，因为以后需要复核设计变化。".into(),
            tags: vec!["演进".into()],
            ..Default::default()
        })
        .unwrap();
    let RememberOutcome::Created { id, .. } = created else {
        panic!()
    };
    drop(legacy);
    let before = files(&meta);
    assert!(project.open(true).is_err());
    assert!(project.bind(Bind::Initialize).is_err());
    assert_eq!(files(&meta), before, "拒绝时不创建日志文件或项目标记");

    let identity = project.bind(Bind::AdoptLegacy).unwrap();
    let s = project.open(false).unwrap();
    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(s.project().unwrap().id(), identity.id());
    assert_eq!(s.count().unwrap(), 1);
    assert_eq!(s.get(&id).unwrap().unwrap().tags, vec!["演进"]);
    let hits = s
        .recall(&RecallQuery {
            query: "历史修订".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits[0].memory.id, id);
}

#[test]
fn two_initializers_cannot_create_competing_databases_for_one_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let paths = [root.join("one.db"), root.join("two.db")];
    let gate = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let workers: Vec<_> = paths
            .iter()
            .map(|path| {
                let gate = &gate;
                scope.spawn(move || {
                    let target = ProjectLocation::select(root, Some(root), Some(path)).unwrap();
                    gate.wait();
                    target.bind(Bind::Initialize).is_ok()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|ok| **ok).count(), 1);
    for (path, succeeded) in paths.iter().zip(results) {
        assert_eq!(path.exists(), succeeded);
        if succeeded {
            let store = ProjectLocation::select(root, Some(root), Some(path))
                .unwrap()
                .open(false)
                .unwrap();
            assert_eq!(
                store.project().unwrap().root(),
                root.canonicalize().unwrap()
            );
        }
    }
}

#[test]
fn unrelated_database_is_refused_even_with_explicit_adoption() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("自有资料 #100%.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE notes (title); PRAGMA user_version=1;")
        .unwrap();
    drop(conn);
    let project = ProjectLocation::select(dir.path(), Some(dir.path()), Some(&db)).unwrap();
    let before = files(dir.path());
    assert!(project.bind(Bind::AdoptLegacy).is_err());
    assert!(project.open(false).is_err());
    assert_eq!(files(dir.path()), before);
}

#[test]
fn external_database_can_move_without_changing_project_identity() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("示例项目");
    fs::create_dir(&root).unwrap();
    let db = dir.path().join("独立库存 #1%.db");
    let project = ProjectLocation::select(&root, Some(&root), Some(&db)).unwrap();
    let id = project.bind(Bind::Initialize).unwrap();
    let moved_db = dir.path().join("移动后的库.db");
    fs::rename(db, &moved_db).unwrap();
    let selected = ProjectLocation::select(&root, None, Some(&moved_db)).unwrap();
    assert_eq!(
        selected.open(false).unwrap().project().unwrap().id(),
        id.id()
    );
    let missing = locations(&root);
    assert!(missing.bind(Bind::Initialize).is_err());
    assert!(!missing.database().exists());
}

#[test]
fn directory_move_requires_confirmation_and_keeps_id() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("旧址");
    let b = dir.path().join("新址");
    fs::create_dir(&a).unwrap();
    let original = locations(&a).bind(Bind::Initialize).unwrap();
    fs::rename(&a, &b).unwrap();
    let moved = locations(&b);
    let before = files(&b.join(".loom"));
    assert!(moved.open(false).is_err());
    assert!(moved.bind(Bind::Initialize).is_err());
    assert!(moved.bind(Bind::Relocate(&new_id())).is_err());
    assert_eq!(files(&b.join(".loom")), before);
    let bound = moved.bind(Bind::Relocate(original.id())).unwrap();
    assert_eq!(bound.id(), original.id());
    assert_eq!(
        moved.open(false).unwrap().root().unwrap(),
        b.canonicalize().unwrap()
    );
}

#[test]
fn copy_is_not_a_move_and_marker_is_checked_independently_of_path() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    for root in [&a, &b] {
        fs::create_dir_all(root.join(".loom")).unwrap();
    }
    let original = locations(&a).bind(Bind::Initialize).unwrap();
    fs::copy(a.join(DEFAULT_DB), b.join(DEFAULT_DB)).unwrap();
    fs::copy(a.join(MARKER), b.join(MARKER)).unwrap();
    let copy = locations(&b);
    let before = files(&b.join(".loom"));
    assert!(copy.open(false).is_err());
    assert!(copy.bind(Bind::Relocate(original.id())).is_err());
    assert_eq!(files(&b.join(".loom")), before);

    fs::write(a.join(MARKER), new_id()).unwrap();
    let before = files(&a.join(".loom"));
    assert!(locations(&a).open(true).is_err());
    assert!(locations(&a).bind(Bind::Initialize).is_err());
    assert_eq!(files(&a.join(".loom")), before);
}

#[test]
fn missing_marker_does_not_autobind_but_init_on_the_same_root_can_restore_it() {
    let dir = tempfile::tempdir().unwrap();
    let project = locations(dir.path());
    let id = project.bind(Bind::Initialize).unwrap();
    fs::remove_file(dir.path().join(MARKER)).unwrap();
    assert!(project.open(false).is_err());
    assert!(!dir.path().join(MARKER).exists());
    assert_eq!(project.bind(Bind::Initialize).unwrap().id(), id.id());
}
