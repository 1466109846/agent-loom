//! Unix 使用目录描述符相对打开；不以 canonicalize 后的字符串进行第二次路径解析。
use super::Mode;
use anyhow::{Result, ensure};
use std::{
    ffi::{CString, OsStr},
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        raw::{c_char, c_int},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path},
};

// openat(2) 的平台 ABI。只在已知 ABI 上启用；未知平台不降级成会跟随链接的打开方式。
#[cfg(target_os = "linux")]
const FLAGS: (c_int, c_int, c_int, c_int, c_int) = (
    if cfg!(any(target_arch = "arm", target_arch = "aarch64")) {
        0o100000
    } else {
        0o400000
    }, // O_NOFOLLOW
    if cfg!(any(target_arch = "arm", target_arch = "aarch64")) {
        0o40000
    } else {
        0o200000
    }, // O_DIRECTORY
    0o2000000,
    0o4000,
    0o100 | 0o200, // CLOEXEC, NONBLOCK, CREAT|EXCL
);
#[cfg(target_vendor = "apple")]
const FLAGS: (c_int, c_int, c_int, c_int, c_int) = (0x100, 0x100000, 0x1000000, 4, 0x200 | 0x800);
#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
const FLAGS: (c_int, c_int, c_int, c_int, c_int) = (0, 0, 0, 0, 0);
unsafe extern "C" {
    fn openat(dir: c_int, path: *const c_char, flags: c_int, ...) -> c_int;
}

pub(super) fn check_location(path: &Path) -> Result<()> {
    ensure!(
        cfg!(target_vendor = "apple")
            || cfg!(all(
                target_os = "linux",
                any(
                    target_arch = "x86",
                    target_arch = "x86_64",
                    target_arch = "arm",
                    target_arch = "aarch64",
                    target_arch = "riscv64"
                )
            )),
        "此平台暂不支持安全打开分享文件"
    );
    ensure!(
        !path.as_os_str().as_bytes().starts_with(b"//"),
        "不接受网络路径"
    );
    Ok(())
}
pub(super) fn check_name(name: &OsStr) -> Result<()> {
    ensure!(
        name.to_str()
            .is_some_and(|s| !s.contains(['\\', ':']) && !s.chars().any(char::is_control)),
        "不接受含设备或网络路径语法的文件名"
    );
    Ok(())
}
pub(super) fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn relative(dir: &File, name: &OsStr, flags: c_int) -> Result<File> {
    let name = CString::new(name.as_bytes())?;
    // SAFETY: 目录句柄在调用期间有效，name 为单一、以 NUL 结尾的分量，创建权限仅限本人。
    let fd = unsafe {
        openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | FLAGS.0 | FLAGS.2,
            0o600 as c_int,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: openat 刚返回独占所有权的文件描述符。
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) struct Parent(File);
impl Parent {
    pub(super) fn open(path: &Path) -> Result<Self> {
        let mut dir = File::open("/")?;
        for c in path.components() {
            if let Component::Normal(name) = c {
                dir = relative(&dir, name, FLAGS.1)?;
            }
        }
        Ok(Self(dir))
    }
    pub(super) fn file(&self, name: &OsStr, mode: Mode) -> Result<File> {
        // O_EXCL|O_CREAT 是 create_new 的 openat 等价形式；NONBLOCK 不在 FIFO 上等待。
        relative(
            &self.0,
            name,
            if mode == Mode::Create {
                FLAGS.4 | 1
            } else {
                FLAGS.3
            },
        )
    }
    pub(super) fn discard(&self, _: &OsStr, file: &File) -> Result<()> {
        // 没有 Windows 的按句柄删除保证。不按名字 unlink（名字可能已被替换），
        // 只清空本次独占创建的文件，并向调用方报告失败。
        file.set_len(0)?;
        Ok(())
    }
}
pub(super) fn regular(file: &File) -> Result<()> {
    let m = file.metadata()?;
    ensure!(m.is_file() && m.nlink() == 1, "只接受不是硬链接的普通文件");
    Ok(())
}
