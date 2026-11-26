//! 写给助手的操作步骤，随程序分发：提示词只要指到 `loom guide`，步骤就跟着版本走。

use clap::ValueEnum;

#[derive(Clone, Copy, Default, ValueEnum)]
pub(super) enum Topic {
    /// 把当前项目接入（默认）
    #[default]
    Setup,
    /// 库放在哪、怎么搬
    Storage,
    /// 可选的向量检索与重排
    Models,
    /// 放进 AGENTS.md 的提示词
    Agents,
}

impl Topic {
    pub(super) fn text(self) -> &'static str {
        match self {
            Self::Setup => include_str!("guide/setup.md"),
            Self::Storage => include_str!("guide/storage.md"),
            Self::Models => include_str!("guide/models.md"),
            Self::Agents => include_str!("guide/agents.md"),
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::{Parser, error::ErrorKind};

    use super::*;

    /// 助手会照着步骤逐条执行，所以里面写的每条 loom 命令都得是这个版本真能解析的。
    #[test]
    fn every_command_in_the_guides_is_accepted_by_this_build() {
        let mut checked = 0;
        for topic in Topic::value_variants() {
            let mut fenced = false;
            for line in topic.text().lines() {
                if line.trim_start().starts_with("```") {
                    fenced = !fenced;
                    continue;
                }
                // 代码块里的整行必须完整可执行；行内提到的命令可以省略参数。
                let spans: Vec<_> = if fenced {
                    vec![(line.trim(), true)]
                } else {
                    line.split('`')
                        .skip(1)
                        .step_by(2)
                        .map(|s| (s, false))
                        .collect()
                };
                for (span, complete) in spans {
                    if !span.starts_with("loom ") {
                        continue;
                    }
                    let args = span.split_whitespace().map(|word| match word {
                        "<键>" => "storage.data_dir",
                        w if w.starts_with('<') => "x",
                        w => w,
                    });
                    match crate::Cli::try_parse_from(args).map_err(|e| (e.kind(), e)) {
                        Ok(_) | Err((ErrorKind::DisplayVersion, _)) => (),
                        Err((ErrorKind::MissingRequiredArgument, _)) if !complete => (),
                        Err((_, e)) => panic!("{span}\n{e}"),
                    }
                    checked += 1;
                }
            }
            assert!(!fenced, "代码块没有闭合");
        }
        assert!(checked >= 30, "只核对到 {checked} 条命令，提取规则可能失效");
    }

    /// 中文手册里供复制的提示词必须与 `loom guide agents` 输出一致。
    #[test]
    fn chinese_readme_carries_the_same_prompt_as_the_agents_topic() {
        let readme = include_str!("../../../README.zh-CN.md").replace("\r\n", "\n");
        let prompt = Topic::Agents.text().replace("\r\n", "\n");
        assert!(readme.contains(prompt.trim()));
    }
}
