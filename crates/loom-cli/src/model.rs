use anyhow::{Result, ensure};
use clap::Subcommand;
use loom_core::{config::UserConfig, enhance::models::Models};
use std::time::Instant;

#[derive(Subcommand)]
pub(crate) enum Model {
    /// 实测已配置的向量与重排接口，不需要项目
    Test,
    /// 让本机 Ollama 下载缺少的向量模型
    Pull,
}
pub(crate) fn configured() -> Models {
    Models::from_config(UserConfig::load())
}
impl Model {
    pub(crate) fn run(&self) -> Result<()> {
        let models = configured();
        let mut failed = false;
        for (label, c, embed) in [
            ("向量", models.embedding, true),
            ("重排", models.rerank, false),
        ] {
            let svc = match c {
                Ok(Some(s)) => s,
                Ok(None) => {
                    println!("{label}：未配置");
                    continue;
                }
                Err(e) => {
                    println!("{label}：{e}");
                    failed = true;
                    continue;
                }
            };
            let start = Instant::now();
            // 服务每秒报很多次进度：只在跨过 5% 的刻度时打一行，分层下载时各层各打一遍。
            let mut shown = None;
            let result = match self {
                Self::Test => svc.probe(embed),
                Self::Pull => svc
                    .pull(embed, |pct| {
                        let step = pct / 5 * 5;
                        if shown.replace(step) != Some(step) {
                            eprintln!("{label}下载：{step}%");
                        }
                    })
                    .map(str::to_owned),
            };
            let detail = result.unwrap_or_else(|e| {
                failed = true;
                format!("{e}，用时 {} ms", start.elapsed().as_millis())
            });
            println!(
                "{label}：{}，密钥来源：{}；{detail}",
                svc.description(),
                svc.key_source()
            );
        }
        ensure!(!failed, "模型操作未全部通过");
        Ok(())
    }
}
