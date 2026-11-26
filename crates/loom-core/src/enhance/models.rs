//! 仅供显式启用的项目模型使用，不读取旧增强的环境开关。
use crate::{
    config::{Endpoint, UserConfig},
    util::sha256_hex,
};
use hyper::Uri;
use serde::{Deserialize, de::IgnoredAny};
use serde_json::json;
use std::{net::IpAddr, time::Instant};
use thiserror::Error;
use tokio_rustls::rustls::pki_types::ServerName;
mod transport;

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum Failure {
    #[error("未配置")]
    Missing,
    #[error("配置不完整或无效，用 loom config list 核对")]
    Config,
    #[error("地址不合规：云端须用 https，http 只用于本机回环地址")]
    Address,
    #[error("连接失败")]
    Connect,
    #[error("超时")]
    Timeout,
    #[error("鉴权失败")]
    Auth,
    #[error("模型不存在")]
    NotFound,
    #[error("服务端错误")]
    Service,
    #[error("返回格式不符")]
    Format,
}

pub type Configured = Result<Option<Service>, Failure>;
pub struct Models {
    pub embedding: Configured,
    pub rerank: Configured,
}
impl Models {
    pub fn from_config(config: anyhow::Result<UserConfig>) -> Self {
        let parse = |e: anyhow::Result<Option<Endpoint>>| {
            e.map_err(|_| Failure::Config)?
                .map(Service::new)
                .transpose()
        };
        match config {
            Ok(c) => Self {
                embedding: parse(c.embedding()),
                rerank: parse(c.rerank()),
            },
            Err(_) => Self {
                embedding: Err(Failure::Config),
                rerank: Err(Failure::Config),
            },
        }
    }
}

pub struct Service {
    endpoint: Endpoint,
    host: String,
    port: u16,
    local: bool,
    tls: bool,
    origin: String,
    path: String,
    pub(super) profile: String,
}

impl Service {
    pub fn new(endpoint: Endpoint) -> Result<Self, Failure> {
        let url = &endpoint.base_url;
        if url.contains(['?', '#', char::from(92)]) {
            return Err(Failure::Address);
        }
        if endpoint
            .api_key_env()
            .is_some_and(|name| name.contains('=') || name.contains(char::is_control))
        {
            return Err(Failure::Auth);
        }
        let uri: Uri = url.parse().map_err(|_| Failure::Address)?;
        let tls = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => return Err(Failure::Address),
        };
        let authority = uri.authority().ok_or(Failure::Address)?;
        if authority.as_str().contains('@') {
            return Err(Failure::Address);
        }
        let raw = authority.host();
        let port = match &authority.as_str()[raw.len()..] {
            "" => {
                if tls {
                    443
                } else {
                    80
                }
            }
            suffix => suffix
                .strip_prefix(':')
                .and_then(|s| s.parse::<u16>().ok())
                .filter(|p| *p != 0)
                .ok_or(Failure::Address)?,
        };
        let host = raw
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(raw);
        let (host, display, local) = match host.parse::<IpAddr>() {
            Ok(ip) => (
                ip.to_string(),
                if ip.is_ipv6() {
                    format!("[{ip}]")
                } else {
                    ip.to_string()
                },
                ip.to_canonical().is_loopback(),
            ),
            Err(_) => {
                ServerName::try_from(host).map_err(|_| Failure::Address)?;
                let host = host.to_ascii_lowercase();
                // 只认名字本身，不看解析结果：连接时也只连回环地址。
                let local = host.trim_end_matches('.') == "localhost";
                let host = if local { "localhost".into() } else { host };
                (host.clone(), host, local)
            }
        };
        if !tls && !local {
            return Err(Failure::Address);
        }
        let origin = format!("{}://{display}:{port}", if tls { "https" } else { "http" });
        let path = uri.path().trim_end_matches('/').to_owned();
        let profile = sha256_hex(
            format!(
                "{origin}{path}\n{}\n{:?}",
                endpoint.model, endpoint.dimensions
            )
            .as_bytes(),
        );
        Ok(Self {
            endpoint,
            host,
            port,
            local,
            tls,
            origin,
            path,
            profile,
        })
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub fn dimensions(&self) -> Option<usize> {
        self.endpoint.dimensions.map(|n| n as usize)
    }
    pub fn batch(&self) -> usize {
        self.endpoint.batch
    }
    pub fn description(&self) -> String {
        let kind = if self.local { "本机" } else { "云端" };
        format!("{} @ {}（{kind}）", self.endpoint.model, self.origin)
    }
    pub fn key_source(&self) -> String {
        let key = self.endpoint.api_key();
        if let Some(name) = self.endpoint.api_key_env() {
            let display = if name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                name
            } else {
                "（名称无效）"
            };
            if std::env::var(name).is_ok_and(|s| !s.trim().is_empty()) {
                return format!("环境变量 {display} 有值");
            }
            return format!(
                "环境变量 {display} 无值；{}",
                if key.is_some() { "凭据文件" } else { "无" }
            );
        }
        if key.is_some() { "凭据文件" } else { "无" }.into()
    }

    pub(super) fn embed(&self, input: &[&str]) -> Result<Vec<Vec<f32>>, Failure> {
        #[derive(Deserialize)]
        struct Row {
            index: usize,
            embedding: Vec<f32>,
        }
        #[derive(Deserialize)]
        struct Reply {
            data: Vec<Row>,
            error: Option<IgnoredAny>,
        }
        let mut body = json!({"model":self.endpoint.model,"input":input,"encoding_format":"float"});
        if let Some(d) = self.dimensions() {
            body["dimensions"] = json!(d);
        }
        let reply: Reply = self.json("embeddings", Some(body), 8 << 20)?;
        if reply.error.is_some() || reply.data.len() != input.len() {
            return Err(Failure::Format);
        }
        let dim = self
            .dimensions()
            .or_else(|| reply.data.first().map(|r| r.embedding.len()))
            .ok_or(Failure::Format)?;
        if !(1..=super::MAX_DIM).contains(&dim) {
            return Err(Failure::Format);
        }
        let mut ordered = vec![Vec::new(); input.len()];
        for row in reply.data {
            let slot = ordered.get_mut(row.index).ok_or(Failure::Format)?;
            let v = row.embedding;
            let norm = v.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>().sqrt();
            if !slot.is_empty() || v.len() != dim || !norm.is_finite() || norm == 0.0 {
                return Err(Failure::Format);
            }
            *slot = v
                .into_iter()
                .map(|x| (f64::from(x) / norm) as f32)
                .collect();
        }
        Ok(ordered)
    }

    pub(super) fn rank(&self, query: &str, documents: &[&str]) -> Result<Vec<f64>, Failure> {
        #[derive(Deserialize)]
        struct Row {
            index: usize,
            relevance_score: Option<f64>,
            score: Option<f64>,
        }
        #[derive(Deserialize)]
        struct Reply {
            results: Vec<Row>,
            error: Option<IgnoredAny>,
        }
        let reply: Reply = self.json("rerank", Some(json!({"model":self.endpoint.model,"query":query,"documents":documents,"top_n":documents.len(),"return_documents":false})), 256 << 10)?;
        if reply.error.is_some() || reply.results.len() != documents.len() {
            return Err(Failure::Format);
        }
        let mut scores = vec![f64::NAN; documents.len()];
        for r in reply.results {
            let slot = scores.get_mut(r.index).ok_or(Failure::Format)?;
            if slot.is_finite() {
                return Err(Failure::Format);
            }
            *slot = r
                .relevance_score
                .or(r.score)
                .filter(|s| s.is_finite())
                .ok_or(Failure::Format)?;
        }
        Ok(scores)
    }

    pub fn probe(&self, embedding: bool) -> Result<String, Failure> {
        let start = Instant::now();
        let dimension = if embedding {
            format!("维度 {}，", self.embed(&["loom 连通性测试"])?[0].len())
        } else {
            self.rank("连通性测试", &["测试文档甲", "测试文档乙"])?;
            String::new()
        };
        Ok(format!(
            "{dimension}用时 {} ms",
            start.elapsed().as_millis()
        ))
    }

    pub fn pull(
        &self,
        embedding: bool,
        progress: impl FnMut(u64) + Send,
    ) -> Result<&'static str, Failure> {
        const OTHER: &str = "不适用，请在模型服务自己的界面里准备模型";
        if !self.local {
            return Ok(OTHER);
        }
        #[derive(Deserialize)]
        struct Tag {
            name: String,
        }
        #[derive(Deserialize)]
        struct Tags {
            models: Vec<Tag>,
        }
        let tags = match self.json::<Tags>("/api/tags", None, 1 << 20) {
            Ok(tags) => tags,
            Err(Failure::Format | Failure::NotFound) => return Ok(OTHER),
            Err(e) => return Err(e),
        };
        if !embedding {
            return Ok(
                "Ollama 没有重排接口；重排请用云端服务，或本机的 llama-server（--reranking）",
            );
        }
        let name = &self.endpoint.model;
        if tags
            .models
            .iter()
            .any(|t| t.name == *name || t.name.strip_suffix(":latest") == Some(name))
        {
            return Ok("已存在");
        }
        self.download(progress)?;
        Ok("下载成功")
    }
}
