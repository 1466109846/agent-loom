//! 写入准入：只收“从代码里看不出来、又能防止出错”的信息。
//!
//! 这里只做便宜的启发式检查，宁可放过也不要误杀：拿不准时接受并给出提示。

use std::sync::LazyLock;

use regex::Regex;

use crate::memory::MemoryType;

pub const MAX_TITLE_CHARS: usize = 120;
pub const MAX_BODY_CHARS: usize = 2000;
pub const MIN_BODY_CHARS: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// 接受，附带给 Agent 的提示（可能为空）。
    Accept(Vec<String>),
    Reject(String),
}

/// 纯粹复述代码事实的句式，例如“foo 函数返回 String”“Bar 结构体有 3 个字段”。
static RESTATES_CODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?i)\s*`?[A-Za-z_][\w:.]*`?\s*(函数|方法|结构体|类|模块|接口|fn|function|method|struct|class)?\s*(返回|接收|包含|有|定义了?|returns?|takes?|has|contains|defines?)\s*[^。.\n]{0,40}[。.]?\s*$",
    )
    .expect("内置正则必须合法")
});

/// 解释“为什么”或“小心什么”的信号词；有这些词时不按“复述代码”拒绝。
static WHY_SIGNALS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(因为|所以|否则|导致|不要|不能|必须|注意|坑|原因|避免|会|why|because|otherwise|must|never|don't|avoid|caus)")
        .expect("内置正则必须合法")
});

pub fn check(ty: MemoryType, title: &str, body: &str) -> Verdict {
    let tlen = title.chars().count();
    let blen = body.chars().count();
    if tlen == 0 {
        return Verdict::Reject("标题不能为空".into());
    }
    if tlen > MAX_TITLE_CHARS {
        return Verdict::Reject(format!("标题 {tlen} 字符，超过上限 {MAX_TITLE_CHARS}，请精简"));
    }
    if blen < MIN_BODY_CHARS {
        return Verdict::Reject(format!(
            "正文只有 {blen} 字符，信息量不足；请写清楚背景、结论和原因"
        ));
    }
    if blen > MAX_BODY_CHARS {
        return Verdict::Reject(format!(
            "正文 {blen} 字符，超过上限 {MAX_BODY_CHARS}；请拆成多条或只保留结论与原因"
        ));
    }
    if RESTATES_CODE.is_match(body) && !WHY_SIGNALS.is_match(body) {
        return Verdict::Reject(
            "内容像是在复述代码本身（读源码就能知道）；只保存读代码推断不出来、且能防止出错的信息".into(),
        );
    }

    let mut hints = Vec::new();
    if ty == MemoryType::Progress {
        hints.push("进度类信息更适合写进任务画布（canvas），它会在任务结束后自然过期".into());
    }
    if matches!(ty, MemoryType::Gotcha | MemoryType::Bugfix | MemoryType::ErrorPattern)
        && !WHY_SIGNALS.is_match(body)
    {
        hints.push("建议补一句原因或后果，方便以后判断这条是否仍然成立".into());
    }
    Verdict::Accept(hints)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_code_restatement() {
        let v = check(MemoryType::CodePattern, "foo", "`foo` 函数返回 String。");
        assert!(matches!(v, Verdict::Reject(_)), "{v:?}");
    }

    #[test]
    fn keeps_restatement_with_reason() {
        let v = check(
            MemoryType::Gotcha,
            "foo 返回值",
            "foo 返回 String，因为调用方会跨 FFI 边界持有它，不能改成 &str。",
        );
        assert!(matches!(v, Verdict::Accept(_)), "{v:?}");
    }

    #[test]
    fn length_limits() {
        assert!(matches!(check(MemoryType::Decision, "", "正文足够长的内容"), Verdict::Reject(_)));
        assert!(matches!(check(MemoryType::Decision, "t", "短"), Verdict::Reject(_)));
        let long = "字".repeat(MAX_BODY_CHARS + 1);
        assert!(matches!(check(MemoryType::Decision, "t", &long), Verdict::Reject(_)));
    }

    #[test]
    fn progress_gets_canvas_hint() {
        let Verdict::Accept(h) = check(MemoryType::Progress, "M0 进度", "完成了存储层与脱敏模块的编写。") else {
            panic!()
        };
        assert!(h.iter().any(|s| s.contains("canvas")));
    }
}
