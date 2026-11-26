//! 扫描与单路径核对共用的源码范围；调用方保留各自打开文件时的安全检查。
use std::{
    ffi::OsStr,
    fs::{self, Metadata},
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use ignore::gitignore::{Gitignore, GitignoreBuilder};

pub(crate) const MAX_BYTES: u64 = 1_048_576;
pub(crate) const MAX_DEPTH: usize = 128;
const MAX_RULES: u64 = 262_144;
const MARKERS: &[&str] = &["pyvenv.cfg", "CACHEDIR.TAG"];
type ReadFile = fn(&Path, &Metadata, u64) -> Result<Vec<u8>>;

pub(crate) fn redirected(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT，包括目录联接。
    }
    #[cfg(not(windows))]
    {
        meta.is_symlink()
    }
}

pub(crate) fn metadata(path: &Path) -> Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn control_file(path: &Path, meta: &Metadata, read: ReadFile) -> Result<Vec<u8>> {
    ensure!(
        meta.is_file()
            && !redirected(meta)
            && meta.len() <= MAX_RULES
            && path.canonicalize()? == path,
        "忽略规则文件不可安全读取；未更新索引"
    );
    let bytes = read(path, meta, MAX_RULES)?;
    ensure!(bytes.len() as u64 <= MAX_RULES, "忽略规则文件过大");
    Ok(bytes)
}

fn rules(base: &Path, path: &Path, read: ReadFile) -> Result<Option<(Gitignore, String)>> {
    let Some(meta) = metadata(path)? else {
        return Ok(None);
    };
    let bytes = control_file(path, &meta, read)?;
    let text = std::str::from_utf8(&bytes).context("忽略规则编码不支持；未更新索引")?;
    let mut builder = GitignoreBuilder::new(base);
    for line in text.trim_start_matches('\u{feff}').lines() {
        builder
            .add_line(None, line)
            .context("忽略规则无效；未更新索引")?;
    }
    Ok(Some((builder.build()?, crate::util::sha256_hex(&bytes))))
}

fn pointed_dir(
    file: &Path,
    meta: &Metadata,
    prefix: &str,
    read: ReadFile,
) -> Result<Option<PathBuf>> {
    if !meta.is_file() || meta.len() > MAX_RULES {
        return Ok(None);
    }
    let bytes = control_file(file, meta, read)?;
    let Some(target) = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|s| s.trim_end_matches(['\r', '\n']).strip_prefix(prefix))
        .filter(|s| !s.is_empty() && !s.contains(['\r', '\n', '\0']))
    else {
        return Ok(None);
    };
    // gitdir 和 commondir 里的相对路径分别以各自所在目录为基准，不能把带 .. 的路径交给 rules。
    match file.with_file_name(target).canonicalize() {
        Ok(dir) => Ok(fs::metadata(&dir)?.is_dir().then_some(dir)),
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::NotFound
                    | ErrorKind::NotADirectory
                    | ErrorKind::InvalidInput
                    | ErrorKind::InvalidFilename
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

fn common_dir(git: &Path, meta: &Metadata, read: ReadFile) -> Result<Option<PathBuf>> {
    let dir = if meta.is_dir() && !redirected(meta) {
        git.to_owned()
    } else if let Some(dir) = pointed_dir(git, meta, "gitdir: ", read)? {
        dir
    } else {
        return Ok(None);
    };
    let common = dir.join("commondir");
    match metadata(&common)? {
        Some(m) => pointed_dir(&common, &m, "", read),
        None => Ok(Some(dir)),
    }
}

#[derive(Clone)]
pub(crate) struct Scope {
    git: bool,
    read: ReadFile,
    stack: Vec<Gitignore>,
    loom: Option<Gitignore>,
    pub(crate) base_rules: Vec<(String, String)>,
}

impl Scope {
    pub(crate) fn new(root: &Path, read: ReadFile) -> Result<Self> {
        let loom = rules(root, &root.join(".loom/ignore"), read)?;
        let mut scope = Self {
            git: metadata(&root.join(".gitignore"))?.is_some(),
            read,
            stack: Vec::new(),
            base_rules: loom
                .as_ref()
                .map(|(_, digest)| (".loom/ignore".into(), digest.clone()))
                .into_iter()
                .collect(),
            loom: loom.map(|(rule, _)| rule),
        };
        let mut outer = String::new();
        'repository: for top in root.ancestors() {
            let git = top.join(".git");
            let Some(meta) = metadata(&git)? else {
                continue;
            };
            if let Some(exclude) =
                common_dir(&git, &meta, read)?.map(|common| common.join("info/exclude"))
                && let Some((rule, digest)) = rules(top, &exclude, read)?
            {
                if exclude.starts_with(root) {
                    scope
                        .base_rules
                        .push((super::scan::key(root, &exclude)?, digest));
                } else {
                    outer.push_str(&digest);
                }
                scope.stack.push(rule);
            }
            let mut dir = top.to_owned();
            for name in root.strip_prefix(top)? {
                if let (_, Some(digest)) = scope.enter(&dir)? {
                    outer.push_str(&digest);
                }
                dir.push(name);
                if scope.git_excluded(&dir, true) {
                    // 最近的仓库不管理这个项目；只保留由项目根自己决定的范围。
                    scope.stack.clear();
                    break 'repository;
                }
            }
            scope.git = true;
            break;
        }
        if !outer.is_empty() {
            scope
                .base_rules
                .push(("^outer".into(), crate::util::sha256_hex(outer.as_bytes())));
        }
        Ok(scope)
    }

    pub(crate) fn name(&self) -> &'static str {
        if self.git { "git" } else { "builtin" }
    }

    pub(crate) fn enter(&mut self, dir: &Path) -> Result<(usize, Option<String>)> {
        let old = self.stack.len();
        if let Some((rule, digest)) = rules(dir, &dir.join(".gitignore"), self.read)? {
            self.stack.push(rule);
            return Ok((old, Some(digest)));
        }
        Ok((old, None))
    }

    pub(crate) fn leave(&mut self, old: usize) {
        self.stack.truncate(old);
    }

    fn git_excluded(&self, path: &Path, dir: bool) -> bool {
        self.stack
            .iter()
            .rev()
            .map(|r| r.matched(path, dir))
            .find(|m| !m.is_none())
            .is_some_and(|m| m.is_ignore())
    }

    pub(crate) fn excluded(&self, path: &Path, name: &str, dir: bool) -> bool {
        let reserved = if self.git {
            administrative(name.as_ref()) || (dir && same_name(name.as_ref(), "node_modules"))
        } else {
            builtin(name, dir)
        };
        reserved
            || self.git_excluded(path, dir)
            || self
                .loom
                .as_ref()
                .is_some_and(|r| r.matched(path, dir).is_ignore())
    }

    pub(crate) fn markers(&self) -> &'static [&'static str] {
        if self.git { MARKERS } else { &[] }
    }

    pub(crate) fn is_marker(&self, name: &OsStr) -> bool {
        self.markers().iter().any(|m| same_name(name, m))
    }
}

pub(super) fn administrative(name: &OsStr) -> bool {
    same_name(name, ".git") || same_name(name, ".loom")
}

pub(super) fn directory_link(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x10 != 0
    }
    #[cfg(not(windows))]
    {
        // 链接本身不携带目标类型，不能为了分类去访问目标。
        meta.is_symlink()
    }
}

fn same_name(name: &OsStr, expected: &str) -> bool {
    if cfg!(windows) {
        name.eq_ignore_ascii_case(expected)
    } else {
        name == expected
    }
}

fn builtin(name: &str, dir: bool) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with('.')
        || if dir {
            matches!(
                n.as_str(),
                "target"
                    | "node_modules"
                    | "dist"
                    | "build"
                    | "out"
                    | "vendor"
                    | "tmp"
                    | "temp"
                    | "__pycache__"
                    | "coverage"
                    | "data"
                    | "logs"
                    | "backups"
                    | "secrets"
                    | "credentials"
                    | "private_keys"
                    | "venv"
            )
        } else {
            generated_name(&n)
                || ["secrets.", "credentials.", "private_key.", "private-key."]
                    .iter()
                    .any(|p| n.starts_with(p))
        }
}

pub(crate) fn generated_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".min.js") || n.ends_with(".generated.ts") || n.ends_with(".generated.js")
}

pub(crate) fn generated_header(text: &str) -> bool {
    text.lines().take(5).any(|line| {
        line.len() < 512
            && (line.trim_start().starts_with("//") || line.trim_start().starts_with('#'))
            && line.contains("@generated")
    })
}
