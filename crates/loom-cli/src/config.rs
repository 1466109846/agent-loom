use std::io::{self, BufRead, IsTerminal, Read};

use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use loom_core::config::{self, KEYS, Service, UserConfig};

#[derive(Subcommand)]
pub(super) enum Config {
    /// 显示用户配置文件的位置
    Path,
    /// 列出所有已知配置和模型服务的密钥来源
    List,
    /// 输出一个键的值；没有设置时退出码非零
    Get { key: String },
    /// 校验后写入一个键；配置文件不存在时创建
    Set { key: String, value: String },
    /// 删除一个键
    Unset { key: String },
    /// 从指定环境变量、管道第一行或不回显的终端输入保存密钥
    Secret {
        #[arg(value_name = "embedding|rerank")]
        service: Service,
        #[arg(long, value_name = "变量名", conflicts_with = "clear")]
        from_env: Option<String>,
        #[arg(long)]
        clear: bool,
    },
}

impl Config {
    pub(super) fn run(self) -> Result<()> {
        let file = config::path()?;
        match self {
            Self::Path => println!(
                "{}（{}）",
                file.display(),
                if file.try_exists()? {
                    "存在"
                } else {
                    "不存在"
                }
            ),
            Self::List => {
                let cfg = UserConfig::load_from(&file)?;
                for key in KEYS {
                    println!("{key} = {}", cfg.get(key)?.as_deref().unwrap_or("未设置"));
                }
                for service in [Service::Embedding, Service::Rerank] {
                    println!("{}", cfg.service_status(service)?);
                }
            }
            Self::Get { key } => println!(
                "{}",
                UserConfig::load_from(&file)?.get(&key)?.context("未设置")?
            ),
            Self::Set { key, value } => config::set(&file, &key, &value)?,
            Self::Unset { key } => config::unset(&file, &key)?,
            Self::Secret {
                service,
                from_env,
                clear,
            } => {
                let input = if clear {
                    None
                } else {
                    Some(read_secret(from_env.as_deref())?)
                };
                config::set_secret(&file, service, input.as_deref())?;
                println!("{}", if clear { "已清除" } else { "已保存" });
            }
        }
        Ok(())
    }
}

fn read_secret(from_env: Option<&str>) -> Result<String> {
    if let Some(var) = from_env {
        ensure!(config::valid_env_name(var), "须指定有效的环境变量名");
        return std::env::var(var).map_err(|_| anyhow::anyhow!("无法从指定环境变量取得密钥"));
    }
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return terminal_secret(rpassword::ConfigBuilder::new());
    }
    let mut line = String::new();
    stdin
        .lock()
        .take(515)
        .read_line(&mut line)
        .context("无法读取密钥")?;
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(line)
}

fn terminal_secret(input: rpassword::ConfigBuilder) -> Result<String> {
    rpassword::prompt_password_with_config("请输入密钥（不回显）：", input.build())
        .context("无法读取终端输入")
}

/// clap 默认会在语法错误里逐字引用多余参数，包括用户误放到命令行的密钥。
pub(super) fn parse_error(err: clap::Error) -> ! {
    if err.use_stderr() && std::env::args_os().any(|arg| arg == "config") {
        eprintln!(
            "配置命令参数无效；请用 loom config --help 查看用法。密钥只从环境变量或标准输入读取。"
        );
        std::process::exit(err.exit_code());
    }
    err.exit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_secret_prompt_does_not_echo_or_report_length() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let output = dir.path().join("terminal");
        let value = loom_core::util::new_id();
        let input = rpassword::ConfigBuilder::new()
            .input_reader(io::Cursor::new(format!("{value}\n")))
            .output_writer(std::fs::File::create(&output)?);
        assert!(terminal_secret(input)? == value);
        let text = std::fs::read_to_string(output)?;
        assert!(text.trim() == "请输入密钥（不回显）：");
        Ok(())
    }
}
