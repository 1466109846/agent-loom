//! CLI 只编辑已知键；读取不要求同一节的其他键已经配齐。
use std::net::IpAddr;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use toml_edit::{DocumentMut, Item, Table, Value};

use super::{MAX_BYTES, RawEndpoint, SECRETS, UserConfig, read_text};
use crate::util::atomic_write;

pub(super) fn read_rerank<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<Option<RawEndpoint>, D::Error> {
    use serde::Deserialize;
    // 两个服务共用运行时类型；仅 embedding 接受维度和批大小，rerank 的未知键不参与反序列化。
    let mut table = toml::Table::deserialize(de)?;
    table.remove("dimensions");
    table.remove("batch");
    table.try_into().map(Some).map_err(serde::de::Error::custom)
}

pub const KEYS: &[&str] = &[
    "storage.data_dir",
    "embedding.base_url",
    "embedding.model",
    "embedding.api_key_env",
    "embedding.dimensions",
    "embedding.timeout_ms",
    "embedding.batch",
    "rerank.base_url",
    "rerank.model",
    "rerank.api_key_env",
    "rerank.timeout_ms",
];

fn parts(key: &str) -> Result<(&str, &str)> {
    ensure!(
        KEYS.contains(&key),
        "未知配置键；可用 loom config list 查看"
    );
    Ok(key.split_once('.').expect("已知键都有节名"))
}

pub fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b == b'_' || b.is_ascii_alphabetic())
        && bytes.all(|b| b == b'_' || b.is_ascii_alphanumeric())
}

pub(super) fn base_url(input: &str) -> Result<(&str, bool)> {
    const INVALID: &str = "base_url 须是 http 或 https 地址，不能含凭据、查询串或片段";
    ensure!(
        !input.contains(['?', '#', '\\'])
            && !input.contains(|c: char| c.is_whitespace() || c.is_control()),
        INVALID
    );
    let uri = input
        .parse::<hyper::Uri>()
        .map_err(|_| anyhow::anyhow!(INVALID))?;
    ensure!(matches!(uri.scheme_str(), Some("http" | "https")), INVALID);
    let authority = uri.authority().context(INVALID)?;
    let host = authority.host();
    ensure!(
        !host.is_empty() && !authority.as_str().contains('@'),
        INVALID
    );
    ensure!(
        authority.as_str() == host || authority.port_u16().is_some(),
        INVALID
    );
    let addr = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>();
    ensure!(
        !host.starts_with('[') || matches!(addr, Ok(IpAddr::V6(_))),
        INVALID
    );
    let local = host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
        || addr.is_ok_and(|ip| ip.to_canonical().is_loopback());
    Ok((input.trim_end_matches('/'), local))
}

fn validated(field: &str, input: &str) -> Result<Value> {
    Ok(match field {
        "data_dir" => {
            ensure!(
                Path::new(input).is_absolute() && !input.contains(char::is_control),
                "storage.data_dir 须是绝对路径"
            );
            input.into()
        }
        "base_url" => base_url(input)?.0.into(),
        "model" => {
            ensure!(
                !input.trim().is_empty() && input.len() <= 128 && !input.contains(char::is_control),
                "model 须非空、不超过 128 字节且不含控制字符"
            );
            input.trim().into()
        }
        "api_key_env" => {
            ensure!(valid_env_name(input), "api_key_env 须是环境变量名");
            input.into()
        }
        field => {
            let range = match field {
                "dimensions" => 1..=4096,
                "timeout_ms" => 500..=60_000,
                "batch" => 1..=64,
                _ => unreachable!(),
            };
            input
                .parse::<i64>()
                .ok()
                .filter(|n| range.contains(n))
                .with_context(|| {
                    format!(
                        "{field} 须是 {} 到 {} 之间的整数",
                        range.start(),
                        range.end()
                    )
                })?
                .into()
        }
    })
}

#[derive(Clone, Copy)]
pub enum Service {
    Embedding,
    Rerank,
}

impl Service {
    pub fn name(self) -> &'static str {
        match self {
            Self::Embedding => "embedding",
            Self::Rerank => "rerank",
        }
    }
}

impl std::str::FromStr for Service {
    type Err = anyhow::Error;
    fn from_str(name: &str) -> Result<Self> {
        match name {
            "embedding" => Ok(Self::Embedding),
            "rerank" => Ok(Self::Rerank),
            _ => bail!("模型服务须是 embedding 或 rerank"),
        }
    }
}

impl UserConfig {
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        let (section, field) = parts(key)?;
        let raw: Option<&RawEndpoint> = match section {
            "storage" => return Ok(self.data_dir.as_ref().map(|p| p.display().to_string())),
            "embedding" => self.embedding.as_ref(),
            "rerank" => self.rerank.as_ref(),
            _ => unreachable!(),
        };
        let Some(raw) = raw else { return Ok(None) };
        let value = match field {
            "base_url" => raw.base_url.clone(),
            "model" => raw.model.clone(),
            "api_key_env" => raw.api_key_env.clone(),
            "dimensions" => raw.dimensions.map(|n| n.to_string()),
            "batch" => raw.batch.map(|n| n.to_string()),
            "timeout_ms" => raw.timeout_ms.map(|n| n.to_string()),
            _ => unreachable!(),
        };
        // 不原样输出手写的未校验 URL，它可能含有口令。
        value
            .map(|s| validated(field, &s).map(|v| v.as_str().map_or(s, str::to_owned)))
            .transpose()
    }

    pub fn service_status(&self, service: Service) -> Result<String> {
        let (raw, secret) = match service {
            Service::Embedding => (&self.embedding, &self.secrets.embedding),
            Service::Rerank => (&self.rerank, &self.secrets.rerank),
        };
        let location = match raw.as_ref().and_then(|r| r.base_url.as_deref()) {
            None => "地址未设置",
            Some(url) if base_url(url)?.1 => "本机",
            Some(_) => "云端",
        };
        let stored = if secret
            .api_key
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
        {
            "凭据文件"
        } else {
            "没有"
        };
        let source = match raw.as_ref().and_then(|r| r.api_key_env.as_deref()) {
            Some(var) => {
                ensure!(valid_env_name(var), "api_key_env 须是环境变量名");
                if std::env::var(var).is_ok_and(|v| !v.trim().is_empty()) {
                    format!("环境变量 {var}（有值）")
                } else {
                    format!("环境变量 {var}（无值）；{stored}")
                }
            }
            None => stored.into(),
        };
        Ok(format!("{}：{location} · 密钥：{source}", service.name()))
    }
}

fn edit(file: &Path, section: &str, field: &str, value: Option<Value>) -> Result<()> {
    if value.is_none() && !file.try_exists()? {
        return Ok(());
    }
    let dir = file.parent().context("缺少配置目录")?;
    std::fs::create_dir_all(dir)?;
    // 文件本身会被改名替换；锁独立于旧文件，避免两个 set 同时读旧配置、互相覆盖。
    let mut lock_name = file.as_os_str().to_owned();
    lock_name.push(".lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_name)?;
    lock.lock()?;
    let mut doc: DocumentMut = read_text(file)?
        .parse()
        .map_err(|_| anyhow::anyhow!("用户配置格式有误，未写入"))?;
    if value.is_none() && !doc.contains_key(section) {
        return Ok(());
    }
    let section_table = doc
        .entry(section)
        .or_insert(Item::Table(Table::new()))
        .as_table_like_mut()
        .context("配置节不是表，未写入")?;
    if let Some(mut v) = value {
        if let Some(slot) = section_table.get_mut(field) {
            if let Some(old) = slot.as_value() {
                *v.decor_mut() = old.decor().clone();
            }
            *slot = Item::Value(v);
        } else {
            section_table.insert(field, Item::Value(v));
        }
    } else {
        if section_table.remove(field).is_none() {
            return Ok(());
        }
        if section_table.is_empty() {
            doc.remove(section);
        }
    }
    let output = doc.to_string();
    ensure!(
        output.len() as u64 <= MAX_BYTES,
        "用户配置超过 64 KiB，未写入"
    );
    atomic_write(file, output.as_bytes())
}

pub fn set(file: &Path, key: &str, input: &str) -> Result<()> {
    let (section, field) = parts(key)?;
    edit(file, section, field, Some(validated(field, input)?))
}

pub fn unset(file: &Path, key: &str) -> Result<()> {
    let (section, field) = parts(key)?;
    edit(file, section, field, None)
}

pub fn set_secret(config: &Path, service: Service, input: Option<&str>) -> Result<()> {
    if let Some(s) = input {
        ensure!(
            !s.is_empty()
                && s.len() <= 512
                && !s.contains(|c: char| c.is_whitespace() || c.is_control()),
            "密钥无效，未保存"
        );
    }
    edit(
        &config.with_file_name(SECRETS),
        service.name(),
        "api_key",
        input.map(Value::from),
    )
}
