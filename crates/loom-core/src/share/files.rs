//! 只处理点名的本地普通文件。先检查名字，再逐级打开父目录；不遍历目录、不创建目录。
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};

use super::wire::MAX_BYTES;

#[cfg(windows)]
#[path = "files/windows.rs"]
mod platform;
#[cfg(not(windows))]
#[path = "files/posix.rs"]
mod platform;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    Snapshot,
    Database,
    Create,
}

fn absolute(file: &Path, snapshot: bool) -> Result<PathBuf> {
    ensure!(file.file_name().is_some(), "须指定一个普通文件名");
    // 不在检查前 canonicalize：它会跟随链接（UNC 甚至可能访问网络），也会抹去穿过链接的证据。
    ensure!(
        !file.components().any(|c| c == Component::ParentDir),
        "分享路径不能含 ..；请使用明确的本地路径"
    );
    platform::check_location(file)?;
    let path = if file.is_absolute() {
        file.into()
    } else {
        std::env::current_dir()?.join(file)
    };
    platform::check_location(&path)?;
    for c in path.components() {
        if let Component::Normal(name) = c {
            platform::check_name(name)?;
            if snapshot {
                ensure!(
                    !name.eq_ignore_ascii_case(".loom") && !name.eq_ignore_ascii_case(".git"),
                    "不在项目元数据目录中读写分享文件"
                );
            }
        }
    }
    Ok(path)
}

fn share_path(file: &Path, database: &Path) -> Result<PathBuf> {
    let path = absolute(file, true)?;
    let db = absolute(database, false)?;
    // 自定义外置库也受保护，包括尚未存在的 SQLite 旁文件。
    // 双重比较还包括目录别名（例如 Windows 的 8.3 名字）。不能先 canonicalize
    // 未检查的整条路径，因为这会跟随重解析点；先固定父目录，只解析已检查的目录。
    let dir = path.parent().context("缺少父目录")?;
    let _guard = platform::Parent::open(dir)?;
    let canonical = dir.canonicalize()?;
    for c in canonical.components() {
        if let Component::Normal(name) = c {
            ensure!(
                !name.eq_ignore_ascii_case(".loom") && !name.eq_ignore_ascii_case(".git"),
                "不在项目元数据目录中读写分享文件"
            );
        }
    }
    let path = canonical.join(path.file_name().context("缺少文件名")?);
    let key = platform::path_key(&path);
    let db_key = platform::path_key(&db);
    let canonical_db = platform::path_key(&db.canonicalize()?);
    for protected in [&db_key, &canonical_db] {
        ensure!(
            &key != protected
                && !["-wal", "-shm", "-journal"]
                    .iter()
                    .any(|suffix| key.strip_suffix(suffix) == Some(protected.as_str())),
            "不能将项目库或其日志用于分享"
        );
    }
    Ok(path)
}

fn open(path: &Path, mode: Mode) -> Result<(platform::Parent, File)> {
    let parent = platform::Parent::open(path.parent().context("缺少父目录")?)?;
    let file = parent.file(path.file_name().context("缺少文件名")?, mode)?;
    platform::regular(&file)?;
    Ok((parent, file))
}

/// 保留目录及本地库句柄，直到身份核对与 SQLite 操作结束。没有建库、认领或升级入口。
pub(super) fn local_database(database: &Path) -> Result<impl Sized> {
    open(&absolute(database, false)?, Mode::Database)
        .context("分享须使用已绑定的本地普通库文件，不跟随链接")
}

/// 不复制业务库。Windows 阻止写入/替换；Unix 在结果交付前核对原 inode 和纳秒级变更时间。
pub(super) fn frozen<T>(path: &Path, read: impl FnOnce(&Path, &File) -> Result<T>) -> Result<T> {
    let path = absolute(path, false)?;
    let (_guard, file) =
        open(&path, Mode::Snapshot).context("不能取得稳定的本地只读文件；请结束并发写入后重试")?;
    #[cfg(unix)]
    let before = file.metadata()?;
    let result = read(&path, &file)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let (_, current) = open(&path, Mode::Snapshot)?;
        let stamp = |m: std::fs::Metadata| (m.dev(), m.ino(), m.len(), m.ctime(), m.ctime_nsec());
        ensure!(
            stamp(before) == stamp(current.metadata()?),
            "读取期间文件已变化；请重新预览"
        );
    }
    Ok(result)
}

pub(super) fn read(file: &Path, db: &Path) -> Result<Vec<u8>> {
    let path = share_path(file, db)?;
    let (_parent, input) = open(&path, Mode::Snapshot)?;
    let len = input.metadata()?.len();
    ensure!(len <= MAX_BYTES as u64, "分享文件超过 {MAX_BYTES} 字节");
    let mut bytes = Vec::with_capacity(len as usize);
    input.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_BYTES, "读取中的分享文件超过上限");
    Ok(bytes)
}

pub(super) fn create(file: &Path, db: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() <= MAX_BYTES, "分享文件超限");
    let path = share_path(file, db)?;
    let (parent, mut output) =
        open(&path, Mode::Create).context("只允许新建本地普通文件，不覆盖已有条目或链接")?;
    if let Err(error) = output.write_all(bytes).and_then(|_| output.sync_all()) {
        parent
            .discard(path.file_name().expect("已检查文件名"), &output)
            .context("写入失败且无法清理未完成文件")?;
        return Err(error).context("分享文件未写成");
    }
    Ok(())
}
