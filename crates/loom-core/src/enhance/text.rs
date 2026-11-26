use crate::redact::redact;
use anyhow::{Result, ensure};

pub(super) const REPORT_BYTES: usize = 4096;
const TITLE_BYTES: usize = 192;

/// 先对完整文本脱敏，再单行化，并复查单行化新形成的凭据模式。绝不对凭据截半后才脱敏。
pub(super) fn clean(raw: &str) -> String {
    let raw = redact(raw).0;
    let clean: String = raw.chars().map(|c| {
        if c.is_control() || matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}') { ' ' } else { c }
    }).collect();
    let line = redact(&clean.split_whitespace().collect::<Vec<_>>().join(" ")).0;
    // 完整块已由通用规则处理；仍有 PEM 边界的片段可能缺头或缺尾，宁可省略整段。
    if line.contains("-----BEGIN ") || line.contains("PRIVATE KEY-----") {
        "（已省略密钥块片段）".into()
    } else {
        line
    }
}

pub(super) fn input(raw: &str, max: usize) -> Result<String> {
    ensure!(raw.len() <= max, "输入超过 {max} UTF-8 字节，未发送");
    let text = clean(raw);
    ensure!(
        !text.is_empty() && text.len() <= max,
        "输入为空或超限，未发送"
    );
    Ok(text)
}

pub(super) fn title(raw: &str) -> Option<String> {
    let text = input(raw, 4096).ok()?;
    Some(clip(text, TITLE_BYTES))
}

pub(super) fn clip(mut text: String, bytes: usize) -> String {
    if text.len() > bytes {
        let mut end = bytes - '…'.len_utf8();
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}
