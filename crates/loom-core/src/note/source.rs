//! 只沿目标的祖先重走 scan 的准入规则，不扫描兄弟目录，不读取未被快照授权的源码。
use std::{
    collections::BTreeMap,
    fs::{self, File, Metadata, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};

use crate::{
    redact::sensitive,
    structure::scope::{self, MAX_BYTES, MAX_DEPTH, Scope, metadata, redirected},
    util::sha256_hex,
};
use anyhow::{Context, Result, ensure};

pub(crate) enum Live {
    Read(String),
    Missing,
    Unavailable(&'static str),
}

pub(super) fn key(raw: &str) -> Result<String> {
    ensure!(
        raw.len() <= 4096
            && !raw.starts_with(['/', '\\'])
            && !raw.contains(':')
            && !raw.chars().any(char::is_control),
        "需要项目内的相对源码路径"
    );
    let mut parts = Vec::new();
    for part in raw.split(['/', '\\']) {
        match part {
            "" | "." => continue,
            ".." => anyhow::bail!("相对源码路径不能含上级目录"),
            _ => parts.push(part),
        }
    }
    ensure!(
        !parts.is_empty() && parts.len() <= MAX_DEPTH + 1,
        "源码路径为空或过深"
    );
    let key = parts.join("/");
    // 路径也是定位键。不能一边返回脱敏的假路径，一边把带凭据的原路径当作人工锚点。
    ensure!(!sensitive(&key), "源码路径含凭据信息，不能用作职责定位键");
    Ok(key)
}

// 先验路径及类型，再验真正打开的句柄，最后才允许读内容。Windows 上固定祖先目录防止被换成联接。
fn open_checked(path: &Path, before: &Metadata, dir: bool) -> Result<File> {
    let allowed = |m: &Metadata| !redirected(m) && if dir { m.is_dir() } else { m.is_file() };
    ensure!(
        allowed(before) && path.canonicalize()? == path,
        "路径不可安全核对"
    );
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const BACKUP_SEMANTICS: u32 = 0x0200_0000;
        // 不共享删除；源文件和规则在读取期间也不共享写入。
        opts.custom_flags(OPEN_REPARSE_POINT | if dir { BACKUP_SEMANTICS } else { 0 })
            .share_mode(if dir { 3 } else { 1 });
    }
    let file = opts.open(path)?;
    let opened = file.metadata()?;
    ensure!(
        allowed(&opened) && path.canonicalize()? == path,
        "打开时路径已改变"
    );
    ensure!(
        dir || super::links::single_link(&file)?,
        "具有多个路径的文件不能用作依据"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            (before.dev(), before.ino()) == (opened.dev(), opened.ino()),
            "打开时目标已更换"
        );
    }
    Ok(file)
}

fn read_limited(path: &Path, m: &Metadata, limit: u64) -> Result<Vec<u8>> {
    ensure!(m.len() <= limit, "文件超过读取上限");
    let file = open_checked(path, m, false)?;
    ensure!(file.metadata()?.len() <= limit, "文件已变大");
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "读取期间文件超限");
    Ok(bytes)
}

pub(super) fn observe(
    project: &crate::project::ProjectIdentity,
    database: Option<&str>,
    key: &str,
    parsed: bool,
) -> Live {
    Inspector::new(project, database)
        .map(|mut i| i.file(key, ReadAs::Note(parsed)))
        .unwrap_or(Live::Unavailable("路径、权限或忽略规则不可安全核对"))
}

pub(crate) enum ReadAs {
    Note(bool),
    Digest,
    Presence,
}

#[derive(Clone, Copy)]
enum Block {
    Missing,
    Unavailable(&'static str),
}
impl Block {
    fn live(self) -> Live {
        match self {
            Self::Missing => Live::Missing,
            Self::Unavailable(reason) => Live::Unavailable(reason),
        }
    }
}

pub(crate) struct Inspector<'a> {
    root: &'a Path,
    db: PathBuf,
    guards: Vec<File>,
    base: Scope,
    dirs: BTreeMap<String, Result<Scope, Block>>,
    rules: BTreeMap<String, String>,
}

impl<'a> Inspector<'a> {
    pub(crate) fn new(
        project: &'a crate::project::ProjectIdentity,
        database: Option<&str>,
    ) -> Result<Self> {
        let root = project.root();
        let db = Path::new(database.context("项目库没有磁盘位置")?).canonicalize()?;
        let mut guards = vec![open_checked(root, &fs::symlink_metadata(root)?, true)?];
        let admin = root.join(".loom");
        guards.push(open_checked(&admin, &fs::symlink_metadata(&admin)?, true)?);
        let marker = admin.join("project.id");
        guards.push(open_checked(
            &marker,
            &fs::symlink_metadata(&marker)?,
            false,
        )?);
        // 再次核对已固定的根与标记，然后才接触规则/源码，避免长寿命连接误读原址的新项目。
        project.verify_live_root()?;
        let base = Scope::new(root, read_limited)?;
        Ok(Self {
            root,
            db,
            guards,
            base,
            dirs: BTreeMap::new(),
            rules: BTreeMap::new(),
        })
    }

    pub(crate) fn scope(&self) -> &str {
        self.base.name()
    }

    pub(crate) fn base_rules(&self) -> impl Iterator<Item = (&str, &str)> {
        self.base
            .base_rules
            .iter()
            .map(|(p, h)| (p.as_str(), h.as_str()))
    }

    fn directory(&mut self, dir: &str) -> Result<&Scope, Block> {
        if !self.dirs.contains_key(dir) {
            let state = self
                .enter(dir)
                .unwrap_or(Err(Block::Unavailable("路径、权限或忽略规则不可安全核对")));
            self.dirs.insert(dir.to_owned(), state);
        }
        self.dirs[dir].as_ref().map_err(|e| *e)
    }

    fn enter(&mut self, dir: &str) -> Result<Result<Scope, Block>> {
        let path = self.root.join(dir);
        if path == self.db {
            return Ok(Err(Block::Unavailable("不在源码范围")));
        }
        let mut scope = if dir.is_empty() {
            self.base.clone()
        } else {
            let (parent, name) = dir.rsplit_once('/').unwrap_or(("", dir));
            let scope = match self.directory(parent) {
                Ok(s) => s,
                Err(block) => return Ok(Err(block)),
            };
            if scope.excluded(&path, name, true) {
                return Ok(Err(Block::Unavailable("当前忽略规则排除此路径")));
            }
            let Some(m) = metadata(&path)? else {
                return Ok(Err(Block::Missing));
            };
            if redirected(&m) || !m.is_dir() {
                return Ok(Err(Block::Unavailable("链接、重定向或目标类型改变")));
            }
            let guard = open_checked(&path, &m, true)?;
            if metadata(&path.join(".loom"))?.is_some() {
                return Ok(Err(Block::Unavailable("路径已属于嵌套项目")));
            }
            for marker in scope.markers() {
                if metadata(&path.join(marker))?.is_some_and(|m| m.is_file()) {
                    return Ok(Err(Block::Unavailable("虚拟环境或缓存目录")));
                }
            }
            let branch = scope.clone();
            self.guards.push(guard);
            branch
        };
        if let (_, Some(hash)) = scope.enter(&path)? {
            self.rules.insert(
                if dir.is_empty() {
                    ".gitignore".into()
                } else {
                    format!("{dir}/.gitignore")
                },
                hash,
            );
        }
        Ok(Ok(scope))
    }

    pub(crate) fn rule(&mut self, path: &str) -> Live {
        if path.rsplit('/').next() != Some(".gitignore") {
            return self
                .base_rules()
                .find(|(p, _)| *p == path)
                .map_or(Live::Missing, |(_, h)| Live::Read(h.to_owned()));
        }
        if !key(path).is_ok_and(|p| p == path) {
            return Live::Unavailable("路径、权限或忽略规则不可安全核对");
        }
        if let Err(b) = self.directory(path.rsplit_once('/').map_or("", |(d, _)| d)) {
            return b.live();
        }
        self.rules
            .get(path)
            .cloned()
            .map_or(Live::Missing, Live::Read)
    }

    pub(crate) fn file(&mut self, path: &str, mode: ReadAs) -> Live {
        self.inspect(path, mode)
            .unwrap_or(Live::Unavailable("路径、权限或忽略规则不可安全核对"))
    }

    fn inspect(&mut self, relative: &str, mode: ReadAs) -> Result<Live> {
        ensure!(key(relative)? == relative, "路径不可安全核对");
        let (dir, name) = relative.rsplit_once('/').unwrap_or(("", relative));
        let path = self.root.join(relative);
        if path == self.db
            || (matches!(mode, ReadAs::Note(_)) && loom_parse::language(name).is_none())
        {
            return Ok(Live::Unavailable("不在源码范围"));
        }
        let scope = match self.directory(dir) {
            Ok(s) => s,
            Err(b) => return Ok(b.live()),
        };
        if scope.excluded(&path, name, false) {
            return Ok(Live::Unavailable("当前忽略规则排除此路径"));
        }
        let Some(m) = metadata(&path)? else {
            return Ok(Live::Missing);
        };
        if redirected(&m) || !m.is_file() {
            return Ok(Live::Unavailable("链接、重定向或目标类型改变"));
        }
        if matches!(mode, ReadAs::Presence) {
            return Ok(Live::Read(String::new()));
        }
        if matches!(mode, ReadAs::Note(_)) && scope::generated_name(name) {
            return Ok(Live::Unavailable("源码标记为生成文件"));
        }
        if matches!(mode, ReadAs::Note(false)) {
            return Ok(Live::Unavailable("路径没有已解析的快照"));
        }
        if m.len() > MAX_BYTES {
            return Ok(Live::Unavailable("源码超过读取上限"));
        }
        let bytes = read_limited(&path, &m, MAX_BYTES)?;
        if matches!(mode, ReadAs::Note(_)) && scope::generated_header(std::str::from_utf8(&bytes)?)
        {
            return Ok(Live::Unavailable("源码标记为生成文件"));
        }
        Ok(Live::Read(sha256_hex(&bytes)))
    }
}
