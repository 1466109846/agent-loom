//! 通用小工具：时间、哈希、ID。

use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// 当前 UTC 秒级时间戳。
pub fn now_ts() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// 解析 RFC3339 时间（支持 9 位小数秒），失败返回 None。
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    OffsetDateTime::parse(s.trim(), &Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp())
}

/// 时间戳格式化为 `YYYY-MM-DD`。
pub fn fmt_date(ts: i64) -> String {
    OffsetDateTime::from_unix_timestamp(ts)
        .map(|t| format!("{:04}-{:02}-{:02}", t.year(), u8::from(t.month()), t.day()))
        .unwrap_or_else(|_| "?".into())
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// 归一化后的内容哈希：忽略首尾空白和连续空白差异，用于去重。
pub fn content_hash(title: &str, body: &str) -> String {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    sha256_hex(format!("{}\n{}", norm(title), norm(body)).as_bytes())
}

pub fn new_id() -> String {
    ulid::Ulid::new().to_string()
}

/// 给人看的路径：去掉 Windows 规范化路径的 `\\?\` 前缀，分隔符统一。定位仍用原路径。
pub fn display_path(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    if !cfg!(windows) {
        return text;
    }
    match text.strip_prefix(r"\\?\UNC\") {
        Some(share) => format!(r"\\{share}"),
        None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned(),
    }
    .replace('/', "\\")
}

/// 字节数写成 KiB、MiB 这样的单位。
pub fn fmt_size(bytes: u64) -> String {
    let mut value = bytes as f64 / 1024.0;
    for unit in ["KiB", "MiB", "GiB"] {
        if value < 1024.0 {
            return format!("{value:.1} {unit}");
        }
        value /= 1024.0;
    }
    format!("{value:.1} TiB")
}

/// 同目录落盘后一次替换；凭据也用它，临时文件从创建起就是私有的。
pub(crate) fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::Write;
    let dir = path.parent().context("缺少目标目录")?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    Ok(())
}

/// 按字符数截断，超出时加省略号。
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nanosecond_rfc3339() {
        assert!(parse_rfc3339("2026-03-10T19:38:16.544904000+00:00").is_some());
        assert!(parse_rfc3339("not a date").is_none());
    }

    #[test]
    fn content_hash_ignores_whitespace() {
        assert_eq!(content_hash("a  b", "x\n y"), content_hash("a b", "x y"));
    }

    #[test]
    fn truncate_handles_cjk() {
        assert_eq!(truncate_chars("你好世界", 3), "你好…");
        assert_eq!(truncate_chars("你好", 3), "你好");
    }
}
