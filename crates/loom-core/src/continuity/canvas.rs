use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};

use super::text::{MAX_REPORT, Report, field};
use super::{check_id, has_table, with_project};
use crate::{
    Store,
    util::{new_id, now_ts},
};

// v1 的 canvases 是无结构的 Mermaid 占位。不要把旧图猜成任务，也不修改其格式。
// 自增事件号同时给同一秒的更新排序；关闭只冻结游标，不删除或覆盖旧步骤。
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS task_canvas (
    id TEXT PRIMARY KEY NOT NULL, goal TEXT NOT NULL, closed INTEGER NOT NULL DEFAULT 0 CHECK(closed IN (0,1)),
    latest INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS task_canvas_pending ON task_canvas(closed,latest DESC);
CREATE TABLE IF NOT EXISTS task_step (
    seq INTEGER PRIMARY KEY, task_id TEXT NOT NULL REFERENCES task_canvas(id),
    next TEXT NOT NULL, evidence TEXT NOT NULL, saved_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS task_step_history ON task_step(task_id,seq DESC);";
const FIELD_BYTES: usize = 384;
const PENDING_COUNT: usize = 4;
const HISTORY_COUNT: usize = 6;
// 给父级 resume 的身份行、最近记忆等保留空间；父级仍须对合成文本单独计量。
const PENDING_BYTES: usize = 768;

impl Store {
    /// 创建本项目任务和第一步。返回值只有可供后续调用的 ID。
    pub fn canvas_open(&self, goal: &str, next: &str, evidence: &str) -> Result<String> {
        let goal = field(goal, FIELD_BYTES)?;
        let next = field(next, FIELD_BYTES)?;
        let evidence = field(evidence, FIELD_BYTES)?;
        let id = new_id();
        with_project(self, |tx| {
            tx.execute_batch(SCHEMA)?;
            tx.execute(
                "INSERT INTO task_canvas(id,goal) VALUES (?1,?2)",
                params![id, goal],
            )?;
            append_step(tx, &id, &next, &evidence)
        })?;
        Ok(id)
    }

    /// 仅对未关闭任务追加下一步及其证据位置。
    pub fn canvas_update(&self, id: &str, next: &str, evidence: &str) -> Result<String> {
        check_id(id)?;
        let next = field(next, FIELD_BYTES)?;
        let evidence = field(evidence, FIELD_BYTES)?;
        with_project(self, |tx| {
            ensure!(has_table(tx, "task_canvas")?, "未找到该任务");
            append_step(tx, id, &next, &evidence)
        })?;
        Ok(format!("已更新任务 {id}"))
    }

    pub fn canvas_close(&self, id: &str) -> Result<String> {
        check_id(id)?;
        with_project(self, |tx| {
            ensure!(has_table(tx, "task_canvas")?, "未找到该任务");
            ensure!(
                tx.execute(
                    "UPDATE task_canvas SET closed=1 WHERE id=?1 AND closed=0",
                    [id]
                )? == 1,
                "任务不存在或已关闭"
            );
            Ok(format!("已关闭任务 {id}"))
        })
    }

    /// 按 ID 取任务及最近几次记录；已关闭的任务仍可按号查阅。
    pub fn canvas_get(&self, id: &str) -> Result<String> {
        check_id(id)?;
        with_project(self, |tx| {
            ensure!(has_table(tx, "task_canvas")?, "未找到该任务");
            let (goal, closed): (String, bool) = tx
                .query_row(
                    "SELECT goal,closed FROM task_canvas WHERE id=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .context("未找到该任务")?;
            let mut out = Report::new(MAX_REPORT, "");
            out.line(&format!(
                "任务 {id} · {}",
                if closed { "已关闭" } else { "未完成" }
            ));
            out.line(&format!("目标：{goal}"));
            let mut st = tx.prepare("SELECT next,evidence,saved_at FROM task_step WHERE task_id=?1 ORDER BY seq DESC LIMIT ?2")?;
            let mut rows = st.query(params![id, (HISTORY_COUNT + 1) as i64])?;
            let mut n = 0;
            while let Some(r) = rows.next()? {
                if n == HISTORY_COUNT {
                    out.omit();
                    break;
                }
                let next: String = r.get(0)?;
                let evidence: String = r.get(1)?;
                let stamp: i64 = r.get(2)?;
                let stage = if n == 0 {
                    "最后记录"
                } else {
                    "历史记录"
                };
                if !out.line(&format!(
                    "{stage} {stamp} · 下一步：{next} · 证据：{evidence}"
                )) {
                    break;
                }
                n += 1;
            }
            Ok(out.finish())
        })
    }

    /// 恢复入口只读未关闭任务的当前步骤，不触碰记忆正文、历史步骤或证据文件。
    pub fn canvas_pending(&self) -> Result<String> {
        with_project(self, |tx| {
            if !has_table(tx, "task_canvas")? {
                return Ok(String::new());
            }
            let mut st = tx.prepare(
                "SELECT c.id,c.goal,s.next,s.evidence FROM task_canvas c
                JOIN task_step s ON s.seq=c.latest AND s.task_id=c.id
                WHERE c.closed=0 ORDER BY c.latest DESC LIMIT ?1",
            )?;
            let mut rows = st.query([(PENDING_COUNT + 1) as i64])?;
            let mut out = Report::new(PENDING_BYTES, "");
            let mut n = 0;
            while let Some(r) = rows.next()? {
                if n == PENDING_COUNT {
                    out.omit();
                    break;
                }
                if n == 0 {
                    out.line("未完成任务（按 ID 查详情）");
                }
                let id: String = r.get(0)?;
                let goal = out.brief(&r.get::<_, String>(1)?, 72);
                let next = out.brief(&r.get::<_, String>(2)?, 80);
                let evidence = out.brief(&r.get::<_, String>(3)?, 88);
                if !out.line(&format!(
                    "- {id} · 目标：{goal} · 下一步：{next} · 证据：{evidence}"
                )) {
                    break;
                }
                n += 1;
            }
            Ok(out.finish())
        })
    }
}

fn append_step(tx: &rusqlite::Connection, id: &str, next: &str, evidence: &str) -> Result<()> {
    if tx.execute(
        "INSERT INTO task_step(task_id,next,evidence,saved_at)
        SELECT id,?2,?3,?4 FROM task_canvas WHERE id=?1 AND closed=0",
        params![id, next, evidence, now_ts()],
    )? != 1
    {
        bail!("任务不存在或已关闭");
    }
    tx.execute(
        "UPDATE task_canvas SET latest=?2 WHERE id=?1",
        params![id, tx.last_insert_rowid()],
    )?;
    Ok(())
}
