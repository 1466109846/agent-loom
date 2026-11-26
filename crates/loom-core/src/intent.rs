//! 绑定项目内的意图、显式置顶与启动概览。提议不是授权；确认类操作由本地调用方把关。

mod overview;
#[cfg(test)]
mod tests;
mod text;

pub(crate) use overview::constraints;
pub(crate) use text::{one_line, valid_id};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};

use crate::Store;
use crate::project::ProjectIdentity;
use crate::util::{new_id, now_ts};
use text::{BODY_BYTES, TITLE_BYTES, field};

// 旧库已有 intents(status, confirmed)：active/0 是提议，active/1 才是当前结论。
// 结束时保留 confirmed（曾得到确认），新概览始终同时判断两列。
const INTENT_INDEX: &str = "CREATE INDEX IF NOT EXISTS loom_intent_effective
    ON intents(kind, status, confirmed, updated_at DESC, id)";
const REPLACEMENTS: &str = "CREATE TABLE IF NOT EXISTS intent_replacements(
    old_id TEXT PRIMARY KEY REFERENCES intents(id), new_id TEXT NOT NULL UNIQUE REFERENCES intents(id),
    CHECK(old_id <> new_id)) WITHOUT ROWID";
const PINS: &str = "CREATE TABLE IF NOT EXISTS memory_pins(
    memory_id TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE) WITHOUT ROWID;
    CREATE TRIGGER IF NOT EXISTS loom_unpin_inactive AFTER UPDATE OF status ON memories
    WHEN NEW.status <> 'active' BEGIN DELETE FROM memory_pins WHERE memory_id=NEW.id; END;";

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )?)
}

impl Store {
    // 也适用于只读连接。所有状态检查、选择、写入及新建辅助表共用这一次 SQLite 快照。
    pub(crate) fn with_intent_project<T>(
        &self,
        f: impl FnOnce(&Connection, &ProjectIdentity) -> Result<T>,
    ) -> Result<T> {
        let project = self.project().context("需要已绑定的项目库")?;
        self.savepoint(|conn| {
            project.verify_live_root()?;
            let matches: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM main.loom_identity WHERE singleton=1 AND project_id=?1 AND canonical_root=?2)",
                params![project.id(), project.root().to_str().context("项目根编码无效")?], |r| r.get(0),
            )?;
            ensure!(matches, "项目身份不符；未加载或更改项目内容");
            f(conn, project)
        })
    }

    /// 只登记提议。返回不带前后缀的新编号，供显式查询或本地确认使用。
    pub fn intent_propose(&self, kind: &str, title: &str, body: &str) -> Result<String> {
        ensure!(
            matches!(kind, "goal" | "constraint" | "non_goal" | "decision"),
            "意图类型须为 goal、constraint、non_goal 或 decision"
        );
        let title = field(title, TITLE_BYTES, "标题")?;
        let body = field(body, BODY_BYTES, "正文")?;
        self.with_intent_project(|conn, _| {
            let id = new_id();
            conn.execute_batch(INTENT_INDEX)?;
            conn.execute(
                "INSERT INTO intents (id,kind,title,body,status,confirmed,updated_at)
                VALUES (?1,?2,?3,?4,'active',0,?5)",
                params![id, kind, title, body, now_ts()],
            )?;
            Ok(id)
        })
    }

    pub fn intent_get(&self, id: &str) -> Result<String> {
        ensure!(valid_id(id), "无效的意图编号");
        self.with_intent_project(|conn, _| text::detail(conn, id))
    }

    pub fn intent_list(&self) -> Result<String> {
        self.with_intent_project(|conn, _| text::list(conn))
    }

    /// 仅本地用户明确操作：提议 → 已确认，不允许二次提升、复活或跳过提议。
    pub fn intent_confirm(&self, id: &str) -> Result<String> {
        ensure!(valid_id(id), "无效的意图编号");
        self.with_intent_project(|conn, _| {
            confirm(conn, id, now_ts())?;
            Ok(format!("已确认：{id}"))
        })
    }

    /// 仅本地用户明确操作：撤销有效确认，保留原文。
    pub fn intent_revoke(&self, id: &str) -> Result<String> {
        ensure!(valid_id(id), "无效的意图编号");
        self.with_intent_project(|conn, _| {
            end_confirmation(conn, id, "revoked", now_ts())?;
            Ok(format!("已撤销：{id}"))
        })
    }

    /// 仅本地用户明确操作：一次提交更新两端和追溯关系，任一前置条件不成立则不变。
    pub fn intent_replace(&self, old: &str, new: &str) -> Result<String> {
        ensure!(
            valid_id(old) && valid_id(new) && old != new,
            "需指定两个不同的有效意图编号"
        );
        self.with_intent_project(|conn, _| {
            let now = now_ts();
            end_confirmation(conn, old, "superseded", now)?;
            confirm(conn, new, now)?;
            conn.execute_batch(REPLACEMENTS)?;
            conn.execute(
                "INSERT INTO intent_replacements(old_id,new_id) VALUES(?1,?2)",
                params![old, new],
            )?;
            Ok(format!("已取代：{old} → {new}"))
        })
    }

    /// 仅本地用户明确置顶或取消；不改变经验本身的状态、重要度、可信度或更新时间。
    pub fn pin_memory(&self, id: &str, pin: bool) -> Result<String> {
        ensure!(valid_id(id), "无效的经验编号");
        self.with_intent_project(|conn, _| {
            let active: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM memories WHERE id=?1 AND status='active')", [id], |r| r.get(0))?;
            ensure!(active, "没有该编号的有效项目经验");
            if pin {
                conn.execute_batch(PINS)?;
                conn.execute("INSERT INTO memory_pins(memory_id) VALUES(?1) ON CONFLICT(memory_id) DO NOTHING", [id])?;
                Ok(format!("已置顶：{id}"))
            } else {
                if table_exists(conn, "memory_pins")? {
                    conn.execute("DELETE FROM memory_pins WHERE memory_id=?1", [id])?;
                }
                Ok(format!("未置顶：{id}"))
            }
        })
    }
}

fn confirm(conn: &Connection, id: &str, now: i64) -> Result<()> {
    let changed = conn.execute("UPDATE intents SET confirmed=1, updated_at=?2
        WHERE id=?1 AND status='active' AND confirmed=0 AND kind IN ('goal','constraint','non_goal','decision')", params![id, now])?;
    ensure!(changed == 1, "需要当前项目内仍待确认的提议");
    Ok(())
}

fn end_confirmation(conn: &Connection, id: &str, status: &str, now: i64) -> Result<()> {
    let changed = conn.execute("UPDATE intents SET status=?2, updated_at=?3 WHERE id=?1 AND status='active' AND confirmed=1", params![id, status, now])?;
    ensure!(changed == 1, "需要当前项目内仍有效的已确认意图");
    Ok(())
}
