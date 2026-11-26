//! 先执行隐私与范围规则，再打开文件。无法完整列目录时使整次刷新回滚。
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use super::scope::{self, MAX_BYTES, MAX_DEPTH, Scope, metadata, redirected};
use crate::{redact::sensitive, util::sha256_hex};
use anyhow::{Context, Result, ensure};

pub(super) fn key(root: &Path, path: &Path) -> Result<String> {
    let p = path
        .strip_prefix(root)?
        .to_str()
        .context("源码路径不能无损表示；保留上一版索引")?;
    let p = p.replace(std::path::MAIN_SEPARATOR, "/");
    ensure!(
        !p.contains([':', '\\']) && !p.chars().any(char::is_control),
        "不支持该源码路径；保留上一版索引"
    );
    Ok(p)
}

fn read_limited(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub(super) type InputFiles = BTreeMap<(&'static str, String), String>;

pub(super) struct Found {
    pub files: BTreeMap<String, PathBuf>,
    pub inputs: InputFiles,
    pub excluded_dirs: usize,
    pub excluded_files: usize,
    pub scope: &'static str,
    /// 本该纳入、只因名字像凭据而没有纳入的路径数。
    pub withheld: usize,
}

fn source(name: &str) -> bool {
    loom_parse::language(name).is_some()
        || matches!(name, "Cargo.toml" | "go.mod")
        || loom_parse::ecma_config(name)
}

pub(super) fn discover(root: &Path, blocked: &mut BTreeSet<String>) -> Result<Found> {
    fn visit(
        root: &Path,
        dir: &Path,
        depth: usize,
        scope: &mut Scope,
        found: &mut Found,
        blocked: &mut BTreeSet<String>,
    ) -> Result<()> {
        ensure!(depth <= MAX_DEPTH, "目录过深；未更新索引");
        let entries = fs::read_dir(dir)
            .context("不能完整列出目录；未更新索引")?
            .collect::<std::io::Result<Vec<_>>>()?;
        if depth != 0 && !scope.markers().is_empty() {
            for entry in &entries {
                if scope.is_marker(&entry.file_name()) && entry.file_type()?.is_file() {
                    found.excluded_dirs += 1;
                    blocked.insert(key(root, dir)?);
                    return Ok(());
                }
            }
        }
        let (old, digest) = scope.enter(dir)?;
        if let Some(digest) = digest {
            found
                .inputs
                .insert(("rules", key(root, &dir.join(".gitignore"))?), digest);
        }
        for entry in entries {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path)?;
            if redirected(&meta) {
                if scope::directory_link(&meta) && !scope::administrative(&entry.file_name()) {
                    found.excluded_dirs += 1;
                }
                if let Ok(k) = key(root, &path) {
                    blocked.insert(k);
                }
                continue;
            }
            let name = entry.file_name();
            let name = name
                .to_str()
                .context("不能无损表示目录项名称；未更新索引")?;
            let is_dir = meta.is_dir();
            let relative = key(root, &path);
            let hidden = relative.as_deref().is_ok_and(sensitive);
            let ignored = scope.excluded(&path, name, is_dir);
            if hidden || ignored {
                if is_dir && !scope::administrative(name.as_ref()) {
                    found.excluded_dirs += 1;
                } else if ignored && meta.is_file() && source(name) {
                    found.excluded_files += 1;
                }
                if !ignored && (is_dir || source(name)) {
                    found.withheld += 1;
                }
                // 只记已经枚举到的边界，不进入忽略目录，也不读取/持久化这些路径。
                if (hidden || is_dir || name.ends_with(".rs"))
                    && let Ok(k) = relative
                {
                    blocked.insert(k);
                }
                continue;
            }
            if is_dir {
                ensure!(
                    path.canonicalize()? == path,
                    "扫描期间目录目标发生变化；未更新索引"
                );
                // 子项目不属于父项目，即便二者目录嵌套。
                if metadata(&path.join(".loom"))?.is_some() {
                    found.excluded_dirs += 1;
                    blocked.insert(relative?);
                    continue;
                }
                visit(root, &path, depth + 1, scope, found, blocked)?;
            } else if meta.is_file() && source(name) {
                found.files.insert(relative?, path);
            }
        }
        scope.leave(old);
        Ok(())
    }
    let mut scope = Scope::new(root, |p, _, limit| Ok(read_limited(p, limit)?))?;
    let mut found = Found {
        files: BTreeMap::new(),
        scope: scope.name(),
        inputs: scope
            .base_rules
            .iter()
            .map(|(key, hash)| (("rules", key.clone()), hash.clone()))
            .collect(),
        excluded_dirs: 0,
        excluded_files: 0,
        withheld: 0,
    };
    visit(root, root, 0, &mut scope, &mut found, blocked)?;
    Ok(found)
}

pub(super) struct Input {
    pub hash: String,
    pub size: u64,
    pub state: &'static str,
    pub text: Option<String>,
}
impl Input {
    fn unavailable(state: &'static str, size: u64) -> Self {
        Self {
            state,
            size,
            hash: String::new(),
            text: None,
        }
    }
}
pub(super) fn read(path: &Path) -> Input {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Input::unavailable("unreadable", 0);
    };
    if redirected(&meta) || !meta.is_file() || !path.canonicalize().is_ok_and(|p| p == path) {
        return Input::unavailable("unreadable", 0);
    }
    let by_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(scope::generated_name);
    let oversized = if by_name { "generated" } else { "too_large" };
    if meta.len() > MAX_BYTES {
        return Input::unavailable(oversized, meta.len());
    }
    let Ok(buf) = read_limited(path, MAX_BYTES) else {
        return Input::unavailable("unreadable", meta.len());
    };
    let size = buf.len() as u64;
    if size > MAX_BYTES {
        return Input::unavailable(oversized, size);
    }
    let hash = sha256_hex(&buf);
    let (state, text) = if by_name {
        ("generated", None)
    } else {
        match String::from_utf8(buf) {
            Ok(text) if scope::generated_header(&text) => ("generated", None),
            Ok(text) => ("read", Some(text)),
            Err(_) => ("invalid_utf8", None),
        }
    };
    Input {
        state,
        size,
        hash,
        text,
    }
}
