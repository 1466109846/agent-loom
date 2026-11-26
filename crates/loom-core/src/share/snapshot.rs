//! SQLite 的 READ_ONLY 在 WAL 模式仍可能创建 -wal/-shm。普通预览使用不写旁文件的
//! immutable 视图，但绝不忽略未合并的 WAL/回滚日志。Windows 读句柄在整个快照期间排除写者。
use std::fmt::Write;
use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, params};

use super::{ShareProject, files, valid_id};
use crate::db::{APPLICATION_ID, SCHEMA_VERSION};

fn quiet(db: &Path) -> Result<()> {
    for suffix in ["-wal", "-journal"] {
        let mut name = db.as_os_str().to_owned();
        name.push(suffix);
        match fs::symlink_metadata(PathBuf::from(name)) {
            Ok(m) => ensure!(
                m.is_file() && m.len() == 0,
                "库尚有日志，不能保证无副作用地读取当前内容；请结束当前写入后重试"
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn uri(db: &Path) -> Result<String> {
    let db = db.to_str().context("无法无损表示库路径")?;
    #[cfg(windows)]
    let db = db.strip_prefix(r"\\?\").unwrap_or(db).replace('\\', "/");
    let mut uri = String::from("file:");
    for b in db.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.:".contains(&b) {
            uri.push(b as char);
        } else {
            write!(uri, "%{b:02X}")?;
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    Ok(uri)
}

pub(super) fn read<T>(
    project: &ShareProject,
    f: impl FnOnce(&Connection, &str) -> Result<T>,
) -> Result<T> {
    files::frozen(&project.root.join(".loom/project.id"), |_, marker| {
        let mut id = String::new();
        ensure!(marker.metadata()?.len() <= 64, "无效项目标记");
        marker.take(65).read_to_string(&mut id)?;
        let id = id.trim();
        ensure!(valid_id(id), "项目标记缺失或无效");
        files::frozen(project.database(), |path, _| {
            quiet(path)?;
            let conn = Connection::open_with_flags(
                uri(path)?,
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            let app: i32 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
            let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
            ensure!(
                app == APPLICATION_ID && version == SCHEMA_VERSION,
                "不支持未绑定或缺字段的遗留库；不会升级"
            );
            let bound:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM main.loom_identity WHERE singleton=1 AND project_id=?1 AND canonical_root=?2)",params![id,project.root.to_str().context("项目根编码无效")?],|r|r.get(0))?;
            ensure!(bound, "项目身份不符；未加载项目内容");
            let result = f(&conn, id)?;
            quiet(path)?;
            Ok(result)
        })
    })
}
