//! 结构是显式刷新、可重建的项目快照，不是实时调用图。
mod index;
pub(crate) mod query;
mod resolve;
mod rust_declarations;
mod scan;
pub(crate) mod scope;
#[cfg(test)]
mod tests;
mod verify;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::Serialize;

// 独立于项目身份库版本；修改提取或解析语义时递增，由下一次显式索引重算。
const REVISION: i64 = 8;
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS structure_snapshot(
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), revision INTEGER NOT NULL,
 finished_at INTEGER NOT NULL, total INTEGER NOT NULL, uncovered INTEGER NOT NULL,
 unresolved INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS structure_files(
 file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE, state TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS structure_refs(
 id INTEGER PRIMARY KEY, source_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
 line INTEGER NOT NULL, spelling TEXT NOT NULL, kind TEXT NOT NULL, certain INTEGER NOT NULL,
 resolution TEXT NOT NULL DEFAULT 'pending', target_id INTEGER REFERENCES files(id) ON DELETE SET NULL);
CREATE INDEX IF NOT EXISTS structure_from ON structure_refs(source_id,line);
CREATE INDEX IF NOT EXISTS structure_to ON structure_refs(target_id,source_id);
CREATE TABLE IF NOT EXISTS structure_candidates(
 ref_id INTEGER NOT NULL REFERENCES structure_refs(id) ON DELETE CASCADE, path TEXT NOT NULL,
 substitute INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(ref_id,path)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS structure_candidate_path ON structure_candidates(path,ref_id);
CREATE INDEX IF NOT EXISTS structure_symbol_file ON symbols(file_id,line_start);
CREATE TABLE IF NOT EXISTS structure_members(
 name TEXT NOT NULL, file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
 exported INTEGER NOT NULL, PRIMARY KEY(name,file_id)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS structure_members_file ON structure_members(file_id);
CREATE TABLE IF NOT EXISTS structure_provides(
 key TEXT NOT NULL, file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
 PRIMARY KEY(key,file_id)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS structure_provides_file ON structure_provides(file_id);
CREATE TABLE IF NOT EXISTS structure_domains(
 file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE, domain TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS structure_search(
 file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE, key TEXT NOT NULL,
 PRIMARY KEY(file_id,key)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS structure_search_key ON structure_search(key,file_id);
CREATE TABLE IF NOT EXISTS structure_inputs(
 name TEXT PRIMARY KEY, digest TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS structure_rust_inputs(
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), digest TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS structure_rust_scopes(
 file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE, is_root INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS structure_rust_modules(
 file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
 name TEXT NOT NULL, path TEXT NOT NULL, usable INTEGER NOT NULL, crate_visible INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS structure_rust_module_file ON structure_rust_modules(file_id);
CREATE TABLE IF NOT EXISTS structure_rust_items(
 file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE, name TEXT NOT NULL, public INTEGER NOT NULL,
 PRIMARY KEY(file_id,name)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS structure_rust_paths(
 ref_id INTEGER NOT NULL REFERENCES structure_refs(id) ON DELETE CASCADE, path TEXT NOT NULL,
 PRIMARY KEY(ref_id,path)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS structure_rust_module_path ON structure_rust_modules(path);
CREATE TABLE IF NOT EXISTS structure_generation(
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), generation INTEGER NOT NULL);
CREATE TRIGGER IF NOT EXISTS structure_generation_insert AFTER INSERT ON structure_snapshot BEGIN
 INSERT INTO structure_generation VALUES(1,1) ON CONFLICT(singleton) DO UPDATE SET generation=generation+1; END;
CREATE TRIGGER IF NOT EXISTS structure_generation_update AFTER UPDATE ON structure_snapshot BEGIN
 INSERT INTO structure_generation VALUES(1,1) ON CONFLICT(singleton) DO UPDATE SET generation=generation+1; END;
CREATE TABLE IF NOT EXISTS structure_publication(
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), generation INTEGER NOT NULL,
 scope TEXT NOT NULL, inputs TEXT NOT NULL, excluded_dirs INTEGER NOT NULL,
 excluded_files INTEGER NOT NULL, withheld INTEGER NOT NULL, refs INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS structure_coverage(
 state TEXT PRIMARY KEY, count INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS structure_input_files(
 kind TEXT NOT NULL, path TEXT NOT NULL, digest TEXT NOT NULL,
 PRIMARY KEY(kind,path)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS structure_local_exclude ON structure_input_files(path)
 WHERE kind='rules' AND path GLOB '*/info/exclude';
";

#[derive(Debug, Default, Serialize)]
pub struct IndexReport {
    pub scope: &'static str,
    /// 本该纳入、只因名字像凭据而没有纳入的路径数。
    pub withheld: usize,
    pub parsed: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub seen: i64,
    pub uncovered: i64,
    pub relinked: usize,
    pub relations: i64,
    pub unresolved: i64,
    pub finished_at: i64,
    pub generation: i64,
    pub excluded_dirs: usize,
    pub excluded_files: usize,
    pub inputs: String,
    pub refs: i64,
    pub coverage: BTreeMap<String, i64>,
}
impl fmt::Display for IndexReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "快照已更新（第 {} 代）：受管 {} 个文件，本次解析 {}，未变 {}，移除 {}；未覆盖 {}{}。\n重新核对 {} 处引用，现有 {} 条有目标的引用证据，{} 处待确定。",
            self.generation,
            self.seen,
            self.parsed,
            self.unchanged,
            self.removed,
            self.uncovered,
            counts(
                self.coverage
                    .iter()
                    .filter(|(k, _)| *k != "indexed" && !k.starts_with("ref:"))
            ),
            self.relinked,
            self.relations,
            self.unresolved
        )?;
        if self.excluded_dirs > 0 {
            write!(
                f,
                "\n有 {} 个目录被排除，里面没有检查。",
                self.excluded_dirs
            )?;
        }
        f.write_str(if self.scope == "git" {
            "\n范围：以 Git 忽略规则为准。"
        } else {
            "\n范围：没有 Git 忽略规则，按内置名单排除构建产物、依赖和私密目录。"
        })?;
        if self.withheld > 0 {
            write!(f, "\n另有 {} 个路径的名字像凭据，没有纳入。", self.withheld)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Outgoing,
    Incoming,
}

#[derive(Debug, Default, Serialize)]
pub struct Coverage {
    pub managed_files: i64,
    pub uncovered_files: i64,
    pub unresolved_refs: i64,
    pub known: bool,
    pub uncovered: Option<BTreeMap<String, i64>>,
    pub unresolved: Option<BTreeMap<String, i64>>,
    pub excluded_dirs: Option<i64>,
    pub excluded_files: Option<i64>,
    pub withheld: Option<i64>,
    pub refs: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub generation: Option<i64>,
    pub revision: i64,
    pub finished_at: i64,
    pub scope: Option<String>,
    pub inputs: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Examined {
    pub files: usize,
    pub refs: i64,
}

#[derive(Debug, Serialize)]
pub struct VerifiedFile {
    pub path: String,
    pub state: &'static str,
}

#[derive(Debug, Serialize)]
pub struct VerifiedInput {
    pub kind: &'static str,
    pub state: &'static str,
}

#[derive(Debug, Default, Serialize)]
pub struct Verification {
    pub files: Vec<VerifiedFile>,
    pub skipped: usize,
    pub inputs: Vec<VerifiedInput>,
}

#[derive(Debug, Default, Serialize)]
pub struct Conclusion {
    pub verdict: &'static str,
    pub gaps: Vec<&'static str>,
}
#[derive(Debug, Serialize)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub line: i64,
}
#[derive(Debug, Serialize)]
pub struct Relation {
    pub source: String,
    pub target: String,
    pub kind: String,
    pub line: i64,
    pub confidence: String,
    pub hop: usize,
}
#[derive(Debug, Serialize)]
pub struct Issue {
    pub source: String,
    pub spelling: String,
    pub line: i64,
    pub state: String,
    /// 只展示实际存在且纳入本项目的候选，不列臆测路径。
    pub candidates: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct QueryReport {
    pub contract: u8,
    pub target: String,
    pub direction: Direction,
    pub depth: usize,
    pub status: String,
    pub snapshot_at: Option<i64>,
    pub coverage: Coverage,
    pub symbols: Vec<Symbol>,
    pub relations: Vec<Relation>,
    pub issues: Vec<Issue>,
    pub truncated: bool,
    pub snapshot: Option<Snapshot>,
    pub examined: Examined,
    pub verification: Verification,
    pub conclusion: Conclusion,
    #[serde(skip)]
    limits: BTreeSet<&'static str>,
}

fn counts<'a>(rows: impl Iterator<Item = (&'a String, &'a i64)>) -> String {
    let values: Vec<_> = rows
        .map(|(state, n)| format!("{} {n}", label(state).trim_start_matches("未覆盖：")))
        .collect();
    if values.is_empty() {
        String::new()
    } else {
        text::clean(format!("（{}）", values.join("、")))
    }
}

pub(crate) fn label(code: &str) -> &str {
    match code {
        "not_indexed" => "还没有索引，请先更新结构快照",
        "absent" => "不在本次受管范围内",
        "indexed" => "已分析",
        "too_large" => "未覆盖：超过大小上限",
        "syntax_error" => "未覆盖：语法不完整",
        "invalid_utf8" => "未覆盖：非受支持的文本编码",
        "unreadable" => "未覆盖：无法读取",
        "generated" => "未覆盖：生成文件",
        "too_many_facts" => "未覆盖：单文件事实过多",
        "exact" => "静态确定",
        "likely" => "可能",
        "ambiguous" => "有多个候选",
        "missing" => "受管文件中没有候选",
        "unsupported" => "无法静态确定",
        "external" => "外部引用",
        "uncovered_target" => "目标未覆盖",
        "pending" => "待确定",
        other => other,
    }
}
mod text;
pub(crate) use text::{clean as display_line, unsafe_char};
