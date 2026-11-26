//! 当前项目的短期任务与按问题查阅的证据。读路径不建表，不改变项目身份版本。

mod canvas;
mod evidence;
mod text;

#[cfg(test)]
mod tests;

use anyhow::{Context, Result, ensure};
use rusqlite::Connection;

use crate::Store;

/// 将根目录标记、库内绑定与后续业务读取/写入放在同一次数据库快照内核对。
fn with_project<T>(store: &Store, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let project = store.project().context("请先绑定当前项目")?;
    store.savepoint(|conn| {
        project.verify_live_root()?;
        let bound: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.loom_identity WHERE singleton=1 AND project_id=?1 AND canonical_root=?2)",
            [project.id(), project.root().to_str().context("项目根编码无效")?], |r| r.get(0),
        )?;
        ensure!(bound, "项目身份不符；未加载或更改项目内容");
        f(conn)
    })
}

fn has_table(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM main.sqlite_schema WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )?)
}

fn check_id(id: &str) -> Result<()> {
    ensure!(
        ulid::Ulid::from_string(id).is_ok(),
        "需要完整的本项目记录 ID"
    );
    Ok(())
}
