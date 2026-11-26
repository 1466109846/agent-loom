use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Subcommand;
use loom_core::config::UserConfig;
use loom_core::project::{Bind, ProjectIdentity, ProjectLocation};
use loom_core::util::{display_path, fmt_size as size};

#[derive(Subcommand)]
pub(super) enum Db {
    /// 将项目库搬到目标文件或目录；默认只预演
    Move {
        target: PathBuf,
        #[arg(long)]
        yes: bool,
    },
}

pub(super) fn bind(project: &ProjectLocation, mode: Bind<'_>) -> Result<ProjectIdentity> {
    let dir = if matches!(mode, Bind::Initialize) && project.needs_data_dir()? {
        UserConfig::load()?.data_dir
    } else {
        None
    };
    project.bind_with_data_dir(mode, dir.as_deref())
}

pub(super) fn print_location(path: &Path) -> Result<()> {
    println!(
        "库：{} · {}",
        display_path(path),
        size(path.metadata()?.len())
    );
    Ok(())
}

impl Db {
    pub(super) fn run(self, project: &ProjectLocation) -> Result<()> {
        let Self::Move { target, yes } = self;
        let report = project.move_database(&target, yes)?;
        if yes {
            println!(
                "新库已生效：{} · {}",
                display_path(&report.target),
                size(report.bytes)
            );
            for file in report.retained {
                println!("未能删除旧文件：{}", display_path(&file));
            }
        } else {
            println!(
                "当前库：{} · {}",
                display_path(project.database()),
                size(report.bytes)
            );
            println!(
                "目标：{}\n执行前请结束正在使用这个项目的会话；加 --yes 执行",
                display_path(&report.target)
            );
        }
        Ok(())
    }
}
