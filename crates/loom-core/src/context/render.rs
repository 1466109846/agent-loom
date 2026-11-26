use super::*;
use crate::{project::ProjectIdentity, structure::display_line};

const RULE: usize = 0;
const EXACT: usize = 1;
const LIKELY: usize = 2;
const ROLE: usize = 3;
const MEMORY: usize = 4;
const BODY: usize = 5;
const ISSUE: usize = 6;
const GROUPS: [(&str, &str); 7] = [
    ("constraints", "约束"),
    ("exact", "确定关系"),
    ("likely", "可能关系"),
    ("role_body", "职责正文"),
    ("memories", "经验"),
    ("memory_bodies", "经验正文"),
    ("issues", "未确定引用"),
];
const FIRST: [usize; 5] = [4, 6, 3, 1, 3];
const RULE_WARNING: &str = "还有约束未展示，请先查看（overview）。";

fn safe(s: &str) -> String {
    redact::redact(&display_line(redact::redact(s).0)).0
}

fn argument(s: &str) -> String {
    if s.contains(crate::structure::unsafe_char) {
        // 续查里的定位键必须能原样还原，不能把不可见字符当作文件名里的空格。
        use std::fmt::Write;
        let mut out = String::from("$'");
        for c in s.chars() {
            if crate::structure::unsafe_char(c) {
                let _ = write!(out, r"\U{:08x}", c as u32);
            } else {
                if matches!(c, '\\' | '\'') {
                    out.push('\\');
                }
                out.push(c);
            }
        }
        out.push('\'');
        return out;
    }
    if !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || "_-./".contains(c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// `more` 是还有命中没展示时的关键词：同样的词再查一遍，只有这时才值得。
fn follow(r: &ContextRequest, more: Option<&str>, memories: &[Experience]) -> String {
    let target = argument(&r.target);
    let mode = r.mode.name();
    let depth = (r.depth + 1).min(3);
    let (structure, note) = if r.target.starts_with('-') {
        (
            format!("{mode} --depth {depth} -- {target}"),
            format!("note get -- {target}"),
        )
    } else {
        (
            format!("{mode} {target} --depth {depth}"),
            format!("note get {target}"),
        )
    };
    let mut commands = vec![structure, note];
    if let Some(words) = more {
        let end = if words.starts_with('-') { "-- " } else { "" };
        commands.push(format!("recall {end}{}", argument(words)));
    }
    if let Some(m) = memories.first() {
        commands.push(format!("manage get {}", m.id));
    }
    format!("续查：{}", commands.join("；"))
}

#[derive(Default)]
struct Rows(Vec<(String, bool)>);

impl Rows {
    fn push(&mut self, line: String) {
        self.0.push((line, false));
    }
    fn shown(&self) -> impl Iterator<Item = &String> {
        self.0.iter().filter_map(|(s, keep)| keep.then_some(s))
    }
}

struct Layout {
    fixed: Sections,
    groups: [Rows; 7],
    incomplete_rules: bool,
    full_heading: Option<String>,
    follow: String,
}

impl Sections {
    fn lines(&self) -> impl Iterator<Item = &String> {
        self.identity_and_constraints
            .iter()
            .chain(&self.structure)
            .chain(&self.role_and_experience)
            .chain(&self.gaps)
    }
    fn bytes(&self) -> usize {
        self.lines().map(|s| s.len() + 1).sum()
    }
}

impl Layout {
    fn new(p: &ProjectIdentity, r: &ContextRequest, words: Option<&str>, s: &mut Sources) -> Self {
        let mut groups: [Rows; 7] = Default::default();
        let mut fixed = Sections::default();
        let heading = p.heading();
        let full_heading = (safe(&heading) == heading).then_some(heading);
        let id = &mut fixed.identity_and_constraints;
        id.push(format!("项目：标识={}", p.id()));
        id.push(format!(
            "任务：{} {} · 深度 {}",
            r.mode.name(),
            safe(&r.target),
            r.depth
        ));
        let incomplete_rules = ["constraints", "invalid_constraints"]
            .iter()
            .any(|k| s.gaps.retrieval.contains_key(k));
        id.push(
            match s.rules.take() {
                None => "约束：未取到",
                Some(lines) if lines.is_empty() && !incomplete_rules => "约束：没有",
                Some(lines) => {
                    groups[RULE].0 = lines.into_iter().map(|l| (l, false)).collect();
                    "约束"
                }
            }
            .into(),
        );

        let gaps = &mut fixed.gaps;
        gaps.push("缺口".into());
        if let Some(q) = &s.structure {
            for e in &q.relations {
                let group = if e.confidence == "exact" {
                    EXACT
                } else {
                    LIKELY
                };
                groups[group].push(safe(e.line().trim_end_matches('\n')));
            }
            for i in &q.issues {
                groups[ISSUE].push(safe(i.line().trim_end_matches('\n')));
            }
            let mut status = "结构：".to_owned();
            if let Some(snapshot) = &q.snapshot {
                let generation = snapshot
                    .generation
                    .map_or_else(|| "旧版，无代次".into(), |g| format!("第 {g} 代"));
                status.push_str(&format!(
                    "快照{generation}，{}；",
                    snapshot.completion_time()
                ));
            }
            let conclusion = q.conclusion_line();
            if q.conclusion.verdict == "found" {
                status.push_str(&format!(
                    "静态确定 {} 条，可能 {} 条",
                    groups[EXACT].0.len(),
                    groups[LIKELY].0.len()
                ));
            } else if q.conclusion.verdict == "none_in_snapshot" {
                status.push_str(conclusion.trim().trim_start_matches("结论："));
            } else {
                if q.status != "indexed" {
                    status.push_str(crate::structure::label(&q.status));
                    status.push('；');
                }
                status.push_str("不能判断");
            }
            fixed.structure.push(status);
            let lines = [
                q.coverage_line(),
                q.limit_line(),
                if q.conclusion.verdict == "none_in_snapshot" {
                    String::new()
                } else {
                    conclusion
                },
                q.verification_line(true),
            ];
            gaps.extend(
                lines
                    .into_iter()
                    .filter(|l| !l.is_empty())
                    .map(|l| safe(l.trim_end_matches('\n'))),
            );
            for &code in &q.conclusion.gaps {
                s.gaps.code(code);
            }
            if q.verification.skipped > 0
                || q.verification
                    .files
                    .iter()
                    .any(|f| f.state == "unverifiable")
                || q.verification
                    .inputs
                    .iter()
                    .any(|i| matches!(i.state, "unknown" | "unverifiable"))
            {
                s.gaps.code("verification_partial");
            }
        } else {
            fixed.structure.push("结构：未取到，不能判断".into());
        }

        let knowledge = &mut fixed.role_and_experience;
        knowledge.push("职责与经验".into());
        let (status, body) = s.role.take().unwrap_or(("未取到", None));
        knowledge.push(format!("职责：{status}"));
        if let Some(code) = match status {
            "需复核" => Some("role_review"),
            "文件消失" => Some("role_missing"),
            "未覆盖或不可校验" => Some("role_unverifiable"),
            _ => None,
        } {
            s.gaps.add(code, format!("职责：{status}。"));
        }
        if let Some(body) = body
            && matches!(status, "当前" | "需复核")
        {
            let label = if status == "当前" {
                "职责（当前）"
            } else {
                "历史职责（需复核）"
            };
            groups[ROLE].push(format!("{label}：{}", intent::one_line(&body)));
        }
        let more = words.filter(|_| {
            s.gaps.retrieval.contains_key("keywords")
                || s.gaps.retrieval.contains_key("folded")
                || s.gaps.codes.contains(&"keyword_window")
        });
        let follow = follow(r, more, &s.memories);
        if s.memories.is_empty() {
            let failed = s
                .gaps
                .codes
                .iter()
                .any(|c| matches!(*c, "anchors_failed" | "keywords_failed"));
            // 说清查过什么，“没有”才有边界。question 可能很长，不回显。
            knowledge.push(match (failed, words, r.question.is_some()) {
                (true, ..) => "经验：未取全".into(),
                (_, None, _) => "经验：没有锚定在本文件上的".into(),
                (_, Some(_), true) => "经验：没有锚定在本文件上的，按 question 也没查到".into(),
                (_, Some(name), false) => {
                    format!("经验：没有锚定在本文件上的，按文件名 {name} 也没查到")
                }
            });
        }
        for m in s.memories.drain(..) {
            groups[MEMORY].push(m.line);
            groups[BODY].push(format!("  正文：{}", m.body));
        }
        gaps.append(&mut s.gaps.lines);
        Self {
            fixed,
            groups,
            incomplete_rules,
            full_heading,
            follow,
        }
    }

    fn view(&self) -> (Sections, Counts) {
        let g = &self.groups;
        let mut sections = self.fixed.clone();
        let mut omitted = Counts::new();
        let mut labels = Vec::new();
        for (i, (key, label)) in GROUPS.into_iter().enumerate() {
            let total = if i == BODY {
                g[MEMORY].shown().count()
            } else {
                g[i].0.len()
            };
            let n = total - g[i].shown().count();
            if n != 0 {
                omitted.insert(key, n);
                labels.push(format!("{label} {n} 条"));
            }
        }
        let id = &mut sections.identity_and_constraints;
        id.extend(g[RULE].shown().cloned());
        if omitted.contains_key("constraints") || self.incomplete_rules {
            id.push(RULE_WARNING.into());
        }
        for (i, label) in [(EXACT, "静态确定"), (LIKELY, "可能"), (ISSUE, "未确定引用")]
        {
            let n = g[i].shown().count();
            if n > 0 {
                sections.structure.push(format!("{label}（{n} 条）"));
                sections.structure.extend(g[i].shown().cloned());
            }
        }
        let k = &mut sections.role_and_experience;
        if let Some(role) = g[ROLE].shown().next() {
            k[1].clone_from(role);
        }
        for ((title, keep), (body, full)) in g[MEMORY].0.iter().zip(&g[BODY].0) {
            if *keep {
                k.push(title.clone());
                if *full {
                    k.push(body.clone());
                }
            }
        }
        if !labels.is_empty() {
            sections
                .gaps
                .push(format!("预算省略：{}。", labels.join("、")));
        }
        if sections.gaps.len() == 1 {
            sections.gaps.push("没有。".into());
        }
        sections.gaps.push(self.follow.clone());
        (sections, omitted)
    }

    fn try_row(&mut self, i: usize, n: usize, budget: usize) {
        let row = &mut self.groups[i].0[n];
        if row.1 || row.0.len() > budget {
            return;
        }
        row.1 = true;
        if self.view().0.bytes() > budget {
            self.groups[i].0[n].1 = false;
        }
    }

    fn pack(&mut self, budget: usize) -> Result<()> {
        // 只有一两条短证据时，展示整组可能比给省略计数和红线警告更省。
        let mut minimum = self.view().0.bytes();
        for i in [RULE, ROLE] {
            for row in &mut self.groups[i].0 {
                row.1 = true;
            }
            let size = self.view().0.bytes();
            if size <= minimum {
                minimum = size;
            } else {
                for row in &mut self.groups[i].0 {
                    row.1 = false;
                }
            }
        }
        ensure!(
            minimum <= MAX_BUDGET as usize,
            "必要内容共 {minimum} 字节，超过 {MAX_BUDGET} 字节上限；请缩短目标或缩小查询范围"
        );
        ensure!(
            budget >= minimum,
            "预算 {budget} 字节不够，这次至少需要 {minimum} 字节"
        );
        // 首轮先照顾能共存的区，避免单条巨大的旧标题占尽余额。
        let mut first: Vec<_> = (0..FIRST.len())
            .filter_map(|i| self.groups[i].0.first().map(|(s, _)| (s.len(), i)))
            .collect();
        first.sort_unstable();
        for (_, i) in first {
            self.try_row(i, 0, budget);
        }
        for n in 1..6 {
            for (i, limit) in FIRST.into_iter().enumerate() {
                if n < limit && n < self.groups[i].0.len() {
                    self.try_row(i, n, budget);
                }
            }
        }
        if let Some(full) = self.full_heading.take() {
            let heading = &self.fixed.identity_and_constraints[0];
            if self.view().0.bytes() - heading.len() + full.len() <= budget {
                self.fixed.identity_and_constraints[0] = full;
            }
        }
        for i in 0..GROUPS.len() {
            if i == BODY
                && FIRST
                    .into_iter()
                    .enumerate()
                    .any(|(j, n)| self.groups[j].0.iter().take(n).any(|r| !r.1))
            {
                continue;
            }
            for n in 0..self.groups[i].0.len() {
                if i != BODY || self.groups[MEMORY].0[n].1 {
                    self.try_row(i, n, budget);
                }
            }
        }
        Ok(())
    }
}

pub(super) fn assemble(
    p: &ProjectIdentity,
    request: ContextRequest,
    keywords: Option<&str>,
    mut s: Sources,
) -> Result<ContextReport> {
    let mut layout = Layout::new(p, &request, keywords, &mut s);
    layout.pack(request.budget as usize)?;
    let (sections, budget) = layout.view();
    if !budget.is_empty() {
        s.gaps.code("budget_omitted");
    }
    let mut text = String::with_capacity(sections.bytes());
    for l in sections.lines() {
        text.push_str(l);
        text.push('\n');
    }
    Ok(ContextReport {
        request,
        bytes: text.len(),
        text,
        sections,
        omitted: Omissions {
            budget,
            retrieval: s.gaps.retrieval,
        },
        gap_codes: s.gaps.codes,
        structure: s.structure,
    })
}
