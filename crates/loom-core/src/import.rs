//! 从 oh-memos `memos_export_wiki` 导出的 Markdown 导入记忆。
//!
//! 页面格式：
//! ```text
//! ---
//! generator: oh-memos-wiki-export
//! id: <uuid>
//! type: DECISION
//! status: activated
//! tags: ["a", "b"]
//! confidence: 0.99
//! created: 2026-03-10T19:38:16.544904000+00:00
//! updated: 2026-03-10T19:38:16.866124000+00:00
//! ---
//!
//! # 标题
//!
//! 正文……
//!
//! ## 关联
//! - ……
//! ```
//! 只读取带 `generator: oh-memos-wiki-export` 标记的页，源目录只读不改。

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use regex::Regex;
use serde::Serialize;
use walkdir::WalkDir;

use crate::db::Store;
use crate::memory::{MemoryType, NewMemory, RememberOutcome};
use crate::util::parse_rfc3339;

const GENERATOR: &str = "oh-memos-wiki-export";

/// 解析出的一页。
#[derive(Debug, Clone)]
pub struct WikiPage {
    pub file: PathBuf,
    pub legacy_id: String,
    pub mem_type: MemoryType,
    pub status: String,
    pub tags: Vec<String>,
    pub confidence: Option<f64>,
    pub created: Option<i64>,
    pub updated: Option<i64>,
    pub title: String,
    pub body: String,
    pub anchors: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportReport {
    pub scanned: usize,
    pub parsed: usize,
    pub created: usize,
    pub duplicate: usize,
    pub skipped: Vec<(String, String)>,
    pub anchors: usize,
    pub redactions: usize,
    pub dry_run: bool,
}

/// 正文里出现的相对源码路径，例如 `src/oh_memos/api/start_api.py`、`mcp-server-node/src/search.ts`。
static PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[\s`'(（\[])((?:[A-Za-z0-9_.\-]+/)+[A-Za-z0-9_.\-]+\.(?:rs|py|ts|tsx|js|jsx|mjs|go|java|vue|wxml|wxss|json|toml|yml|yaml|md|sql|sh|ps1|bat))",
    )
    .expect("内置正则必须合法")
});

fn parse_tags(raw: &str) -> Vec<String> {
    let raw = raw.trim();
    if raw.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<Vec<String>>(raw) {
            return v;
        }
        return raw
            .trim_matches(|c| c == '[' || c == ']')
            .split(',')
            .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    if raw.is_empty() { Vec::new() } else { vec![raw.to_string()] }
}

fn map_status(s: &str) -> Option<&'static str> {
    match s.trim() {
        "activated" | "active" | "" => Some("active"),
        "archived" => Some("archived"),
        "superseded" => Some("superseded"),
        _ => None, // deleted 等不导入
    }
}

/// 解析单页；不是导出页或缺少必要字段时返回 Err(原因)。
pub fn parse_page(file: &Path, text: &str) -> std::result::Result<WikiPage, String> {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let rest = text.strip_prefix("---\n").ok_or("缺少 front-matter")?;
    let (fm, content) = rest.split_once("\n---").ok_or("front-matter 未闭合")?;

    let mut kv = std::collections::HashMap::new();
    for line in fm.lines() {
        if let Some((k, v)) = line.split_once(':') {
            kv.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    if kv.get("generator").map(String::as_str) != Some(GENERATOR) {
        return Err("不是 oh-memos 导出页".into());
    }
    let legacy_id = kv.get("id").filter(|s| !s.is_empty()).ok_or("缺少 id")?.clone();
    let ty_raw = kv.get("type").ok_or("缺少 type")?;
    let mem_type: MemoryType = ty_raw.parse().map_err(|_| format!("未知类型 {ty_raw}"))?;
    let status = map_status(kv.get("status").map(String::as_str).unwrap_or(""))
        .ok_or_else(|| format!("状态 {} 不导入", kv.get("status").cloned().unwrap_or_default()))?
        .to_string();

    // 正文：取第一个 `# ` 作为标题，去掉末尾的 `## 关联` 段（关系边当前不迁移）。
    let content = content.trim_start_matches('-').trim();
    let mut title = String::new();
    let mut body_lines = Vec::new();
    for line in content.lines() {
        if title.is_empty() && line.starts_with("# ") {
            title = line[2..].trim().to_string();
            continue;
        }
        if line.trim() == "## 关联" {
            break;
        }
        body_lines.push(line);
    }
    let body = body_lines.join("\n").trim().to_string();
    if title.is_empty() {
        title = body.lines().next().unwrap_or_default().chars().take(60).collect();
    }
    if body.is_empty() {
        return Err("正文为空".into());
    }

    let mut anchors: Vec<String> = PATH_RE
        .captures_iter(&body)
        .map(|c| c[1].trim_end_matches('.').to_string())
        .filter(|p| !p.starts_with("http") && !p.contains("//"))
        .collect();
    anchors.sort();
    anchors.dedup();
    anchors.truncate(8);

    Ok(WikiPage {
        file: file.to_path_buf(),
        legacy_id,
        mem_type,
        status,
        tags: kv.get("tags").map(|t| parse_tags(t)).unwrap_or_default(),
        confidence: kv.get("confidence").and_then(|c| c.parse().ok()),
        created: kv.get("created").and_then(|t| parse_rfc3339(t)),
        updated: kv.get("updated").and_then(|t| parse_rfc3339(t)),
        title,
        body,
        anchors,
    })
}

/// 导入目录（wiki 根目录或其 `pages/` 子目录均可）。`apply = false` 时只统计不写入。
pub fn import_oh_memos_wiki(store: &Store, dir: &Path, apply: bool) -> Result<ImportReport> {
    let pages_dir = if dir.join("pages").is_dir() { dir.join("pages") } else { dir.to_path_buf() };
    anyhow::ensure!(pages_dir.is_dir(), "目录不存在: {}", pages_dir.display());

    let mut report = ImportReport { dry_run: !apply, ..Default::default() };
    let mut files: Vec<PathBuf> = WalkDir::new(&pages_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "md"))
        .map(|e| e.into_path())
        .collect();
    files.sort();

    let tx_guard = if apply { Some(store.conn.unchecked_transaction()?) } else { None };
    for file in files {
        report.scanned += 1;
        let name = file.strip_prefix(&pages_dir).unwrap_or(&file).display().to_string();
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("读取失败: {}", file.display()))?;
        let page = match parse_page(&file, &text) {
            Ok(p) => p,
            Err(why) => {
                report.skipped.push((name, why));
                continue;
            }
        };
        report.parsed += 1;

        if !apply {
            let dup: bool = store.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM memories WHERE legacy_id = ?1)",
                [&page.legacy_id],
                |r| r.get(0),
            )?;
            if dup {
                report.duplicate += 1;
            } else {
                report.created += 1;
                report.anchors += page.anchors.len();
            }
            continue;
        }

        let anchor_count = page.anchors.len();
        let out = store.remember(NewMemory {
            mem_type: Some(page.mem_type),
            title: page.title,
            body: page.body,
            tags: page.tags,
            importance: None,
            confidence: page.confidence,
            anchors: page.anchors,
            origin: Some("import:oh-memos".into()),
            legacy_id: Some(page.legacy_id),
            status: Some(page.status),
            created_at: page.created,
            updated_at: page.updated.or(page.created),
            force: true,
            bypass_admission: true,
            unverified_anchors: true,
        })?;
        match out {
            RememberOutcome::Created { redactions, .. } => {
                report.created += 1;
                report.anchors += anchor_count;
                report.redactions += redactions;
            }
            RememberOutcome::Duplicate { .. } => report.duplicate += 1,
            RememberOutcome::Similar { .. } => unreachable!("force=true 不会返回 Similar"),
            RememberOutcome::Rejected { reason } => report.skipped.push((name, reason)),
        }
    }
    if let Some(tx) = tx_guard {
        tx.commit()?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "---\ngenerator: oh-memos-wiki-export\nid: eadea7a4-fc1b-4144-99ac-ff7676d4ac35\ntype: DECISION\nstatus: activated\ntags: [\"性能优化\", \"根因分析\"]\nconfidence: 0.99\ncreated: 2026-03-10T19:38:16.544904000+00:00\nupdated: 2026-03-10T19:38:16.866124000+00:00\n---\n\n# API启动慢根因分析\n\nstartup_auto_register 串行注册 cube，见 src/oh_memos/api/start_api.py，导致启动慢。\n\n## 关联\n\n- 被上级 ← [[x]]\n";

    #[test]
    fn parses_exported_page() {
        let p = parse_page(Path::new("a.md"), PAGE).unwrap();
        assert_eq!(p.mem_type, MemoryType::Decision);
        assert_eq!(p.title, "API启动慢根因分析");
        assert_eq!(p.status, "active");
        assert_eq!(p.tags, vec!["性能优化", "根因分析"]);
        assert!(!p.body.contains("关联"));
        assert_eq!(p.anchors, vec!["src/oh_memos/api/start_api.py"]);
        assert!(p.created.is_some());
    }

    #[test]
    fn rejects_foreign_pages() {
        assert!(parse_page(Path::new("a.md"), "# 普通文档\n内容").is_err());
        let other = PAGE.replace(GENERATOR, "someone-else");
        assert!(parse_page(Path::new("a.md"), &other).is_err());
    }

    #[test]
    fn dry_run_then_apply_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let pages = dir.path().join("pages").join("DECISION");
        std::fs::create_dir_all(&pages).unwrap();
        std::fs::write(pages.join("a.md"), PAGE).unwrap();
        std::fs::write(pages.join("b.md"), "no front matter").unwrap();

        let s = Store::open_in_memory().unwrap();
        let dry = import_oh_memos_wiki(&s, dir.path(), false).unwrap();
        assert_eq!((dry.scanned, dry.parsed, dry.created, dry.skipped.len()), (2, 1, 1, 1));
        assert_eq!(s.count().unwrap(), 0);

        let real = import_oh_memos_wiki(&s, dir.path(), true).unwrap();
        assert_eq!(real.created, 1);
        let again = import_oh_memos_wiki(&s, dir.path(), true).unwrap();
        assert_eq!((again.created, again.duplicate), (0, 1));
        assert_eq!(s.count().unwrap(), 1);
    }
}
