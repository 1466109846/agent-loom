//! 会话去重和宿主计量协商在入口处理。这里仅在宿主接受 UTF-8 字节上界后选择一次内容。
use anyhow::Result;
use rusqlite::{Connection, params};

use super::table_exists;
use super::text::{ID_LIMIT, READ_TITLE_BYTES, ROW_LIMIT, SUMMARY_COLS, Summary, one_line};
use crate::Store;

const MAX_BUDGET: usize = 2_000;
const RULE_WARNING: &str = "\n还有约束未展示，请先查看。";
const MORE: &str = "\n还有条目未展示，请按需查询。";
const NO_GOAL: &str = "\n目标：尚未确认。";

#[derive(Default)]
struct Group {
    lines: Vec<String>,
    omitted: bool,
}

impl Group {
    fn exists(&self) -> bool {
        self.omitted || !self.lines.is_empty()
    }
}

const EFFECTIVE: &str = "status='active' AND confirmed=1";
const RULES: [(&str, &str); 2] = [("constraint", "红线"), ("non_goal", "非目标")];

fn by_kind() -> String {
    format!(
        "SELECT {SUMMARY_COLS} FROM intents WHERE {EFFECTIVE} AND kind=?4 ORDER BY updated_at DESC,id LIMIT ?3"
    )
}

fn titles(conn: &Connection, sql: &str, kind: &str) -> Result<(Vec<Summary>, bool)> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(params![
        ID_LIMIT,
        READ_TITLE_BYTES,
        (ROW_LIMIT + 1) as i64,
        kind
    ])?;
    let mut titles = Vec::new();
    while let Some(row) = rows.next()? {
        if titles.len() == ROW_LIMIT {
            return Ok((titles, true));
        }
        titles.push(Summary::from_row(row)?);
    }
    Ok((titles, false))
}

fn collect(conn: &Connection, sql: &str, kind: &str, label: &str) -> Result<Group> {
    let (titles, omitted) = titles(conn, sql, kind)?;
    let mut group = Group {
        lines: Vec::new(),
        omitted,
    };
    for summary in titles {
        // 长标题也保留按需取回的编号；部分标题或越过窗口的旧记录均计入省略。
        group.omitted |= !summary.complete();
        if summary.id.is_some() {
            group.lines.push(summary.line(label));
        }
    }
    Ok(group)
}

pub(crate) fn constraints(conn: &Connection) -> Result<(Vec<String>, usize, usize)> {
    let mut lines = Vec::new();
    let (mut limited, mut invalid) = (0, 0);
    for (kind, label) in RULES {
        let (titles, more) = titles(conn, &by_kind(), kind)?;
        if more {
            let count: i64 = conn.query_row(
                &format!("SELECT count(*) FROM intents WHERE {EFFECTIVE} AND kind=?1"),
                [kind],
                |r| r.get(0),
            )?;
            limited += count as usize - titles.len();
        }
        for summary in titles {
            if let (Some(id), Some(title)) = (summary.id, summary.title) {
                lines.push(format!("{label} [{id}] {title}"));
            } else {
                invalid += 1;
            }
        }
    }
    Ok((lines, limited, invalid))
}

impl Store {
    /// 整份 UTF-8 文本（含换行、身份、来源编号和警告）的保守上界，不是 tokenizer。
    /// `budget` 包括首行；不足以可靠交付身份和必要警示时返回空串，宿主不应注入。
    pub fn overview_text(&self, budget: usize) -> Result<String> {
        self.with_intent_project(|conn, project| {
            let budget = budget.min(MAX_BUDGET);
            let heading = one_line(&project.heading());
            if heading.len() > budget {
                return Ok(String::new());
            }

            // 不查 body、不按时间取窗口。各类别独立限量 + 1，提议或近期经验不能挤掉旧红线。
            let by_kind = by_kind();
            let mut rules = Group::default();
            for (kind, label) in RULES {
                let group = collect(conn, &by_kind, kind, label)?;
                rules.lines.extend(group.lines);
                rules.omitted |= group.omitted;
            }
            let goals = collect(conn, &by_kind, "goal", "目标")?;
            let pins = if table_exists(conn, "memory_pins")? {
                let sql = format!(
                    "SELECT {SUMMARY_COLS} FROM memory_pins p JOIN memories m ON m.id=p.memory_id
                    WHERE m.status=?4 ORDER BY p.memory_id LIMIT ?3"
                );
                collect(conn, &sql, "active", "置顶经验")?
            } else {
                Group::default()
            };
            Ok(render(heading, budget, rules, goals, pins))
        })
    }
}

fn render(mut out: String, budget: usize, rules: Group, goals: Group, pins: Group) -> String {
    // 先算全量候选与最终提示的字节数，再从低优先级一端撤掉整行。预算恰够时不误报省略。
    let no_goal = if goals.exists() { "" } else { NO_GOAL };
    let rule_count = rules.lines.len();
    let mut lines = rules.lines;
    lines.extend(goals.lines);
    lines.extend(pins.lines);
    let all = lines.len();
    let mut cost = out.len() + no_goal.len() + lines.iter().map(|l| 1 + l.len()).sum::<usize>();
    loop {
        let note = if rules.omitted || lines.len() < rule_count {
            RULE_WARNING
        } else if goals.omitted || pins.omitted || lines.len() < all {
            MORE
        } else {
            ""
        };
        if cost + note.len() <= budget {
            out.push_str(no_goal);
            for line in lines {
                out.push('\n');
                out.push_str(&line);
            }
            out.push_str(note);
            return out;
        }
        let Some(line) = lines.pop() else {
            // 连“尚未确认”也挤不下时，优先保留完整身份和红线警告。两者不全就不注入。
            if note == RULE_WARNING && out.len() + note.len() <= budget {
                out.push_str(note);
                return out;
            }
            return String::new();
        };
        cost -= 1 + line.len();
    }
}
