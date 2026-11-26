use super::*;

pub(crate) fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c,'\u{061c}'|'\u{200b}'..='\u{200f}'|'\u{2028}'..='\u{202e}'|'\u{2060}'..='\u{206f}'|'\u{feff}')
}

pub(crate) fn clean(s: String) -> String {
    if s.contains(unsafe_char) {
        s.replace(unsafe_char, " ")
    } else {
        s
    }
}

fn line(s: String) -> String {
    let mut s = clean(s);
    s.push('\n');
    s
}

/// 缺口各用一句话说清。数字在覆盖行里，这里不重复。
fn reason(code: &str, incoming: bool) -> &str {
    match code {
        "not_indexed" => "还没有结构快照",
        "absent" => "目标不在这次快照的受管范围里",
        "empty_scope" => "受管范围是空的",
        "target_uncovered" => "目标本身未覆盖",
        "truncated" => "查询撞到上限",
        "unresolved_refs" if incoming => "项目里有引用没能确定目标",
        "unresolved_refs" => "查过的文件里有引用没能确定目标",
        "uncovered_files" => "有文件未覆盖",
        "coverage_unknown" => "覆盖明细未知",
        "changed" => "涉及的文件在快照之后变了",
        "inputs_changed" => "忽略规则或配置在快照之后变了",
        _ => code,
    }
}

impl QueryReport {
    fn snapshot_line(&self) -> String {
        let Some(s) = &self.snapshot else {
            return String::new();
        };
        let time = s.completion_time();
        // 输入指纹只在结构化结果里给：文本里它帮不上读的人，核对行已经说明输入变没变。
        let origin = match (s.generation, s.scope.as_deref()) {
            (Some(g), Some(scope)) => {
                let scope = match scope {
                    "git" => "按 Git 忽略规则",
                    "builtin" => "按内置名单",
                    other => other,
                };
                format!("第 {g} 代，{time}，范围{scope}")
            }
            (Some(g), None) => format!(
                "第 {g} 代，{time}，由旧版写入，没有范围和覆盖明细（执行一次 loom index 可补齐）"
            ),
            (None, _) => {
                format!("旧版生成，{time}，没有代次和覆盖明细（执行一次 loom index 可补齐）")
            }
        };
        let legend = if self.relations.is_empty() {
            ""
        } else {
            "箭头指向被使用方，行号在使用方。"
        };
        line(format!(
            "快照：{origin}；深度 {}，查了 {} 个文件、{} 处引用。{legend}",
            self.depth, self.examined.files, self.examined.refs
        ))
    }

    pub(crate) fn verification_line(&self, paths: bool) -> String {
        let v = &self.verification;
        if v.files.is_empty() {
            // 目标不在快照里时不去探磁盘；还没有快照时由结论行说明。
            return if self.snapshot.is_some() {
                line("核对：目标不在快照里，没有去看磁盘上有没有这个文件。".into())
            } else {
                String::new()
            };
        }
        let same = v.files.iter().filter(|f| f.state == "same").count();
        let mut parts = vec![if same == v.files.len() {
            format!("涉及的 {same} 个文件与快照一致")
        } else {
            format!("涉及的 {} 个文件里 {same} 个与快照一致", v.files.len())
        }];
        for (state, name) in [("changed", "已变化"), ("missing", "已消失")] {
            let matches: Vec<_> = v
                .files
                .iter()
                .filter(|f| f.state == state)
                .map(|f| f.path.as_str())
                .collect();
            if !matches.is_empty() {
                parts.push(if paths {
                    format!("{name}：{}", matches.join("、"))
                } else {
                    format!("{} 个{name}（路径省略）", matches.len())
                });
            }
        }
        let unknown = v.files.iter().filter(|f| f.state == "unverifiable").count();
        if unknown > 0 {
            parts.push(format!("{unknown} 个无法核对"));
        }
        if v.skipped > 0 {
            parts.push(format!("另有 {} 个超出上限没有核对", v.skipped));
        }
        let mut unrecorded = false;
        for i in &v.inputs {
            let status = match i.state {
                "same" => continue,
                "changed" => "已变化",
                "missing" => "已消失",
                "unknown" => {
                    unrecorded = true;
                    continue;
                }
                _ => "无法核对",
            };
            let name = match i.kind {
                "rules" => "忽略规则",
                "ecma" => "JS/TS 配置",
                "go" => "go.mod ",
                "rust" => "Cargo.toml ",
                _ => "包标记 __init__.py ",
            };
            parts.push(format!("{name}{status}"));
        }
        if unrecorded {
            parts.push("快照没有记录忽略规则和配置，无从比较".into());
        }
        if self
            .conclusion
            .gaps
            .iter()
            .any(|s| matches!(*s, "changed" | "inputs_changed"))
        {
            parts.push("先执行 loom index 再采信".into());
        }
        line(format!(
            "核对：{}。其余文件未核对，新增的文件查不出。",
            parts.join("；")
        ))
    }

    /// 只在没有查到关系时给：这时否定结论能说到多远，全看覆盖。查到关系时数字并进结论行。
    pub(crate) fn coverage_line(&self) -> String {
        let c = &self.coverage;
        if self.snapshot.is_none() || self.conclusion.verdict == "found" {
            return String::new();
        }
        let files = if c.managed_files == 0 {
            "受管 0 个文件（空范围）".into()
        } else if c.uncovered_files == 0 {
            format!("受管 {} 个文件，全部已解析", c.managed_files)
        } else {
            format!(
                "受管 {} 个文件，{} 个未覆盖{}",
                c.managed_files,
                c.uncovered_files,
                counts(c.uncovered.iter().flatten())
            )
        };
        let refs = if c.unresolved_refs == 0 {
            "没有未确定的引用".into()
        } else {
            format!(
                "{} 处引用未确定{}",
                c.unresolved_refs,
                counts(c.unresolved.iter().flatten())
            )
        };
        let excluded = match c.excluded_dirs {
            None => "按原因的明细和排除的目录数未知".into(),
            Some(0) => "没有排除目录".into(),
            Some(n) => format!("另有 {n} 个目录被排除，里面没有检查"),
        };
        line(format!("覆盖：{files}，{refs}；{excluded}。"))
    }

    pub(crate) fn conclusion_line(&self) -> String {
        let incoming = self.direction == Direction::Incoming;
        let gaps = &self.conclusion.gaps;
        let reasons = gaps
            .iter()
            .map(|c| reason(c, incoming))
            .collect::<Vec<_>>()
            .join("；");
        line(match self.conclusion.verdict {
            "none_in_snapshot" if incoming => {
                "结论：本次快照的受管范围内没有查到引用它的文件，不代表当前项目里没有。".into()
            }
            "none_in_snapshot" => {
                "结论：本次快照的受管范围内没有查到它依赖的本地文件，不代表当前项目里没有。".into()
            }
            "gap" if gaps == &["not_indexed"] => "结论：还没有结构快照，先执行 loom index。".into(),
            "gap" if incoming => format!("结论：不能判断有没有文件引用它：{reasons}。"),
            "gap" => format!("结论：不能判断它依赖哪些本地文件：{reasons}。"),
            _ => return self.found_line(incoming),
        })
    }

    /// 查到了关系：缺口带上数字说在一行里，不另起覆盖行。
    fn found_line(&self, incoming: bool) -> String {
        let c = &self.coverage;
        let mut parts: Vec<String> = self
            .conclusion
            .gaps
            .iter()
            .map(|code| match *code {
                "unresolved_refs" if incoming => {
                    format!("项目里有 {} 处引用没能确定目标", c.unresolved_refs)
                }
                "unresolved_refs" => {
                    format!("查过的文件里有 {} 处引用没能确定目标", self.issues.len())
                }
                "uncovered_files" => format!("有 {} 个文件未覆盖", c.uncovered_files),
                other => reason(other, incoming).into(),
            })
            .collect();
        let complete = parts.is_empty();
        // 被排除的目录里可能还有使用方；只在查被引用时提。
        if let Some(n) = c.excluded_dirs.filter(|n| incoming && *n > 0) {
            parts.push(format!("另有 {n} 个目录被排除，里面没有检查"));
        }
        if parts.is_empty() {
            return String::new();
        }
        let lead = if complete {
            "结论：以上关系来自快照的受管范围"
        } else {
            "结论：以上关系来自快照，不能判断是否只有这些"
        };
        line(format!("{lead}：{}。", parts.join("；")))
    }

    pub(crate) fn limit_line(&self) -> String {
        if self.truncated {
            line(format!(
                "查询撞到上限（{}），已截短，可能还有更多；请缩小目标或降低深度。",
                self.limits.iter().copied().collect::<Vec<_>>().join("、")
            ))
        } else {
            String::new()
        }
    }

    pub fn text(&self, limit: usize) -> String {
        let side = if self.direction == Direction::Outgoing {
            "依赖"
        } else {
            "被引用"
        };
        let header = |target: &str| line(format!("{target} · {side} · {}", label(&self.status)));
        let mut first = header(&self.target);
        let snapshot = self.snapshot_line();
        let mut checked = self.verification_line(true);
        let coverage = self.coverage_line();
        let conclusion = self.conclusion_line();
        let cut = self.limit_line();
        let mut evidence: Vec<_> = self.relations.iter().map(Relation::line).collect();
        evidence.extend(self.issues.iter().map(Issue::line));
        for s in &self.symbols {
            evidence.push(line(format!(
                "符号 {} · {} · 第 {} 行",
                s.name, s.kind, s.line
            )));
        }
        let omitted = |keep: usize| {
            if keep == evidence.len() {
                return String::new();
            }
            let r = self.relations.len();
            let i = self.issues.len();
            let s = self.symbols.len();
            // 只列确实省略了的几类；全部省略时这一行最长，按它预留空间。
            let parts: Vec<_> = [
                (r.saturating_sub(keep), "条关系"),
                (i.saturating_sub(keep.saturating_sub(r)), "处未确定引用"),
                (s.saturating_sub(keep.saturating_sub(r + i)), "个符号"),
            ]
            .into_iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, unit)| format!("{n} {unit}"))
            .collect();
            line(format!(
                "预览省略 {}；要看全用命令行加 --json，或缩小目标、降低深度。",
                parts.join("、")
            ))
        };
        let fixed = snapshot.len() + coverage.len() + cut.len() + conclusion.len();
        let reserve = fixed + omitted(0).len();
        if reserve + checked.len() + first.len() > limit {
            checked = self.verification_line(false);
            if reserve + checked.len() + first.len() > limit {
                first = header("（目标路径省略）");
            }
        }
        let mut used =
            fixed + first.len() + checked.len() + evidence.iter().map(String::len).sum::<usize>();
        let mut keep = evidence.len();
        let mut notice = omitted(keep);
        while keep > 0 && used + notice.len() > limit {
            keep -= 1;
            used -= evidence[keep].len();
            notice = omitted(keep);
        }
        if used + notice.len() > limit {
            let lines = [first, snapshot, checked, coverage, cut, notice, conclusion];
            let mut keep = [false; 7];
            let mut room = limit;
            for n in [6, 4, 5, 2, 3, 1, 0] {
                if lines[n].len() <= room {
                    keep[n] = true;
                    room -= lines[n].len();
                }
            }
            return lines
                .into_iter()
                .zip(keep)
                .filter_map(|(s, k)| k.then_some(s))
                .collect();
        }
        [
            first,
            snapshot,
            evidence[..keep].concat(),
            checked,
            coverage,
            cut,
            notice,
            conclusion,
        ]
        .concat()
    }
}

impl Snapshot {
    pub(crate) fn completion_time(&self) -> String {
        time::OffsetDateTime::from_unix_timestamp(self.finished_at)
            .map(|t| format!("{} {:02}:{:02} UTC", t.date(), t.hour(), t.minute()))
            .unwrap_or_else(|_| "完成时间未知".into())
    }
}

impl Relation {
    pub(crate) fn line(&self) -> String {
        line(format!(
            "{} → {} · 第 {} 行 · {} · 第 {} 跳",
            self.source,
            self.target,
            self.line,
            label(&self.confidence),
            self.hop
        ))
    }
}

impl Issue {
    pub(crate) fn line(&self) -> String {
        let options = if self.candidates.is_empty() {
            String::new()
        } else {
            format!("：{}", self.candidates.join("、"))
        };
        line(format!(
            "{}:{} · {} · {}{options}",
            self.source,
            self.line,
            self.spelling,
            label(&self.state)
        ))
    }
}

impl fmt::Display for QueryReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text(6_144))
    }
}
