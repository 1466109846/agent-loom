//! 固定每一级本地目录：不跟随重解析点，持有不共享写/删除的目录句柄，直到叶子操作结束。
use std::ffi::{OsStr, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::{
    fs::{MetadataExt, OpenOptionsExt},
    io::AsRawHandle,
};
use std::path::{Component, Path, PathBuf, Prefix};

use anyhow::{Result, ensure};

use super::Mode;

const READ_ATTR: u32 = 0x80;
const READ_DATA: u32 = 1; // 目录上为 FILE_LIST_DIRECTORY；只获取句柄，不枚举目录。
const SHARE_READ: u32 = 1;
const SHARE_WRITE: u32 = 2;
const OPEN_REPARSE: u32 = 0x0020_0000;
const BACKUP_SEMANTICS: u32 = 0x0200_0000;
const REPARSE: u32 = 0x400;

// 稳定版 std 尚未提供 Windows 硬链接数或“删除此句柄所指文件”的接口。
#[repr(C)]
#[derive(Default)]
struct StandardInfo {
    allocation: i64,
    end: i64,
    links: u32,
    delete_pending: u8,
    directory: u8,
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn GetFileType(file: *mut c_void) -> u32;
    fn GetFileInformationByHandleEx(
        file: *mut c_void,
        class: i32,
        data: *mut c_void,
        size: u32,
    ) -> i32;
    fn SetFileInformationByHandle(
        file: *mut c_void,
        class: i32,
        data: *const c_void,
        size: u32,
    ) -> i32;
}

pub(super) fn check_location(path: &Path) -> Result<()> {
    match path.components().next() {
        Some(Component::Prefix(prefix)) => {
            let (Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)) = prefix.kind() else {
                anyhow::bail!("仅允许本地磁盘路径，不接受 UNC、设备或网络路径");
            };
            ensure!(
                path.is_absolute() && letter.is_ascii_alphabetic(),
                "不接受驱动器相对路径"
            );
            let root = [u16::from(letter), b':' as u16, b'\\' as u16, 0];
            // SAFETY: root 是有效且以 NUL 结尾的盘符根，调用不保留指针。
            ensure!(
                matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, 2 | 3 | 5 | 6),
                "不接受映射网络盘或未知设备"
            );
        }
        Some(Component::RootDir) => anyhow::bail!("请给出完整的本地盘符路径"),
        _ => (),
    }
    Ok(())
}

pub(super) fn check_name(name: &OsStr) -> Result<()> {
    let s = name
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("文件名编码无效"))?;
    ensure!(
        !s.ends_with([' ', '.'])
            && !s.contains([':', '"', '?', '*', '<', '>', '|', '/', '\\'])
            && !s.chars().any(char::is_control),
        "不接受设备名、备用数据流或有歧义的路径"
    );
    let base = s
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let port = base
        .strip_prefix("COM")
        .or_else(|| base.strip_prefix("LPT"));
    ensure!(
        !matches!(
            base.as_str(),
            "NUL" | "CON" | "CONIN$" | "CONOUT$" | "AUX" | "PRN" | "CLOCK$"
        ) && !port.is_some_and(|n| matches!(
            n,
            "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
        )),
        "分享路径不能使用 Windows 设备名"
    );
    Ok(())
}

pub(super) fn path_key(path: &Path) -> String {
    let p = path.to_string_lossy().replace('/', "\\");
    p.strip_prefix(r"\\?\").unwrap_or(&p).to_lowercase()
}

pub(super) struct Parent {
    path: PathBuf,
    _guards: Vec<File>,
}
impl Parent {
    pub(super) fn open(path: &Path) -> Result<Self> {
        let mut guards = Vec::new();
        for dir in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
            let guard = OpenOptions::new()
                .access_mode(READ_ATTR | READ_DATA)
                .share_mode(SHARE_READ)
                .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE)
                .open(dir)?;
            let m = guard.metadata()?;
            ensure!(
                m.is_dir() && m.file_attributes() & REPARSE == 0,
                "分享路径含非目录或重解析点"
            );
            guards.push(guard);
        }
        Ok(Self {
            path: path.into(),
            _guards: guards,
        })
    }
    pub(super) fn file(&self, name: &OsStr, mode: Mode) -> Result<File> {
        let mut o = OpenOptions::new();
        o.custom_flags(OPEN_REPARSE);
        match mode {
            Mode::Create => {
                // GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES；只在失败清理时使用 DELETE。
                o.write(true)
                    .create_new(true)
                    .access_mode(0x4001_0000 | READ_ATTR)
                    .share_mode(0);
            }
            Mode::Snapshot => {
                o.read(true).share_mode(SHARE_READ);
            }
            Mode::Database => {
                o.read(true).share_mode(SHARE_READ | SHARE_WRITE);
            }
        }
        Ok(o.open(self.path.join(name))?)
    }
    pub(super) fn discard(&self, _: &OsStr, file: &File) -> Result<()> {
        let delete: u8 = 1;
        // SAFETY: 借用独占的新文件句柄；FileDispositionInfo=4，FILE_DISPOSITION_INFO 是一个 BOOLEAN。
        if unsafe {
            SetFileInformationByHandle(file.as_raw_handle(), 4, (&delete as *const u8).cast(), 1)
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
}

pub(super) fn regular(file: &File) -> Result<()> {
    let m = file.metadata()?;
    // SAFETY: 句柄在调用期间保持打开；FILE_TYPE_DISK=1。
    ensure!(
        m.is_file()
            && m.file_attributes() & REPARSE == 0
            && unsafe { GetFileType(file.as_raw_handle()) } == 1,
        "只接受无重解析点的普通磁盘文件"
    );
    let mut info = StandardInfo::default();
    // SAFETY: FileStandardInfo=1，缓冲区为对应的 C 结构，传入其实际字节长度。
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            1,
            (&mut info as *mut StandardInfo).cast(),
            size_of::<StandardInfo>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    ensure!(
        info.links == 1 && info.directory == 0 && info.delete_pending == 0,
        "不接受硬链接或正在删除的文件"
    );
    Ok(())
}
