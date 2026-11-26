//! 最终文本（含标签、换行与省略提示）的 UTF-8 字节预算。
//! 对按字节分词的宿主可作保守上界，不是任意宿主的精确词元数。
use anyhow::{Result, ensure};

use crate::redact::redact;

pub(super) const MAX_REPORT: usize = 1500;
const OMITTED: &str = "… 已省略部分条目或长字段。\n";

fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// 限制原始输入后才做脱敏；不把多行文本解释为新的任务或证据段。
pub(super) fn field(raw: &str, max: usize) -> Result<String> {
    ensure!(
        raw.len() <= max && !raw.chars().any(unsafe_char),
        "需要不超过 {max} 字节的单行文本"
    );
    let value = raw.trim();
    ensure!(!value.is_empty(), "文本不能为空");
    let (value, _) = redact(value);
    ensure!(value.len() <= max, "脱敏后文本过长");
    Ok(value)
}

pub(super) struct Report {
    out: String,
    limit: usize,
    omitted: bool,
    footer: &'static str,
}

impl Report {
    pub(super) fn new(limit: usize, footer: &'static str) -> Self {
        assert!(limit <= MAX_REPORT && limit >= OMITTED.len() + footer.len());
        Self {
            out: String::new(),
            limit,
            omitted: false,
            footer,
        }
    }

    pub(super) fn omit(&mut self) {
        self.omitted = true;
    }

    /// 原始标题、来源和锚点也可能来自旧库；先脱敏再截断，避免泄漏半个密钥。
    pub(super) fn brief(&mut self, raw: &str, bytes: usize) -> String {
        let (raw, _) = redact(raw);
        let line: String = raw
            .chars()
            .map(|c| if unsafe_char(c) { ' ' } else { c })
            .collect();
        let line = line.trim();
        if line.len() <= bytes {
            return line.into();
        }
        self.omit();
        const ELLIPSIS: &str = "…";
        let mut end = bytes.saturating_sub(ELLIPSIS.len());
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}{ELLIPSIS}", &line[..end])
    }

    /// 不把某行的 ID、状态或箭头截成半个。省略提示和尾注不参与争抢预算。
    pub(super) fn line(&mut self, line: &str) -> bool {
        if self.out.len() + line.len() + 1 + OMITTED.len() + self.footer.len() > self.limit {
            self.omit();
            return false;
        }
        self.out.push_str(line);
        self.out.push('\n');
        true
    }

    /// 每段单独分配空间，早先的长标题或密集关系不能挤掉后面的复核与缺口。
    pub(super) fn section(&mut self, heading: &str, lines: &[String], allowance: usize) {
        self.line(heading);
        if lines.is_empty() {
            self.line("- 无");
            return;
        }
        let mut left = allowance;
        for line in lines {
            if line.len() + 1 > left {
                self.omit();
                continue;
            }
            left -= line.len() + 1;
            self.line(line);
        }
    }

    pub(super) fn finish(mut self) -> String {
        if self.omitted {
            self.out.push_str(OMITTED);
        }
        self.out.push_str(self.footer);
        self.out
    }
}
