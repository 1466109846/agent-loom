//! 对真正打开的文件句柄查硬链接数。无法排除另一个项目/私密路径的别名时，不读数据。
use std::{fs::File, io};

#[cfg(unix)]
pub(super) fn single_link(file: &File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.nlink() == 1)
}

#[cfg(windows)]
pub(super) fn single_link(file: &File) -> io::Result<bool> {
    use std::{ffi::c_void, mem::MaybeUninit, os::windows::io::AsRawHandle};
    // 标准库的 MetadataExt::number_of_links 仍非稳定 API。此 ABI 自 Windows Vista 可用。
    #[repr(C)]
    struct FileStandardInfo {
        allocation_size: i64,
        end_of_file: i64,
        number_of_links: u32,
        delete_pending: u8,
        directory: u8,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            handle: *mut c_void,
            class: i32,
            info: *mut c_void,
            size: u32,
        ) -> i32;
    }
    const FILE_STANDARD_INFO: i32 = 1;
    let mut info = MaybeUninit::<FileStandardInfo>::uninit();
    // SAFETY: 活跃的 File 句柄；缓冲区对齐、大小和信息类均与 FILE_STANDARD_INFO 匹配。
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FILE_STANDARD_INFO,
            info.as_mut_ptr().cast(),
            size_of::<FileStandardInfo>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: 成功返回后才取已填充的字段。
    Ok(unsafe { info.assume_init().number_of_links } == 1)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn single_link(_: &File) -> io::Result<bool> {
    Ok(false)
}
