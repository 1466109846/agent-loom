//! 持久库先核对归属，再交给记忆模块。项目目录只保存一个不含业务数据的标识。

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};

use crate::Store;
use crate::db::{APPLICATION_ID, SCHEMA_V1, SCHEMA_VERSION, create_identity_schema};
use crate::util::{atomic_write, display_path, new_id};

mod movement;
pub use movement::MoveReport;

const MARKER: &str = ".loom/project.id";
const POINTER: &str = ".loom/database.path";
const MAX_PATH: usize = 4096;
const DEFAULT_DB: &str = ".loom/loom.db";
const MISMATCH: &str = "项目身份不符；请核对所选根目录和库，未加载项目内容";

/// 这个位置还没有接入过 Loom。与库丢失、身份不符分开：会话服务据此引导助手去接入，而不是去修。
#[derive(Debug, thiserror::Error)]
pub enum Unbound {
    #[error("未找到已绑定项目；请明确指定 --root，首次使用需执行 init")]
    NoProject,
    #[error("项目库未初始化：{0}；首次绑定使用 init")]
    NoDatabase(String),
}

/// 用户写的 `..` 和混用的分隔符先规整掉，记下来的和显示出来的才是同一个干净的路径。不解析链接。
fn tidy(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProjectIdentity {
    id: String,
    root: PathBuf,
}

impl ProjectIdentity {
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn id(&self) -> &str {
        &self.id
    }

    /// 长寿命会话刷新结构前重新核对根目录，避免目录搬走后误读原址的新项目。
    pub(crate) fn verify_live_root(&self) -> Result<()> {
        ensure!(
            self.root.canonicalize()? == self.root
                && marker(&self.root)?.as_deref() == Some(self.id()),
            MISMATCH
        );
        Ok(())
    }

    pub fn heading(&self) -> String {
        let label = self
            .root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("根目录");
        format!(
            "项目：{label} · 标识={} · 根={}",
            self.id,
            self.root.display()
        )
    }
}

/// 只有显式的本地初始化操作可以建立、认领或搬迁绑定。
pub enum Bind<'a> {
    Initialize,
    AdoptLegacy,
    Relocate(&'a str),
}

pub struct ProjectLocation {
    root: PathBuf,
    db: PathBuf,
    explicit: bool,
}

enum DiskState {
    Legacy,
    Bound(ProjectIdentity),
}

fn entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn metadata_file(root: &Path, name: &str, limit: usize) -> Result<Option<fs::File>> {
    let dir = root.join(".loom");
    if entry_exists(&dir)? {
        ensure!(
            dir.canonicalize()? == dir && fs::symlink_metadata(&dir)?.is_dir(),
            "项目元数据目录不能指向其他位置"
        );
    }
    let path = root.join(name);
    if !entry_exists(&path)? {
        return Ok(None);
    }
    let meta = fs::symlink_metadata(&path)?;
    ensure!(
        meta.is_file() && meta.len() <= limit as u64 && path.canonicalize()? == path,
        "{name} 不是有效的小型普通文件"
    );
    Ok(Some(fs::File::open(path)?))
}

fn metadata_text(root: &Path, name: &str, limit: usize) -> Result<Option<String>> {
    let Some(file) = metadata_file(root, name, limit)? else {
        return Ok(None);
    };
    let mut text = String::new();
    file.take(limit as u64 + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("{name} 须为 UTF-8"))?;
    ensure!(text.len() <= limit, "{name} 超出大小限制");
    Ok(Some(text))
}

fn marker(root: &Path) -> Result<Option<String>> {
    let Some(text) = metadata_text(root, MARKER, 64)? else {
        return Ok(None);
    };
    let id = text.trim();
    ensure!(
        ulid::Ulid::from_string(id).is_ok(),
        "项目标记损坏，不会自动替换"
    );
    Ok(Some(id.to_owned()))
}

fn pointer_path(path: &Path) -> Result<&str> {
    let s = path.to_str().context("库路径须为 UTF-8")?;
    ensure!(
        path.is_absolute() && s.len() <= MAX_PATH && !s.contains(char::is_control),
        "database.path 须是单行、不超过 4096 字节的绝对路径"
    );
    Ok(s)
}

fn pointer(root: &Path) -> Result<Option<PathBuf>> {
    let Some(text) = metadata_text(root, POINTER, MAX_PATH)? else {
        return Ok(None);
    };
    let s = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(&text);
    pointer_path(Path::new(s))?;
    Ok(Some(s.into()))
}

fn connect(path: &Path, writable: bool) -> Result<Connection> {
    // 特意不传创建标志：普通查询、握手和导入预演都不能建库。
    let mode = if writable {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let conn = Connection::open_with_flags(path, mode | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .context("无法打开指定的本地库")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

/// 只探查已落盘的身份页，不加入其他项目的日志协调，也不创建旁文件。
/// 探查不是授权：随后必须在实际连接上再次核对，业务读取不使用这个快照。
fn probe(path: &Path) -> Result<DiskState> {
    use std::fmt::Write as _;
    let path = path.canonicalize().context("项目库不可访问")?;
    ensure!(path.is_file(), "项目库必须是普通文件");
    let name = path.to_str().context("无法无损表示库文件位置")?;
    #[cfg(windows)]
    let name = {
        let plain = if let Some(unc) = name.strip_prefix(r"\\?\UNC\") {
            format!("//{unc}")
        } else {
            name.strip_prefix(r"\\?\").unwrap_or(name).to_owned()
        };
        plain.replace('\\', "/")
    };
    let mut uri = String::from("file:");
    if !name.starts_with('/') {
        uri.push('/');
    }
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~:".contains(&b) {
            uri.push(char::from(b));
        } else {
            write!(uri, "%{b:02X}")?;
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    let conn = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    inspect(&conn)
}

fn inspect(conn: &Connection) -> Result<DiskState> {
    let app: i32 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if app == APPLICATION_ID && version == SCHEMA_VERSION {
        let (id, root): (String, String) = conn
            .query_row(
                "SELECT project_id, canonical_root FROM loom_identity WHERE singleton = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .context("项目身份记录缺失或损坏，未加载项目内容")?;
        ensure!(
            ulid::Ulid::from_string(&id).is_ok() && Path::new(&root).is_absolute(),
            "项目身份记录无效"
        );
        return Ok(DiskState::Bound(ProjectIdentity {
            id,
            root: root.into(),
        }));
    }
    // 只查结构，不读取、输出旧库里的任何记忆。版本号相同不足以认定为旧库。
    if app == 0 && version == 1 {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name IN ('files','symbols','file_notes','intents','memories','anchors','edges','fts','vectors','canvases')", [], |r| r.get(0),
        )?;
        if n == 10 && conn.prepare("SELECT id,type,title,body,tags,importance,confidence,status,origin,legacy_id,created_at,updated_at,content_hash FROM memories LIMIT 0").is_ok() {
            return Ok(DiskState::Legacy);
        }
    }
    bail!("不是可识别的 Loom 库或版本不受支持，未修改");
}

impl ProjectLocation {
    /// 显式根优先，否则使用最近的本地项目标记。遇到未认领的旧库不越过它找父项目。
    pub fn select(cwd: &Path, root: Option<&Path>, db: Option<&Path>) -> Result<Self> {
        let root = if let Some(r) = root {
            cwd.join(r)
                .canonicalize()
                .context("项目根目录不存在或不可访问")?
        } else {
            let start = cwd.canonicalize().context("无法确定启动目录")?;
            let mut candidate = None;
            for parent in start.ancestors() {
                if entry_exists(&parent.join(MARKER))? || entry_exists(&parent.join(DEFAULT_DB))? {
                    candidate = Some(parent.to_path_buf());
                    break;
                }
            }
            candidate.ok_or(Unbound::NoProject)?
        };
        ensure!(root.is_dir(), "项目根必须是目录");
        ensure!(root.to_str().is_some(), "项目根无法无损保存为文本");
        let explicit = db.is_some();
        let db = match db {
            Some(path) => tidy(cwd.join(path)),
            None => pointer(&root)?.unwrap_or_else(|| root.join(DEFAULT_DB)),
        };
        Ok(Self { root, db, explicit })
    }

    pub fn database(&self) -> &Path {
        &self.db
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn marker(&self) -> Result<Option<String>> {
        marker(&self.root)
    }

    fn update_pointer(&self, db: &Path) -> Result<()> {
        // 分享不接受带 .. 的位置；绑定完成后再解析，可正确处理穿过链接的上级目录。
        let normalized;
        let db = if db
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            normalized = db.canonicalize()?;
            normalized.as_path()
        } else {
            db
        };
        let current = pointer(&self.root)?;
        let default = self.root.join(DEFAULT_DB);
        let path = self.root.join(POINTER);
        if db == default
            || (entry_exists(&default)? && db.canonicalize()? == default.canonicalize()?)
        {
            if current.is_some() {
                fs::remove_file(path)?;
            }
        } else if current.as_deref() != Some(db) {
            atomic_write(&path, pointer_path(db)?.as_bytes())?;
        }
        Ok(())
    }

    fn missing(&self) -> anyhow::Error {
        anyhow::anyhow!(
            "项目库不在 {}；若已手工移动，执行 loom --root \"{}\" init --db <新位置> 重新关联",
            display_path(&self.db),
            display_path(&self.root)
        )
    }

    fn probe_bound(&self) -> Result<ProjectIdentity> {
        let marker = self.marker()?;
        if !entry_exists(&self.db)? {
            if marker.is_some() {
                return Err(self.missing());
            }
            return Err(Unbound::NoDatabase(display_path(&self.db)).into());
        }
        self.verify(probe(&self.db)?, marker.as_deref())
    }

    fn ensure_marker(&self, id: &str) -> Result<()> {
        if let Some(found) = self.marker()? {
            ensure!(found == id, MISMATCH);
            return Ok(());
        }
        fs::create_dir_all(self.root.join(".loom"))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.root.join(MARKER))
            .context("未能新建项目标记；不会覆盖已有标记，可核对后重试 init")?;
        writeln!(file, "{id}")?;
        file.sync_all()?;
        Ok(())
    }

    fn verify(&self, state: DiskState, marker: Option<&str>) -> Result<ProjectIdentity> {
        match state {
            DiskState::Legacy => bail!("旧库尚未绑定；请核对归属后显式执行 init --adopt-legacy"),
            DiskState::Bound(identity) => {
                ensure!(identity.root == self.root, MISMATCH);
                let id = marker.context("根目录没有项目标记；请核对归属后显式执行 init")?;
                ensure!(id == identity.id, MISMATCH);
                Ok(identity)
            }
        }
    }

    /// 先用只读连接检查身份。需要写入时重新打开同一库，再次核对后才交付连接。
    pub fn open(&self, writable: bool) -> Result<Store> {
        let expected = self.probe_bound()?;
        let conn = connect(&self.db, writable)?;
        let check = self.verify(inspect(&conn)?, self.marker()?.as_deref())?;
        ensure!(check == expected, MISMATCH);
        if !self.explicit {
            // 若建连时正等在搬库的 SQLite 锁上，不能把刚刚淘汰的旧连接交给会话。
            ensure!(
                pointer(&self.root)?.unwrap_or_else(|| self.root.join(DEFAULT_DB)) == self.db,
                "库位置已改变，请重试"
            );
        }
        Store::from_connection(conn, Some(expected))
    }

    fn authorize(&self, state: &DiskState, marker: Option<&str>, mode: &Bind<'_>) -> Result<()> {
        match state {
            DiskState::Legacy => {
                ensure!(marker.is_none(), MISMATCH);
                ensure!(
                    matches!(mode, Bind::AdoptLegacy),
                    "旧库尚未绑定；请核对归属后显式执行 init --adopt-legacy"
                );
            }
            DiskState::Bound(p) => {
                ensure!(marker.is_none_or(|id| id == p.id), MISMATCH);
                if let Bind::Relocate(id) = mode {
                    ensure!(*id == p.id, MISMATCH);
                }
                if p.root != self.root {
                    ensure!(matches!(mode, Bind::Relocate(id) if *id == p.id), MISMATCH);
                    ensure!(
                        !entry_exists(&p.root)?,
                        "原根目录仍存在，不能把副本自动当作搬迁后的项目"
                    );
                }
            }
        }
        Ok(())
    }

    fn binding_preflight(&self, mode: &Bind<'_>) -> Result<Option<DiskState>> {
        let marker = self.marker()?;
        let existing = entry_exists(&self.db)?;
        let default_db = self.root.join(DEFAULT_DB);
        if marker.is_none() && entry_exists(&default_db)? {
            ensure!(
                existing && self.db.canonicalize()? == default_db.canonicalize()?,
                "根目录已有尚待确认的项目库，不能创建替代库"
            );
        }
        if existing {
            let state = probe(&self.db)?;
            self.authorize(&state, marker.as_deref(), mode)?;
            Ok(Some(state))
        } else {
            if marker.is_some() || pointer(&self.root)?.is_some() {
                return Err(self.missing());
            }
            ensure!(matches!(mode, Bind::Initialize), "没有可认领或搬迁的现有库");
            Ok(None)
        }
    }

    fn binding_lock(&self) -> Result<fs::File> {
        let meta = self.root.join(".loom");
        fs::create_dir_all(&meta)?;
        let lock_path = meta.join("bind.lock");
        if entry_exists(&lock_path)? {
            ensure!(
                fs::symlink_metadata(&lock_path)?.is_file()
                    && lock_path.canonicalize()? == lock_path,
                "绑定锁文件位置无效"
            );
        }
        let guard = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        guard
            .try_lock()
            .context("另一个绑定操作正在进行，未修改项目库，请稍后重试")?;
        Ok(guard)
    }

    pub fn bind(&self, mode: Bind<'_>) -> Result<ProjectIdentity> {
        self.bind_with_data_dir(mode, None)
    }

    /// 只在确实要新建默认库时才需要调用方读取本机配置。
    pub fn needs_data_dir(&self) -> Result<bool> {
        Ok(!self.explicit
            && !entry_exists(&self.db)?
            && self.marker()?.is_none()
            && pointer(&self.root)?.is_none())
    }

    pub fn bind_with_data_dir(
        &self,
        mode: Bind<'_>,
        data_dir: Option<&Path>,
    ) -> Result<ProjectIdentity> {
        // 错误请求不创建锁文件；锁内重新选择，以免采用另一个绑定或搬库操作之前的位置。
        self.binding_preflight(&mode)?;
        let _guard = self.binding_lock()?;
        let mut location = Self::select(
            &self.root,
            Some(&self.root),
            self.explicit.then_some(&self.db),
        )?;
        let plan = location.binding_preflight(&mode)?;
        let fresh_id = plan.is_none().then(new_id);
        if let (Some(id), Some(dir)) = (&fresh_id, data_dir)
            && !self.explicit
        {
            ensure!(dir.is_absolute(), "storage.data_dir 须是绝对路径");
            location.db = tidy(dir.join(id).join("loom.db"));
        }
        pointer_path(&location.db)?;
        location.check_reserved(&location.db)?;
        pointer(&self.root)?;
        location.bind_locked(mode, plan, fresh_id)
    }

    fn bind_locked(
        &self,
        mode: Bind<'_>,
        plan: Option<DiskState>,
        fresh_id: Option<String>,
    ) -> Result<ProjectIdentity> {
        if let Some(DiskState::Bound(p)) = &plan
            && p.root == self.root
        {
            let current = inspect(&connect(&self.db, false)?)?;
            ensure!(
                matches!(current, DiskState::Bound(ref live) if live == p),
                MISMATCH
            );
            self.ensure_marker(&p.id)?;
            self.update_pointer(&self.db)?;
            return Ok(p.clone());
        }
        if plan.is_none() {
            if let Some(parent) = self.db.parent() {
                fs::create_dir_all(parent)?;
            }
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.db)
                .context("未能独占新建项目库；不会覆盖已有库")?;
        }
        let mut writer = connect(&self.db, true)?;
        let tx = writer.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = if plan.is_some() {
            let state = inspect(&tx)?;
            self.authorize(&state, self.marker()?.as_deref(), &mode)?;
            match state {
                DiskState::Legacy => new_id(),
                DiskState::Bound(p) => p.id,
            }
        } else {
            tx.execute_batch(SCHEMA_V1)?;
            fresh_id.expect("新库路径与身份一起分配")
        };
        create_identity_schema(&tx)?;
        let changed = tx.execute(
            "INSERT INTO loom_identity(singleton,project_id,canonical_root) VALUES(1,?1,?2) ON CONFLICT(singleton) DO UPDATE SET canonical_root=excluded.canonical_root WHERE project_id=excluded.project_id",
            params![id, self.root.to_str().context("根目录编码无效")?],
        )?;
        ensure!(changed == 1, MISMATCH);
        tx.commit()?;
        // 身份要先落到主库，后续才能在不碰日志文件的情况下做只读预检。
        checkpoint_wal(&writer)?;
        // 若进程恰在两者间停止，普通打开仍然拒绝；确认同一根后 init 可补回标记。
        self.update_pointer(&self.db)?;
        self.ensure_marker(&id)?;
        Ok(ProjectIdentity {
            id,
            root: self.root.clone(),
        })
    }
}

fn checkpoint_wal(conn: &Connection) -> Result<()> {
    let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    ensure!(mode == "wal", "项目库未能启用 WAL");
    let (busy, frames, saved): (i32, i32, i32) =
        conn.query_row("PRAGMA wal_checkpoint(FULL)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
    ensure!(
        busy == 0 && frames == saved,
        "身份已写入但仍待落盘，请关闭其他会话后重试"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
