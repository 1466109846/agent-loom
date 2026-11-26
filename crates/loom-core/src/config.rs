//! 用户级配置：一份 TOML，记这台机器上的偏好（库放哪、用哪个模型），不随项目走。
//! 读取从不创建文件。项目内容不进这里，这里的地址和密钥也不进项目库。

use std::{
    env, fmt, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

mod settings;
pub use settings::{KEYS, Service, set, set_secret, unset, valid_env_name};

const SECRETS: &str = "credentials.toml";
const MAX_BYTES: u64 = 64 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 8_000;
const DEFAULT_BATCH: usize = 10;

/// 配置文件的位置：`LOOM_CONFIG` 指定的文件，否则在系统约定的用户配置目录里。
pub fn path() -> Result<PathBuf> {
    if let Some(file) = env::var_os("LOOM_CONFIG").filter(|s| !s.is_empty()) {
        let file = PathBuf::from(file);
        ensure!(file.is_absolute(), "LOOM_CONFIG 须是配置文件的绝对路径");
        ensure!(
            file.file_name()
                .is_some_and(|s| !s.eq_ignore_ascii_case(SECRETS)),
            "LOOM_CONFIG 不能指向凭据文件"
        );
        return Ok(file);
    }
    let set = |name: &str| {
        env::var_os(name)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    };
    let dir = if cfg!(windows) {
        set("APPDATA")
    } else {
        set("XDG_CONFIG_HOME").or_else(|| set("HOME").map(|home| home.join(".config")))
    };
    let dir = dir.context("找不到用户配置目录；请用 LOOM_CONFIG 指定配置文件")?;
    ensure!(dir.is_absolute(), "用户配置目录不是绝对路径");
    Ok(dir.join("loom").join("config.toml"))
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Raw {
    storage: RawStorage,
    embedding: Option<RawEndpoint>,
    #[serde(deserialize_with = "settings::read_rerank")]
    rerank: Option<RawEndpoint>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawStorage {
    data_dir: Option<PathBuf>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
struct RawEndpoint {
    base_url: Option<String>,
    model: Option<String>,
    api_key_env: Option<String>,
    dimensions: Option<u32>,
    timeout_ms: Option<u64>,
    batch: Option<usize>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawSecrets {
    embedding: RawSecret,
    rerank: RawSecret,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawSecret {
    api_key: Option<String>,
}

/// 一个模型服务的地址和参数。密钥只在发请求时取，不出现在调试输出里。
#[derive(Clone)]
pub struct Endpoint {
    /// 不带结尾斜杠，如 `https://api.example.com/v1`。
    pub base_url: String,
    pub model: String,
    pub dimensions: Option<u32>,
    pub timeout: Duration,
    pub batch: usize,
    api_key_env: Option<String>,
    stored_key: Option<String>,
}

impl Endpoint {
    /// 环境变量优先，其次是凭据文件；都没有时不带密钥（本机服务通常不需要）。
    pub fn api_key(&self) -> Option<String> {
        self.api_key_env
            .as_deref()
            .and_then(|name| env::var(name).ok())
            .filter(|key| !key.trim().is_empty())
            .or_else(|| self.stored_key.clone())
            .map(|key| key.trim().to_owned())
            .filter(|key| !key.is_empty())
    }

    /// 密钥从哪个环境变量取；只给名字，不给值。
    pub fn api_key_env(&self) -> Option<&str> {
        self.api_key_env.as_deref()
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("dimensions", &self.dimensions)
            .field("api_key", &self.api_key().map(|_| "已设置"))
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
pub struct UserConfig {
    /// 新绑定的项目把库建在 `<data_dir>/<项目标识>/loom.db`；没设时建在项目的 `.loom` 里。
    pub data_dir: Option<PathBuf>,
    embedding: Option<RawEndpoint>,
    rerank: Option<RawEndpoint>,
    secrets: RawSecrets,
}

fn read_text(file: &Path) -> Result<String> {
    let meta = match fs::symlink_metadata(file) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(e).context("用户配置不可读取"),
    };
    ensure!(
        meta.is_file() && meta.len() <= MAX_BYTES,
        "用户配置须是不超过 64 KiB 的普通文件"
    );
    fs::read_to_string(file).context("用户配置不可读取")
}

fn read<T: for<'a> Deserialize<'a>>(file: &Path) -> Result<T> {
    // 解析错误可能回显文件内容，凭据文件也走这里，所以只报位置不报原文。
    toml::from_str(&read_text(file)?).map_err(|e| match e.span() {
        Some(span) => anyhow::anyhow!("用户配置格式有误（第 {} 字节附近）", span.start),
        None => anyhow::anyhow!("用户配置格式有误"),
    })
}

impl UserConfig {
    /// 没有配置文件等于全部默认：库在项目里，模型不启用。
    pub fn load() -> Result<Self> {
        Self::load_from(&path()?)
    }

    pub fn load_from(file: &Path) -> Result<Self> {
        let raw: Raw = read(file)?;
        if let Some(dir) = &raw.storage.data_dir {
            ensure!(dir.is_absolute(), "storage.data_dir 须是绝对路径");
        }
        Ok(Self {
            data_dir: raw.storage.data_dir,
            embedding: raw.embedding,
            rerank: raw.rerank,
            secrets: read(&file.with_file_name(SECRETS))?,
        })
    }

    /// 向量模型；没写这一节返回 `None`，写了但不完整或取值越界时报错。
    pub fn embedding(&self) -> Result<Option<Endpoint>> {
        endpoint(
            "embedding",
            self.embedding.as_ref(),
            &self.secrets.embedding,
        )
    }

    /// 重排模型，规则同上。
    pub fn rerank(&self) -> Result<Option<Endpoint>> {
        endpoint("rerank", self.rerank.as_ref(), &self.secrets.rerank)
    }
}

fn text(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn endpoint(name: &str, raw: Option<&RawEndpoint>, secret: &RawSecret) -> Result<Option<Endpoint>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let (Some(url), Some(model)) = (text(&raw.base_url), text(&raw.model)) else {
        bail!("[{name}] 需要同时写 base_url 和 model");
    };
    let (url, _) = settings::base_url(url)?;
    ensure!(
        model.len() <= 128 && !model.contains(char::is_control),
        "{name}.model 无效"
    );
    let timeout = raw.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let batch = raw.batch.unwrap_or(DEFAULT_BATCH);
    ensure!(
        raw.dimensions.is_none_or(|d| (1..=4096).contains(&d)),
        "{name}.dimensions 须在 1 到 4096 之间"
    );
    ensure!(
        (500..=60_000).contains(&timeout),
        "{name}.timeout_ms 须在 500 到 60000 之间"
    );
    ensure!((1..=64).contains(&batch), "{name}.batch 须在 1 到 64 之间");
    if let Some(var) = &raw.api_key_env {
        ensure!(valid_env_name(var), "{name}.api_key_env 须是环境变量名");
    }
    Ok(Some(Endpoint {
        base_url: url.to_owned(),
        model: model.to_owned(),
        dimensions: raw.dimensions,
        timeout: Duration::from_millis(timeout),
        batch,
        api_key_env: text(&raw.api_key_env).map(str::to_owned),
        stored_key: secret.api_key.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_all_defaults_and_keys_never_reach_debug_output() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("config.toml");
        let none = UserConfig::load_from(&file)?;
        assert!(none.data_dir.is_none() && none.embedding()?.is_none() && none.rerank()?.is_none());
        assert!(!file.exists());

        fs::write(
            &file,
            "[embedding]\nbase_url = \"https://api.example.com/v1/\"\nmodel = \"BAAI/bge-m3\"\n[rerank]\nmodel = \"x\"\n",
        )?;
        let key = crate::util::new_id();
        fs::write(
            dir.path().join(SECRETS),
            format!("[embedding]\napi_key = '{key}'\n"),
        )?;
        let cfg = UserConfig::load_from(&file)?;
        let e = cfg.embedding()?.unwrap();
        assert_eq!(
            (e.base_url.as_str(), e.batch),
            ("https://api.example.com/v1", DEFAULT_BATCH)
        );
        assert!(e.api_key().as_deref() == Some(key.as_str()));
        assert!(!format!("{e:?}").contains(&key));
        assert!(cfg.rerank().unwrap_err().to_string().contains("base_url"));
        Ok(())
    }
}
