//! 经验层：记忆的数据模型与读写。

use std::fmt;
use std::str::FromStr;

use anyhow::{Result, anyhow, bail};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::admission::{self, Verdict};
use crate::db::Store;
use crate::redact::redact;
use crate::util::{content_hash, new_id, now_ts};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MemoryType {
    Decision,
    Bugfix,
    Gotcha,
    ErrorPattern,
    Config,
    Feature,
    Milestone,
    Progress,
    CodePattern,
    Synthesis,
}

impl MemoryType {
    pub const ALL: [MemoryType; 10] = [
        Self::Decision,
        Self::Bugfix,
        Self::Gotcha,
        Self::ErrorPattern,
        Self::Config,
        Self::Feature,
        Self::Milestone,
        Self::Progress,
        Self::CodePattern,
        Self::Synthesis,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "DECISION",
            Self::Bugfix => "BUGFIX",
            Self::Gotcha => "GOTCHA",
            Self::ErrorPattern => "ERROR_PATTERN",
            Self::Config => "CONFIG",
            Self::Feature => "FEATURE",
            Self::Milestone => "MILESTONE",
            Self::Progress => "PROGRESS",
            Self::CodePattern => "CODE_PATTERN",
            Self::Synthesis => "SYNTHESIS",
        }
    }

    /// 新鲜度半衰期（天）。None 表示不按时间衰减。
    pub fn half_life_days(self) -> Option<f64> {
        match self {
            Self::Decision | Self::Gotcha | Self::CodePattern => None,
            Self::Progress => Some(7.0),
            Self::Milestone => Some(30.0),
            Self::Feature | Self::Config | Self::Synthesis => Some(60.0),
            Self::Bugfix | Self::ErrorPattern => Some(90.0),
        }
    }
}

impl fmt::Display for MemoryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MemoryType {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let up = s.trim().to_ascii_uppercase().replace('-', "_");
        Self::ALL
            .into_iter()
            .find(|t| t.as_str() == up)
            .ok_or_else(|| anyhow!("未知记忆类型: {s}"))
    }
}

/// 写入请求。
#[derive(Debug, Clone, Default)]
pub struct NewMemory {
    pub mem_type: Option<MemoryType>,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub importance: Option<i64>,
    pub confidence: Option<f64>,
    /// `path` 或 `path#symbol`，相对项目根。
    pub anchors: Vec<String>,
    pub origin: Option<String>,
    pub legacy_id: Option<String>,
    pub status: Option<String>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    /// 跳过“相似记忆”检查（导入时或 Agent 已确认要新建时）。
    pub force: bool,
    /// 跳过准入规则（仅导入使用；脱敏仍然执行）。
    pub bypass_admission: bool,
    /// 锚点不记录当前哈希，漂移状态显示为 unverified（迁移来的记忆不知道当时的源码）。
    pub unverified_anchors: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Anchor {
    pub path: String,
    pub symbol: String,
    pub sha256_at_save: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Memory {
    pub id: String,
    pub mem_type: MemoryType,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub importance: i64,
    pub confidence: f64,
    pub status: String,
    pub origin: String,
    pub legacy_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Memory {
    pub(crate) const COLUMNS: &'static str = "id, type, title, body, tags, importance, confidence, status, origin, legacy_id, created_at, updated_at";

    pub(crate) fn from_row(r: &Row<'_>) -> rusqlite::Result<Self> {
        let ty: String = r.get(1)?;
        let tags: String = r.get(4)?;
        Ok(Memory {
            id: r.get(0)?,
            mem_type: ty.parse().unwrap_or(MemoryType::Synthesis),
            title: r.get(2)?,
            body: r.get(3)?,
            tags: serde_json::from_str(&tags).unwrap_or_default(),
            importance: r.get(5)?,
            confidence: r.get(6)?,
            status: r.get(7)?,
            origin: r.get(8)?,
            legacy_id: r.get(9)?,
            created_at: r.get(10)?,
            updated_at: r.get(11)?,
        })
    }
}

/// 写入结果。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RememberOutcome {
    Created { id: String, redactions: usize, notes: Vec<String> },
    /// 内容完全相同，未新建。
    Duplicate { id: String },
    /// 有高度相似的记忆，需要 Agent 决定（加 force 重试，或改为更新已有条目）。
    Similar { candidates: Vec<(String, String)> },
    Rejected { reason: String },
}

/// 代码漂移状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Drift {
    /// 锚点文件哈希与保存时一致，或没有锚点。
    Clean,
    /// 至少一个锚点文件已改动。
    Drift,
    /// 至少一个锚点文件已不存在。
    Orphaned,
    /// 有锚点但保存时没有哈希（例如迁移来的），无法判断。
    Unverified,
}

impl Store {
    /// 写入一条记忆：脱敏 → 准入 → 去重 → 相似检查 → 落库。
    pub fn remember(&self, req: NewMemory) -> Result<RememberOutcome> {
        let mem_type = req.mem_type.ok_or_else(|| anyhow!("缺少记忆类型"))?;
        let (title, r1) = redact(req.title.trim());
        let (body, r2) = redact(req.body.trim());
        let redactions = r1 + r2;

        let mut notes = Vec::new();
        if title.is_empty() || body.is_empty() {
            return Ok(RememberOutcome::Rejected { reason: "标题和正文都不能为空".into() });
        }
        if !req.bypass_admission {
            match admission::check(mem_type, &title, &body) {
                Verdict::Reject(reason) => return Ok(RememberOutcome::Rejected { reason }),
                Verdict::Accept(mut hints) => notes.append(&mut hints),
            }
        }

        let hash = content_hash(&title, &body);
        if let Some(id) = self
            .conn
            .query_row(
                "SELECT id FROM memories WHERE content_hash = ?1 AND status != 'deleted' LIMIT 1",
                params![hash],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(RememberOutcome::Duplicate { id });
        }

        if let Some(legacy) = &req.legacy_id {
            let exists: Option<String> = self
                .conn
                .query_row(
                    "SELECT id FROM memories WHERE legacy_id = ?1",
                    params![legacy],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = exists {
                return Ok(RememberOutcome::Duplicate { id });
            }
        }

        if !req.force {
            let candidates = self.similar_titles(&title, mem_type, 3)?;
            if !candidates.is_empty() {
                return Ok(RememberOutcome::Similar { candidates });
            }
        }

        let importance = req.importance.unwrap_or(2).clamp(1, 5);
        let confidence = req.confidence.unwrap_or(0.8).clamp(0.0, 1.0);
        let status = req.status.unwrap_or_else(|| "active".into());
        if !matches!(status.as_str(), "active" | "archived" | "superseded") {
            bail!("非法状态: {status}");
        }
        let origin = req.origin.unwrap_or_else(|| "agent".into());
        let now = now_ts();
        let created = req.created_at.unwrap_or(now);
        let updated = req.updated_at.unwrap_or(created);
        let tags: Vec<String> = req
            .tags
            .iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        let id = new_id();

        let provenance = if req.unverified_anchors { "inferred" } else { "agent" };
        self.savepoint(|tx| {
            tx.execute(
                "INSERT INTO memories (id, type, title, body, tags, importance, confidence, status, origin, legacy_id, created_at, updated_at, content_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    id,
                    mem_type.as_str(),
                    title,
                    body,
                    serde_json::to_string(&tags)?,
                    importance,
                    confidence,
                    status,
                    origin,
                    req.legacy_id,
                    created,
                    updated,
                    hash
                ],
            )?;
            for raw in &req.anchors {
                let (path, symbol) = split_anchor(raw);
                if path.is_empty() {
                    continue;
                }
                let sha = if req.unverified_anchors || self.root().is_none() {
                    None
                } else {
                    match self.file_sha(&path) {
                        Ok(Some(sha)) => Some(sha),
                        Ok(None) => { notes.push(format!("锚点目标不存在：{path}")); None }
                        Err(_) => { notes.push(format!("锚点未校验：{path}")); None }
                    }
                };
                tx.execute(
                    "INSERT OR IGNORE INTO anchors (memory_id, path, symbol, sha256_at_save) VALUES (?1, ?2, ?3, ?4)",
                    params![id, path, symbol, sha],
                )?;
                tx.execute(
                    "INSERT OR IGNORE INTO edges (src, dst, kind, provenance) VALUES (?1, ?2, 'anchors', ?3)",
                    params![format!("mem:{id}"), format!("file:{path}"), provenance],
                )?;
            }
            Ok(())
        })?;
        if redactions > 0 {
            notes.push(format!("已脱敏 {redactions} 处疑似凭据"));
        }
        Ok(RememberOutcome::Created { id, redactions, notes })
    }

    pub fn get(&self, id: &str) -> Result<Option<Memory>> {
        let sql = format!("SELECT {} FROM memories WHERE id = ?1 OR legacy_id = ?1", Memory::COLUMNS);
        Ok(self.conn.query_row(&sql, params![id], Memory::from_row).optional()?)
    }

    /// 一条记忆的完整文本：类型、标题、状态、重要度、漂移、标签、正文和锚点。
    /// 会话和命令行按编号取全文时共用这一份；编号不存在时返回 None。
    pub fn memory_detail(&self, id: &str) -> Result<Option<String>> {
        use std::fmt::Write as _;
        let Some(m) = self.get(id)? else {
            return Ok(None);
        };
        let mut out = String::new();
        let _ = writeln!(
            out,
            "{} {} · id={} · {} · 重要度 {} · 漂移 {:?}\n标签：{}\n{}",
            m.mem_type,
            m.title,
            m.id,
            m.status,
            m.importance,
            self.drift(&m.id)?,
            m.tags.join(", "),
            m.body
        );
        for an in self.anchors(&m.id)? {
            let sym = if an.symbol.is_empty() {
                String::new()
            } else {
                format!("#{}", an.symbol)
            };
            let _ = writeln!(out, "↳ {}{}", an.path, sym);
        }
        Ok(Some(out))
    }

    pub fn anchors(&self, id: &str) -> Result<Vec<Anchor>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT path, symbol, sha256_at_save FROM anchors WHERE memory_id = ?1 ORDER BY path")?;
        let rows = st.query_map(params![id], |r| {
            Ok(Anchor { path: r.get(0)?, symbol: r.get(1)?, sha256_at_save: r.get(2)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 没有项目根的临时库无法验证锚点，不能报告为一致。
    pub fn drift(&self, id: &str) -> Result<Drift> {
        let anchors = self.anchors(id)?;
        if self.root().is_none() && !anchors.is_empty() {
            return Ok(Drift::Unverified);
        }
        let mut state = Drift::Clean;
        for a in anchors {
            let s = match (&a.sha256_at_save, self.file_sha(&a.path)) {
                (_, Err(_)) => Drift::Unverified,
                (_, Ok(None)) => Drift::Orphaned,
                (None, Ok(Some(_))) => Drift::Unverified,
                (Some(old), Ok(Some(new))) if *old != new => Drift::Drift,
                _ => Drift::Clean,
            };
            state = worse(state, s);
        }
        Ok(state)
    }

    /// 修改状态：archived / active / superseded。
    pub fn set_status(&self, ids: &[String], status: &str) -> Result<usize> {
        if !matches!(status, "active" | "archived" | "superseded") {
            bail!("非法状态: {status}");
        }
        let now = now_ts();
        let mut n = 0;
        for id in ids {
            n += self.conn.execute(
                "UPDATE memories SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status, now, id],
            )?;
        }
        Ok(n)
    }

    /// `new_id` 取代 `old_id`：旧条目标为 superseded，并记一条边。
    pub fn supersede(&self, old_id: &str, new_id: &str) -> Result<()> {
        // 取代自身只会把这条记忆标成已取代，却没有任何结论接替它。
        if old_id == new_id {
            bail!("记忆不能取代自身: {old_id}");
        }
        if self.get(new_id)?.is_none() {
            bail!("新记忆不存在: {new_id}");
        }
        if self.set_status(&[old_id.to_string()], "superseded")? == 0 {
            bail!("旧记忆不存在: {old_id}");
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO edges (src, dst, kind, provenance) VALUES (?1, ?2, 'supersedes', 'agent')",
            params![format!("mem:{new_id}"), format!("mem:{old_id}")],
        )?;
        Ok(())
    }

    /// 物理删除（连同锚点、FTS 条目和相关边）。
    pub fn delete(&self, ids: &[String]) -> Result<usize> {
        self.savepoint(|tx| {
            let mut n = 0;
            for id in ids {
                let key = format!("mem:{id}");
                tx.execute("DELETE FROM edges WHERE src = ?1 OR dst = ?1", params![key])?;
                n += tx.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
            }
            Ok(n)
        })
    }

    /// 最近更新的记忆。
    pub fn recent(&self, since_ts: i64, limit: usize) -> Result<Vec<Memory>> {
        let sql = format!(
            "SELECT {} FROM memories WHERE status = 'active' AND updated_at >= ?1 ORDER BY updated_at DESC LIMIT ?2",
            Memory::COLUMNS
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params![since_ts, limit as i64], Memory::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 按状态、类型统计。
    pub fn stats(&self) -> Result<Vec<(String, String, i64)>> {
        let mut st = self.conn.prepare(
            "SELECT status, type, COUNT(*) FROM memories GROUP BY status, type ORDER BY status, type",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn count(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))?)
    }

    /// 只给根内的普通文件算指纹。只有目标不存在才返回空，其他错误不能误报为删除。
    fn file_sha(&self, rel: &str) -> std::io::Result<Option<String>> {
        use sha2::{Digest, Sha256};
        use std::io::{Error, ErrorKind};
        use std::path::{Component, Path};
        let root = self.root().ok_or_else(|| Error::other("无绑定根目录"))?;
        let relative = Path::new(rel);
        if !relative.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)) {
            return Err(Error::other("锚点必须是项目内的相对路径"));
        }
        let target = match root.join(relative).canonicalize() {
            Ok(path) => path,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if !target.starts_with(root) || !target.metadata()?.is_file() {
            return Err(Error::other("不是项目内的普通文件"));
        }
        let mut input = std::fs::File::open(target)?;
        let mut hash = Sha256::new();
        std::io::copy(&mut input, &mut hash)?;
        Ok(Some(hex::encode(hash.finalize())))
    }
}

fn worse(a: Drift, b: Drift) -> Drift {
    let rank = |d: Drift| match d {
        Drift::Clean => 0,
        Drift::Unverified => 1,
        Drift::Drift => 2,
        Drift::Orphaned => 3,
    };
    if rank(b) > rank(a) { b } else { a }
}

/// `src/a.rs#Foo::bar` → (`src/a.rs`, `Foo::bar`)；路径统一成 `/` 分隔，去掉 `file:` 前缀和 `./`。
pub fn split_anchor(raw: &str) -> (String, String) {
    let raw = raw.trim().trim_start_matches("file:");
    let (p, s) = raw.split_once('#').unwrap_or((raw, ""));
    let p = p.replace('\\', "/");
    let p = p.trim_start_matches("./").to_string();
    (p, s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(ty: MemoryType, title: &str, body: &str) -> NewMemory {
        NewMemory { mem_type: Some(ty), title: title.into(), body: body.into(), ..Default::default() }
    }

    #[test]
    fn parses_types_case_insensitive() {
        assert_eq!("gotcha".parse::<MemoryType>().unwrap(), MemoryType::Gotcha);
        assert_eq!("error-pattern".parse::<MemoryType>().unwrap(), MemoryType::ErrorPattern);
        assert!("nope".parse::<MemoryType>().is_err());
    }

    #[test]
    fn remember_dedups_exact_content() {
        let s = Store::open_in_memory().unwrap();
        let body = "Windows 下 PowerShell 不认 /dev/null，要写 $null。";
        let a = s.remember(req(MemoryType::Gotcha, "PowerShell 空设备", body)).unwrap();
        let RememberOutcome::Created { id, .. } = a else { panic!("{a:?}") };
        let b = s.remember(req(MemoryType::Gotcha, "PowerShell  空设备", body)).unwrap();
        assert!(matches!(b, RememberOutcome::Duplicate { id: ref d } if *d == id), "{b:?}");
    }

    #[test]
    fn remember_redacts_secrets() {
        let s = Store::open_in_memory().unwrap();
        let out = s
            .remember(req(MemoryType::Config, "数据库连接", "连接串 bolt://neo4j:{{PASSWORD_ddlkudhm}}@localhost:7687 用于测试环境"))
            .unwrap();
        let RememberOutcome::Created { id, redactions, .. } = out else { panic!("{out:?}") };
        assert_eq!(redactions, 1);
        assert!(!s.get(&id).unwrap().unwrap().body.contains("{{PASSWORD_ddlkudhm}}"));
    }

    #[test]
    fn similar_title_needs_force() {
        let s = Store::open_in_memory().unwrap();
        s.remember(req(MemoryType::Bugfix, "修复 cube 自动注册串行导致启动慢", "改为并发注册，启动从 40s 降到 6s。")).unwrap();
        let second = s
            .remember(req(MemoryType::Bugfix, "修复 cube 自动注册串行导致启动慢的问题", "另一种说法：并发注册。"))
            .unwrap();
        assert!(matches!(second, RememberOutcome::Similar { .. }), "{second:?}");
        let mut forced = req(MemoryType::Bugfix, "修复 cube 自动注册串行导致启动慢的问题", "另一种说法：并发注册。");
        forced.force = true;
        assert!(matches!(s.remember(forced).unwrap(), RememberOutcome::Created { .. }));
    }

    #[test]
    fn drift_detects_file_change_and_removal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "fn a() {}").unwrap();
        let project = crate::project::ProjectLocation::select(dir.path(), Some(dir.path()), None).unwrap();
        project.bind(crate::project::Bind::Initialize).unwrap();
        let s = project.open(true).unwrap();
        let mut r = req(MemoryType::Gotcha, "a 函数不能改签名", "外部插件通过 FFI 按名字调用 a，改签名会导致运行时崩溃。");
        r.anchors = vec!["./src/a.rs#a".into()];
        let RememberOutcome::Created { id, .. } = s.remember(r).unwrap() else { panic!() };
        assert_eq!(s.drift(&id).unwrap(), Drift::Clean);
        std::fs::write(dir.path().join("src/a.rs"), "fn a(x: i32) {}").unwrap();
        assert_eq!(s.drift(&id).unwrap(), Drift::Drift);
        std::fs::remove_file(dir.path().join("src/a.rs")).unwrap();
        assert_eq!(s.drift(&id).unwrap(), Drift::Orphaned);
    }

    #[test]
    fn uncheckable_anchor_is_not_a_clean_or_deleted_file() {
        let dir = tempfile::tempdir().unwrap();
        let project = crate::project::ProjectLocation::select(dir.path(), Some(dir.path()), None).unwrap();
        project.bind(crate::project::Bind::Initialize).unwrap();
        std::fs::create_dir(dir.path().join("模块")).unwrap();
        for s in [project.open(true).unwrap(), Store::open_in_memory().unwrap()] {
            let mut r = req(MemoryType::Gotcha, "核对范围", "来源不是可核对的普通文件，因此不能断言它已删除。");
            r.anchors = vec!["模块".into()];
            let RememberOutcome::Created { id, .. } = s.remember(r).unwrap() else { panic!() };
            assert_eq!(s.drift(&id).unwrap(), Drift::Unverified);
        }
    }

    #[test]
    fn delete_cleans_fts_and_edges() {
        let s = Store::open_in_memory().unwrap();
        let mut r = req(MemoryType::Decision, "选择 SQLite 作为唯一存储", "替代 Neo4j 与 Qdrant，降低内存占用到百兆以内。");
        r.anchors = vec!["Cargo.toml".into()];
        let RememberOutcome::Created { id, .. } = s.remember(r).unwrap() else { panic!() };
        assert_eq!(s.delete(std::slice::from_ref(&id)).unwrap(), 1);
        let fts: i64 = s.conn.query_row("SELECT COUNT(*) FROM fts", [], |r| r.get(0)).unwrap();
        let edges: i64 = s.conn.query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0)).unwrap();
        assert_eq!((fts, edges), (0, 0));
    }

    #[test]
    fn detail_carries_the_full_text_and_a_memory_cannot_supersede_itself() {
        let s = Store::open_in_memory().unwrap();
        let mut r = req(
            MemoryType::Decision,
            "选择 SQLite 作为唯一存储",
            "替代 Neo4j 与 Qdrant，降低内存占用到百兆以内。",
        );
        r.anchors = vec!["Cargo.toml#deps".into()];
        r.tags = vec!["存储".into()];
        let RememberOutcome::Created { id, .. } = s.remember(r).unwrap() else {
            panic!()
        };
        let detail = s.memory_detail(&id).unwrap().unwrap();
        assert!(
            detail.starts_with("DECISION 选择 SQLite 作为唯一存储 · id="),
            "{detail}"
        );
        for part in [
            id.as_str(),
            " · active · ",
            "标签：存储\n",
            "降低内存占用到百兆以内。\n",
            "↳ Cargo.toml#deps\n",
        ] {
            assert!(detail.contains(part), "{part}: {detail}");
        }
        assert!(s.memory_detail("absent").unwrap().is_none());
        assert!(s.supersede(&id, &id).is_err());
        assert_eq!(s.get(&id).unwrap().unwrap().status, "active");
    }
}
