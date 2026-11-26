//! 显式任务上下文：共用一份项目读视图，现场结构核对不占用读事务。
mod memories;
mod render;
#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, str::FromStr};

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::{
    Store, intent, note, redact,
    structure::{Direction, QueryReport, query},
};
use memories::Experience;

pub const DEFAULT_BUDGET: i64 = 1_500;
pub const MAX_BUDGET: i64 = 6_144;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Explain,
    Impact,
}

impl FromStr for Mode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "explain" => Ok(Self::Explain),
            "impact" => Ok(Self::Impact),
            _ => anyhow::bail!("mode 须为 explain 或 impact"),
        }
    }
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Explain => "explain",
            Self::Impact => "impact",
        }
    }
    fn direction(self) -> Direction {
        match self {
            Self::Explain => Direction::Outgoing,
            Self::Impact => Direction::Incoming,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextRequest {
    pub target: String,
    pub mode: Mode,
    pub question: Option<String>,
    pub depth: usize,
    pub budget: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Sections {
    pub identity_and_constraints: Vec<String>,
    pub structure: Vec<String>,
    pub role_and_experience: Vec<String>,
    pub gaps: Vec<String>,
}

type Counts = BTreeMap<&'static str, usize>;

#[derive(Debug, Default, Serialize)]
pub struct Omissions {
    pub budget: Counts,
    pub retrieval: Counts,
}

#[derive(Debug, Serialize)]
pub struct ContextReport {
    pub request: ContextRequest,
    pub text: String,
    pub bytes: usize,
    pub sections: Sections,
    pub omitted: Omissions,
    pub gap_codes: Vec<&'static str>,
    pub structure: Option<QueryReport>,
}

#[derive(Default)]
struct Gaps {
    codes: Vec<&'static str>,
    lines: Vec<String>,
    retrieval: Counts,
}

impl Gaps {
    fn code(&mut self, code: &'static str) {
        if !self.codes.contains(&code) {
            self.codes.push(code);
        }
    }

    fn add(&mut self, code: &'static str, line: String) {
        self.code(code);
        self.lines.push(line);
    }

    fn source<T>(&mut self, code: &'static str, name: &str, result: Result<T>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(e) => {
                self.add(
                    code,
                    format!("{name}未取到：{}", intent::one_line(&e.to_string())),
                );
                None
            }
        }
    }

    fn limited(&mut self, key: &'static str, label: &str, n: usize) {
        if n != 0 {
            self.retrieval.insert(key, n);
            self.add("retrieval_limited", format!("检索省略：{label} {n} 条。"));
        }
    }
}

struct Sources {
    rules: Option<Vec<String>>,
    structure: Option<QueryReport>,
    role: Option<(&'static str, Option<String>)>,
    memories: Vec<Experience>,
    gaps: Gaps,
}

impl Store {
    pub fn context(&self, mut request: ContextRequest) -> Result<ContextReport> {
        ensure!(
            (1..=MAX_BUDGET).contains(&request.budget),
            "预算范围为 1 至 {MAX_BUDGET} 字节"
        );
        ensure!((1..=3).contains(&request.depth), "查询深度须为 1 至 3");
        ensure!(
            request.question.as_ref().is_none_or(|s| s.len() <= 512),
            "question 最多 512 个 UTF-8 字节"
        );
        request.target = query::target_key(&request.target)?;
        let target = &request.target;
        ensure!(
            !redact::sensitive(target)
                && !redact::sensitive(&crate::structure::display_line(target.clone())),
            "目标路径含凭据信息，不能用作定位键"
        );
        // 没给 question 时按文件名补查：带扩展名的名字比主干少撞上无关的词。
        let name = target.rsplit('/').next().unwrap_or(target);
        let asked = request.question.is_some();
        let keywords = request
            .question
            .as_deref()
            .or_else(|| (name.chars().count() >= 3).then_some(name));
        let mut gaps = Gaps::default();
        let (project, rules, structure, role, memories) =
            self.with_intent_project(|db, project| {
                let rules = gaps
                    .source("constraints_failed", "约束", intent::constraints(db))
                    .map(|(lines, limited, invalid)| {
                        gaps.limited("constraints", "约束（每类最多 64 条）", limited);
                        gaps.limited("invalid_constraints", "约束（字段超限或无效）", invalid);
                        lines
                    });
                let structure = gaps.source(
                    "structure_failed",
                    "结构",
                    query::read_snapshot(db, target, request.mode.direction(), request.depth),
                );
                let role = gaps.source("role_failed", "职责", note::read_role(db, project, target));
                let memories = memories::read(self, target, keywords, asked, &mut gaps);
                Ok((project.clone(), rules, structure, role, memories))
            })?;
        let structure = structure.map(|(mut report, basis)| {
            basis.verify(&project, self.conn.path(), &mut report);
            report.conclude();
            report
        });
        let searched = keywords
            .filter(|s| !s.trim().is_empty())
            .map(intent::one_line);
        request.question = request.question.as_deref().map(intent::one_line);
        render::assemble(
            &project,
            request,
            searched.as_deref(),
            Sources {
                rules,
                structure,
                role,
                memories,
                gaps,
            },
        )
    }
}
