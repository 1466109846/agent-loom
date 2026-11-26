use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use super::{Direction, QueryReport, VerifiedFile, VerifiedInput};
use crate::{
    note::source::{Inspector, Live, ReadAs},
    project::ProjectIdentity,
};

const MAX_FILES: usize = 32;
const MAX_CONFIGS: usize = 16;

impl QueryReport {
    pub(crate) fn conclude(&mut self) {
        let mut gaps = Vec::new();
        match self.status.as_str() {
            "not_indexed" => gaps.push("not_indexed"),
            "absent" => {
                gaps.push("absent");
                if self.coverage.managed_files == 0 {
                    gaps.push("empty_scope");
                }
            }
            _ => {
                if self.status != "indexed" {
                    gaps.push("target_uncovered");
                }
                if self.truncated {
                    gaps.push("truncated");
                }
                let incoming = self.direction == Direction::Incoming;
                if if incoming {
                    self.coverage.unresolved_refs > 0
                } else {
                    !self.issues.is_empty()
                } {
                    gaps.push("unresolved_refs");
                }
                if incoming && self.coverage.uncovered_files > 0 {
                    gaps.push("uncovered_files");
                }
                if incoming && !self.coverage.known {
                    gaps.push("coverage_unknown");
                }
                if self
                    .verification
                    .files
                    .iter()
                    .any(|f| matches!(f.state, "changed" | "missing"))
                {
                    gaps.push("changed");
                }
                if self
                    .verification
                    .inputs
                    .iter()
                    .any(|f| matches!(f.state, "changed" | "missing"))
                {
                    gaps.push("inputs_changed");
                }
            }
        }
        self.conclusion.verdict = if !self.relations.is_empty() {
            "found"
        } else if gaps.is_empty() {
            "none_in_snapshot"
        } else {
            "gap"
        };
        self.conclusion.gaps = gaps;
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

#[derive(Default)]
pub(crate) struct Basis {
    files: Vec<(String, String)>,
    skipped: usize,
    kinds: BTreeSet<&'static str>,
    inputs: BTreeMap<&'static str, BTreeMap<String, Option<String>>>,
    partial: BTreeSet<&'static str>,
}

impl Basis {
    pub(super) fn load(db: &Connection, out: &QueryReport, refs: &[i64]) -> Result<Self> {
        let mut basis = Self::default();
        if matches!(out.status.as_str(), "absent" | "not_indexed") {
            return Ok(basis);
        }
        let mut seen = BTreeSet::new();
        let mut paths = Vec::new();
        let mut add = |p: &str| {
            if seen.insert(p.to_owned()) {
                paths.push(p.to_owned());
            }
        };
        add(&out.target);
        for e in &out.relations {
            add(&e.source);
            add(&e.target);
        }
        for i in &out.issues {
            add(&i.source);
        }
        for (e, id) in out.relations.iter().zip(refs) {
            if e.source.ends_with(".rs") {
                super::rust_declarations::chain(db, *id, &e.source, &mut add)?;
            }
        }
        basis.skipped = paths.len().saturating_sub(MAX_FILES);
        let mut dirs = BTreeSet::new();
        let mut py_dirs = BTreeSet::new();
        let mut hash = db.prepare_cached("SELECT sha256 FROM files WHERE path=?1")?;
        for p in paths.into_iter().take(MAX_FILES) {
            let lang = match loom_parse::language(&p) {
                Some("javascript" | "typescript" | "vue") => Some("ecma"),
                Some("go") => Some("go"),
                Some("rust") => Some("rust"),
                Some("python") => Some("python"),
                _ => None,
            };
            basis.kinds.insert("rules");
            if let Some(kind) = lang {
                basis.kinds.insert(kind);
            }
            for dir in
                std::iter::successors(Some(parent(&p)), |d| (!d.is_empty()).then(|| parent(d)))
            {
                dirs.insert(dir.to_owned());
                if lang == Some("python") {
                    py_dirs.insert(dir.to_owned());
                }
            }
            let digest = hash.query_row([&p], |r| r.get(0))?;
            basis.files.push((p, digest));
        }
        if !out.coverage.known {
            return Ok(basis);
        }
        let mut get = db
            .prepare_cached("SELECT digest FROM structure_input_files WHERE kind=?1 AND path=?2")?;
        for &kind in &basis.kinds {
            let entries = basis.inputs.entry(kind).or_default();
            if kind == "rules" {
                let mut base=db.prepare_cached("SELECT path,digest FROM structure_input_files WHERE kind='rules' AND path GLOB '*/info/exclude'")?;
                for row in base.query_map([], |r| Ok((r.get(0)?, Some(r.get(1)?))))? {
                    let (p, h) = row?;
                    entries.insert(p, h);
                }
                for path in dirs
                    .iter()
                    .map(|d| join(d, ".gitignore"))
                    .chain([".loom/ignore".into(), "^outer".into()])
                {
                    let val = get
                        .query_row(params![kind, path], |r| r.get(0))
                        .optional()?;
                    entries.insert(path, val);
                }
            } else if kind == "python" {
                let mut exists=db.prepare_cached("SELECT EXISTS(SELECT 1 FROM files f JOIN structure_files s ON s.file_id=f.id WHERE f.path=?1)")?;
                for d in &py_dirs {
                    let path = join(d, "__init__.py");
                    let present: bool = exists.query_row([&path], |r| r.get(0))?;
                    entries.insert(path, present.then(String::new));
                }
            } else {
                let names: &[&str] = match kind {
                    "ecma" => &["tsconfig.json", "jsconfig.json"],
                    "go" => &["go.mod"],
                    _ => &["Cargo.toml"],
                };
                let mut q=db.prepare_cached("SELECT path,digest FROM structure_input_files WHERE kind=?1 ORDER BY path LIMIT ?2")?;
                let all = q
                    .query_map(params![kind, (MAX_CONFIGS + 1) as i64], |r| {
                        Ok((r.get::<_, String>(0)?, Some(r.get::<_, String>(1)?)))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if all.len() <= MAX_CONFIGS {
                    entries.extend(all);
                } else {
                    basis.partial.insert(kind);
                    let mut under=db.prepare_cached("SELECT path,digest FROM structure_input_files WHERE kind=?1 AND path>=?2 AND path<?3")?;
                    for d in &dirs {
                        for name in names {
                            let prefix = join(d, name.trim_end_matches(".json"));
                            for row in under.query_map(
                                params![kind, prefix, format!("{prefix}\u{10ffff}")],
                                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                            )? {
                                let (p, h) = row?;
                                if parent(&p) == d {
                                    entries.insert(p, Some(h));
                                }
                            }
                        }
                    }
                }
                for dir in &dirs {
                    for name in names {
                        entries.entry(join(dir, name)).or_default();
                    }
                }
            }
        }
        Ok(basis)
    }

    pub(crate) fn verify(self, project: &ProjectIdentity, db: Option<&str>, out: &mut QueryReport) {
        if self.files.is_empty() {
            return;
        }
        let mut live = Inspector::new(project, db).ok();
        let v = &mut out.verification;
        v.skipped = self.skipped;
        for (path, hash) in self.files {
            let state = match live.as_mut().map(|i| i.file(&path, ReadAs::Digest)) {
                Some(Live::Read(s)) if s == hash => "same",
                Some(Live::Read(_)) if !hash.is_empty() => "changed",
                Some(Live::Missing) => "missing",
                _ => "unverifiable",
            };
            v.files.push(VerifiedFile { path, state });
        }
        for kind in ["rules", "go", "ecma", "rust", "python"]
            .into_iter()
            .filter(|k| self.kinds.contains(k))
        {
            let state = match (&mut live, self.inputs.get(kind)) {
                (_, None) => "unknown",
                (None, _) => "unverifiable",
                (Some(i), Some(entries)) => {
                    let changed_scope = kind == "rules"
                        && (out.snapshot.as_ref().and_then(|s| s.scope.as_deref())
                            != Some(i.scope())
                            || i.base_rules().any(|(p, _)| !entries.contains_key(p)));
                    let mut state = if changed_scope {
                        "changed"
                    } else if self.partial.contains(kind) {
                        "unverifiable"
                    } else {
                        "same"
                    };
                    for (p, old) in entries {
                        let now = match (kind, p.as_str()) {
                            ("rules", _) => i.rule(p),
                            ("python", _) => i.file(p, ReadAs::Presence),
                            _ if old.is_none() => i.file(p, ReadAs::Presence),
                            _ => i.file(p, ReadAs::Digest),
                        };
                        let next = match (old, now) {
                            (None, Live::Missing) => "same",
                            (Some(_), Live::Missing) => "missing",
                            (Some(before), Live::Read(after)) if before == &after => "same",
                            (Some(before), Live::Read(_))
                                if before.is_empty() && kind != "python" =>
                            {
                                "unverifiable"
                            }
                            (_, Live::Read(_)) => "changed",
                            _ => "unverifiable",
                        };
                        let rank = |s: &str| match s {
                            "changed" => 3,
                            "missing" => 2,
                            "unverifiable" => 1,
                            _ => 0,
                        };
                        if rank(next) > rank(state) {
                            state = next;
                        }
                    }
                    state
                }
            };
            v.inputs.push(VerifiedInput { kind, state });
        }
    }
}
