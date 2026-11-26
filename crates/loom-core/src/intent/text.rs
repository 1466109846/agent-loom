//! 文本先完整脱敏，再做单行化和预算裁剪。旧库的超长字段宁可整项不显示，也不截断凭据后尝试识别。
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::table_exists;
use crate::redact::redact;

pub(super) const TITLE_BYTES: usize = 192;
// 旧意图、经验的标题未受新提议的写入限额约束；安全窗口内先完整脱敏，再生成短预览。
pub(super) const READ_TITLE_BYTES: i64 = 4_096;
pub(super) const BODY_BYTES: usize = 1_024;
const ID_BYTES: usize = 64;
const PREVIEW_BYTES: usize = 1_500;
pub(super) const ROW_LIMIT: usize = 64;
const OMITTED_FIELD: &str = "（字段超限或无效，已省略）";

pub(crate) fn one_line(raw: &str) -> String {
    let (masked, _) = redact(raw);
    let mut out = String::with_capacity(masked.len());
    let mut space = false;
    for c in masked.chars() {
        // 除 C0/C1、行分隔符外，还清理双向覆盖/隔离等不可见格式字符，避免伪造状态或编号。
        if c.is_whitespace()
            || c.is_control()
            || matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
        {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    // 单行化可能拼出新的可识别形式（例如 Bearer 与令牌间原有方向控制符）。
    redact(&out).0
}

pub(super) fn field(raw: &str, max: usize, name: &'static str) -> Result<String> {
    ensure!(raw.len() <= max, "{name}超过 {max} UTF-8 字节");
    let clean = one_line(raw);
    ensure!(
        !clean.is_empty() && clean.len() <= max,
        "{name}为空或超出长度限制"
    );
    Ok(clean)
}

pub(crate) fn valid_id(id: &str) -> bool {
    (1..=ID_BYTES).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        && redact(id).1 == 0
}

fn clip(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.into();
    }
    let mut i = limit - '…'.len_utf8();
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    format!("{}…", &s[..i])
}

// iif 先在 SQLite 内核判长度；Rust 不取得任意长旧字段或半截密钥。列名只来自本模块常量。
pub(super) const SUMMARY_COLS: &str = "iif(typeof(id)='text' AND octet_length(id)<=?1,id,NULL),
    iif(typeof(title)='text' AND octet_length(title)<=?2,title,NULL)";
pub(super) const ID_LIMIT: i64 = ID_BYTES as i64;

pub(super) struct Summary {
    pub id: Option<String>,
    pub title: Option<String>,
}

impl Summary {
    pub fn from_row(r: &Row<'_>) -> rusqlite::Result<Self> {
        let id = r.get::<_, Option<String>>(0)?.filter(|v| valid_id(v));
        let title = r
            .get::<_, Option<String>>(1)?
            .map(|s| one_line(&s))
            .filter(|s| !s.is_empty());
        Ok(Self { id, title })
    }
    pub fn line(&self, label: &str) -> String {
        let title = clip(self.title.as_deref().unwrap_or(OMITTED_FIELD), TITLE_BYTES);
        format!(
            "{label} [{}] {title}",
            self.id.as_deref().unwrap_or("编号无效")
        )
    }
    pub fn complete(&self) -> bool {
        self.id.is_some()
            && self
                .title
                .as_deref()
                .is_some_and(|s| s.len() <= TITLE_BYTES)
    }
}

fn kind(r: &Row<'_>) -> rusqlite::Result<&'static str> {
    Ok(match r.get_ref(2)?.as_str().ok() {
        Some("goal") => "目标",
        Some("constraint") => "红线",
        Some("non_goal") => "非目标",
        Some("decision") => "决策",
        _ => "未知类型",
    })
}

fn state(r: &Row<'_>) -> rusqlite::Result<&'static str> {
    let status = r.get_ref(3)?.as_str().ok();
    let confirmed = r.get_ref(4)?.as_i64().ok();
    Ok(match (status, confirmed) {
        (Some("active"), Some(0)) => "待确认",
        (Some("active"), Some(1)) => "已确认",
        (Some("revoked"), _) => "已撤销",
        (Some("superseded"), _) => "已取代",
        (Some("archived"), _) => "已归档",
        _ => "未知状态",
    })
}

fn summary_line(r: &Row<'_>) -> rusqlite::Result<String> {
    let summary = Summary::from_row(r)?;
    Ok(summary.line(&format!("{}·{}", kind(r)?, state(r)?)))
}

pub(super) fn list(conn: &Connection) -> Result<String> {
    let sql = format!(
        "SELECT {SUMMARY_COLS}, substr(kind,1,16),substr(status,1,16),confirmed
        FROM intents ORDER BY updated_at DESC,id LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![ID_LIMIT, READ_TITLE_BYTES, (ROW_LIMIT + 1) as i64])?;
    let mut out = String::from("项目意图（按编号查询正文）");
    const NOTE: &str = "\n还有意图未展示。";
    for i in 0..=ROW_LIMIT {
        let Some(row) = rows.next()? else {
            if i == 0 {
                out.push_str("：暂无记录");
            }
            break;
        };
        let line = summary_line(row)?;
        if i == ROW_LIMIT || out.len() + 1 + line.len() + NOTE.len() > PREVIEW_BYTES {
            out.push_str(NOTE);
            break;
        }
        out.push('\n');
        out.push_str(&line);
    }
    Ok(out)
}

pub(super) fn detail(conn: &Connection, id: &str) -> Result<String> {
    let sql = format!(
        "SELECT {SUMMARY_COLS}, substr(kind,1,16),substr(status,1,16),confirmed,
        iif(typeof(body)='text' AND octet_length(body)<=?3,body,NULL) FROM intents WHERE id=?4"
    );
    let (mut out, body) = conn
        .query_row(
            &sql,
            params![ID_LIMIT, READ_TITLE_BYTES, BODY_BYTES as i64, id],
            |r| Ok((summary_line(r)?, r.get::<_, Option<String>>(5)?)),
        )
        .optional()?
        .context("当前项目没有该意图编号")?;
    if table_exists(conn, "intent_replacements")? {
        // 两个唯一键各一次查找。只追一跳，历史再长也不递归铺开。
        for (label, query) in [
            (
                "前项",
                "SELECT iif(octet_length(old_id)<=?1,old_id,'') FROM intent_replacements WHERE new_id=?2",
            ),
            (
                "后继",
                "SELECT iif(octet_length(new_id)<=?1,new_id,'') FROM intent_replacements WHERE old_id=?2",
            ),
        ] {
            if let Some(link) = conn
                .query_row(query, params![ID_LIMIT, id], |r| r.get::<_, String>(0))
                .optional()?
            {
                out.push_str(&format!(
                    "\n{label}：{}",
                    if valid_id(&link) {
                        link.as_str()
                    } else {
                        "编号无效"
                    }
                ));
            }
        }
    }
    out.push_str("\n正文：");
    let body = body.map(|s| one_line(&s)).filter(|s| !s.is_empty());
    out.push_str(&clip(
        body.as_deref().unwrap_or(OMITTED_FIELD),
        PREVIEW_BYTES - out.len(),
    ));
    Ok(out)
}
