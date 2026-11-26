//! SQLite 存储与表结构迁移。

use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;

use crate::project::ProjectIdentity;

pub const SCHEMA_VERSION: i64 = 2;
pub(crate) const APPLICATION_ID: i32 = i32::from_be_bytes(*b"LOOM");

pub(crate) const SCHEMA_V1: &str = r#"
-- 结构层（M1 起使用，M0 先建表）
CREATE TABLE IF NOT EXISTS files (
  id INTEGER PRIMARY KEY,
  path TEXT UNIQUE NOT NULL,
  lang TEXT, size INTEGER, loc INTEGER,
  sha256 TEXT NOT NULL,
  module TEXT,
  indexed_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS symbols (
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  name TEXT NOT NULL, qualname TEXT, kind TEXT,
  line_start INTEGER, line_end INTEGER,
  exported INTEGER NOT NULL DEFAULT 0,
  signature TEXT
);
CREATE TABLE IF NOT EXISTS file_notes (
  file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
  role TEXT, touch TEXT, contract TEXT, guard TEXT,
  source_sha256 TEXT NOT NULL,
  author TEXT, updated_at INTEGER NOT NULL
);

-- 意图层
CREATE TABLE IF NOT EXISTS intents (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  title TEXT NOT NULL, body TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'active',
  confirmed INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER NOT NULL
);

-- 经验层
CREATE TABLE IF NOT EXISTS memories (
  id TEXT PRIMARY KEY,
  type TEXT NOT NULL,
  title TEXT NOT NULL,
  body TEXT NOT NULL,
  tags TEXT NOT NULL DEFAULT '[]',
  importance INTEGER NOT NULL DEFAULT 2,
  confidence REAL NOT NULL DEFAULT 0.8,
  status TEXT NOT NULL DEFAULT 'active',
  origin TEXT NOT NULL DEFAULT 'agent',
  legacy_id TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  content_hash TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS memories_hash ON memories(content_hash);
CREATE UNIQUE INDEX IF NOT EXISTS memories_legacy ON memories(legacy_id) WHERE legacy_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS memories_updated ON memories(status, updated_at DESC);

CREATE TABLE IF NOT EXISTS anchors (
  memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
  path TEXT NOT NULL,
  symbol TEXT NOT NULL DEFAULT '',
  sha256_at_save TEXT,
  PRIMARY KEY (memory_id, path, symbol)
);
CREATE INDEX IF NOT EXISTS anchors_path ON anchors(path);

-- 三层之间的边
CREATE TABLE IF NOT EXISTS edges (
  src TEXT NOT NULL,
  dst TEXT NOT NULL,
  kind TEXT NOT NULL,
  weight REAL NOT NULL DEFAULT 1.0,
  provenance TEXT NOT NULL,
  PRIMARY KEY (src, dst, kind)
);
CREATE INDEX IF NOT EXISTS edges_dst ON edges(dst, kind);

-- 全文检索：trigram 分词，中英文混排都能做子串命中
CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(
  key UNINDEXED, title, body, tags,
  tokenize = 'trigram'
);

CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
  INSERT INTO fts(key, title, body, tags) VALUES ('mem:' || new.id, new.title, new.body, new.tags);
END;
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
  DELETE FROM fts WHERE key = 'mem:' || old.id;
END;
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE OF title, body, tags ON memories BEGIN
  DELETE FROM fts WHERE key = 'mem:' || old.id;
  INSERT INTO fts(key, title, body, tags) VALUES ('mem:' || new.id, new.title, new.body, new.tags);
END;

-- 可选向量
CREATE TABLE IF NOT EXISTS vectors (
  key TEXT PRIMARY KEY, model TEXT NOT NULL, dim INTEGER NOT NULL, vec BLOB NOT NULL
);

-- 短期任务画布
CREATE TABLE IF NOT EXISTS canvases (
  name TEXT PRIMARY KEY, goal TEXT NOT NULL,
  mermaid TEXT NOT NULL, open INTEGER NOT NULL DEFAULT 1, updated_at INTEGER NOT NULL
);
"#;

/// 由绑定操作在同一事务中添加身份表和版本标记，旧记忆和索引不重建。
pub(crate) fn create_identity_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS loom_identity (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1),
        project_id TEXT NOT NULL UNIQUE, canonical_root TEXT NOT NULL
    )")?;
    conn.pragma_update(None, "application_id", APPLICATION_ID)?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

pub struct Store {
    pub(crate) conn: Connection,
    project: Option<ProjectIdentity>,
}

impl Store {
    /// 无磁盘路径的隔离库；不能再附上任意项目根。
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA_V1)?;
        create_identity_schema(&conn)?;
        Self::from_connection(conn, None)
    }

    pub(crate) fn from_connection(conn: Connection, project: Option<ProjectIdentity>) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self { conn, project })
    }

    pub fn project(&self) -> Option<&ProjectIdentity> {
        self.project.as_ref()
    }

    pub fn root(&self) -> Option<&Path> {
        self.project().map(ProjectIdentity::root)
    }

    /// 在 SAVEPOINT 里执行 `f`：出错回滚，成功释放。
    /// 和 `BEGIN` 不同，SAVEPOINT 可以嵌套在外层事务里（例如批量导入时）。
    pub(crate) fn savepoint<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("SAVEPOINT loom_sp")?;
        match f(&self.conn) {
            Ok(v) => {
                self.conn.execute_batch("RELEASE loom_sp")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK TO loom_sp; RELEASE loom_sp");
                Err(e)
            }
        }
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_fresh_db() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn reopen_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let location = crate::project::ProjectLocation::select(dir.path(), Some(dir.path()), None).unwrap();
        let first = location.bind(crate::project::Bind::Initialize).unwrap();
        let s = location.open(false).unwrap();
        assert_eq!(s.project().unwrap().id(), first.id());
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    }
}
