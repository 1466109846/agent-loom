//! 指针是唯一的切换点。切换前出错，原库仍是完整的可用库。
use super::*;
use rusqlite::backup::{Backup, StepResult};

const IN_USE: &str = "还有会话在使用这个库，请先结束它们";
const SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"];

pub struct MoveReport {
    pub target: PathBuf,
    pub bytes: u64,
    pub retained: Vec<PathBuf>,
}

fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut s = db.as_os_str().to_owned();
    s.push(suffix);
    s.into()
}

fn same_slot(a: &Path, b: &Path) -> bool {
    let (Some(ap), Some(bp), Some(an), Some(bn)) =
        (a.parent(), b.parent(), a.file_name(), b.file_name())
    else {
        return false;
    };
    (an == bn || (cfg!(windows) && an.eq_ignore_ascii_case(bn)))
        && (ap == bp
            || ap
                .canonicalize()
                .is_ok_and(|a| bp.canonicalize().is_ok_and(|b| a == b)))
}

impl ProjectLocation {
    pub(super) fn check_reserved(&self, db: &Path) -> Result<()> {
        for name in [MARKER, POINTER, ".loom/bind.lock"] {
            ensure!(
                !same_slot(db, &self.root.join(name)),
                "项目元数据不能用作库位置"
            );
        }
        Ok(())
    }

    fn check_target(&self, target: &Path) -> Result<()> {
        pointer_path(target)?;
        self.check_reserved(target)?;
        ensure!(!entry_exists(target)?, "目标已存在或与原库相同");
        for suffix in SUFFIXES {
            ensure!(
                !same_slot(&sidecar(&self.db, suffix), target),
                "目标与原库日志重叠"
            );
            ensure!(
                !entry_exists(&sidecar(target, suffix))?,
                "目标位置已有 SQLite 旁文件，未覆盖"
            );
        }
        Ok(())
    }

    pub fn move_database(&self, target: &Path, yes: bool) -> Result<MoveReport> {
        let identity = self.probe_bound()?;
        let name = target.to_str().context("目标路径须为 UTF-8")?;
        ensure!(!name.is_empty(), "须指定目标文件或目录");
        let directory = name.ends_with(['/', '\\']);
        let mut target = tidy(std::env::current_dir()?.join(target));
        #[cfg(not(windows))]
        if name.ends_with('\\') {
            target = std::env::current_dir()?.join(name.trim_end_matches('\\'));
        }
        // 只有写明了库文件的扩展名才当作文件：`D:\LoomData` 还不存在时，用户指的也是目录。
        let file = target
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
                ["db", "sqlite", "sqlite3"].contains(&e.to_ascii_lowercase().as_str())
            });
        if directory || target.is_dir() || !file {
            target = target.join(identity.id()).join("loom.db");
        }
        self.check_target(&target)?;
        let mut report = MoveReport {
            target,
            bytes: fs::metadata(&self.db)?.len(),
            retained: Vec::new(),
        };
        if !yes {
            return Ok(report);
        }

        let _guard = self.binding_lock()?;
        if !self.explicit {
            ensure!(
                Self::select(&self.root, Some(&self.root), None)?.db == self.db,
                "库位置已改变，请重新预演"
            );
        }
        let store = self.open(true)?;
        ensure!(store.project() == Some(&identity), MISMATCH);
        let src = &store.conn;
        src.busy_timeout(Duration::ZERO)?;
        let migrate = || -> Result<u64> {
            let mode: String = src
                .query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))
                .context(IN_USE)?;
            ensure!(mode == "delete", IN_USE);
            // COMMIT 结束写事务以便备份；EXCLUSIVE 模式仍保留文件锁，直到关闭原连接。
            src.execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE; COMMIT;")
                .context(IN_USE)?;
            let dir = report.target.parent().context("目标缺少父目录")?;
            fs::create_dir_all(dir)?;
            self.check_target(&report.target)?;
            let tmp = tempfile::NamedTempFile::new_in(dir)?.into_temp_path();
            {
                let mut dst = connect(&tmp, true)?;
                ensure!(
                    Backup::new(src, &mut dst)?
                        .step(-1)
                        .context("复制 SQLite 页失败")?
                        == StepResult::Done,
                    "未能完成独占备份"
                );
                verify_copy(src, &dst, &identity).context("核对副本失败")?;
            }
            // FTS 的计数游标可在校验连接上保留读锁；关闭它，再在发布前完成检查点。
            checkpoint_wal(&connect(&tmp, true)?).context("副本检查点失败")?;
            let file = OpenOptions::new().write(true).open(&tmp)?;
            file.sync_all()?;
            let bytes = file.metadata()?.len();
            drop(file);
            self.check_target(&report.target)?;
            tmp.persist_noclobber(&report.target)
                .context("未发布新库；不会覆盖已有文件")?;
            if let Err(e) = self.update_pointer(&report.target) {
                fs::remove_file(&report.target).context("指针未切换，未能移除已校验的副本")?;
                return Err(e);
            }
            Ok(bytes)
        };
        match migrate() {
            Ok(bytes) => report.bytes = bytes,
            Err(e) => {
                drop(store);
                if let Ok(conn) = connect(&self.db, true) {
                    let _ = checkpoint_wal(&conn);
                }
                return Err(e);
            }
        }
        drop(store);
        for path in std::iter::once(self.db.clone()).chain(SUFFIXES.map(|s| sidecar(&self.db, s))) {
            if let Err(e) = fs::remove_file(&path)
                && e.kind() != ErrorKind::NotFound
            {
                report.retained.push(path);
            }
        }
        Ok(report)
    }
}

fn verify_copy(src: &Connection, dst: &Connection, id: &ProjectIdentity) -> Result<()> {
    let status: String = dst.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    ensure!(status == "ok", "副本未通过完整性检查");
    ensure!(
        matches!(inspect(dst)?, DiskState::Bound(ref found) if found == id),
        MISMATCH
    );
    // 动态枚举也包含按需创建的功能表，不让搬库依赖一份手抄的表名清单。
    let counts = |conn: &Connection| -> Result<Vec<(String, i64)>> {
        let mut tables =
            conn.prepare("SELECT name FROM main.sqlite_schema WHERE type='table' ORDER BY name")?;
        let mut counts = Vec::new();
        for name in tables.query_map([], |r| r.get::<_, String>(0))? {
            let name = name?;
            let quoted = name.replace('"', "\"\"");
            let count = conn.query_row(
                &format!("SELECT count(*) FROM main.\"{quoted}\""),
                [],
                |r| r.get(0),
            )?;
            counts.push((name, count));
        }
        Ok(counts)
    };
    ensure!(counts(src)? == counts(dst)?, "副本与原库的表或行数不一致");
    Ok(())
}
