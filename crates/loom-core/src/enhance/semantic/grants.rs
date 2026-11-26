//! 授权表仅在人工 allow 时创建。读请求不建表；状态变化和撤权在同一 SQLite 事务内生效。
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};

use super::{LIMIT, Member};
use crate::{
    Store,
    enhance::{checked, text},
};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS main.semantic_grants(
    memory_id TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE
) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS main.semantic_revoke_inactive AFTER UPDATE OF status ON memories
WHEN NEW.status <> 'active' BEGIN DELETE FROM semantic_grants WHERE memory_id=OLD.id; END;";

fn exists(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE type='table' AND name='semantic_grants')", [], |r| r.get(0))?)
}
fn valid(id: &str) -> Result<()> {
    ensure!(
        ulid::Ulid::from_string(id).is_ok(),
        "需要本项目记忆的完整 ID"
    );
    Ok(())
}

/// 仅供已收取人工确认的本地命令调用。重复授权幂等，满额时不替换任何条目。
pub fn allow(store: &Store, id: &str) -> Result<String> {
    valid(id)?;
    checked(store, || {
        let conn = &store.conn;
        let active: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.memories WHERE id=?1 AND status='active')",
            [id],
            |r| r.get(0),
        )?;
        ensure!(active, "只能授权本项目的有效记忆");
        conn.execute_batch(SCHEMA)?;
        let existing: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.semantic_grants WHERE memory_id=?1)",
            [id],
            |r| r.get(0),
        )?;
        if !existing {
            let n = conn.execute("INSERT INTO main.semantic_grants(memory_id) SELECT ?1 WHERE (SELECT count(*) FROM main.semantic_grants) < ?2",params![id, LIMIT as i64])?;
            ensure!(n == 1, "授权集已满（最多 {LIMIT} 条）；请先显式撤回一项");
        }
        Ok(format!("本项目已授权：{id}"))
    })
}

/// 仅供已收取人工确认的本地命令调用；未授权的 ID 不披露库中是否存在同名记忆。
pub fn deny(store: &Store, id: &str) -> Result<String> {
    valid(id)?;
    checked(store, || {
        if exists(&store.conn)?
            && store
                .conn
                .execute("DELETE FROM main.semantic_grants WHERE memory_id=?1", [id])?
                != 0
        {
            Ok(format!("已撤回语义查找授权：{id}"))
        } else {
            Ok("该编号不在本项目授权集中。".into())
        }
    })
}

/// 调用方持有已核对身份的读快照。授权表驱动主键查找，不遍历记忆表或全库 FTS。
pub(super) fn members(conn: &Connection) -> Result<Vec<Member>> {
    if !exists(conn)? {
        return Ok(Vec::new());
    }
    let mut st = conn.prepare(
        "SELECT g.memory_id, CASE WHEN length(CAST(m.title AS BLOB)) <= 4096 THEN m.title END
        FROM main.semantic_grants g CROSS JOIN main.memories m
        WHERE m.id=g.memory_id AND m.status='active' ORDER BY g.memory_id LIMIT ?1",
    )?;
    let rows = st.query_map([(LIMIT + 1) as i64], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut all = Vec::new();
    for row in rows {
        let (id, raw) = row?;
        valid(&id)?;
        all.push(Member {
            id,
            title: raw.as_deref().and_then(text::title),
        });
    }
    ensure!(
        all.len() <= LIMIT,
        "授权集超过 {LIMIT} 条，拒绝截取或发送；请先撤回多余条目"
    );
    Ok(all)
}
