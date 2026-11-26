//! 只在管理员提供有效模型名时构造环回端点。没有通用 URL 客户端、DNS、代理、重试或重定向。
use std::{
    ffi::OsStr,
    net::{Ipv4Addr, SocketAddrV4},
    num::NonZeroU16,
    thread,
    time::Instant,
};

use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    Request, StatusCode,
    body::{Body, Bytes},
    client::conn::http1,
};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub(super) enum Model {
    Off,
    Invalid,
    Local { name: String, port: NonZeroU16 },
}

impl Model {
    pub(super) fn from_env(switch: &str) -> Self {
        Self::parse(
            std::env::var_os(switch).as_deref(),
            std::env::var_os("LOOM_MODEL_PORT").as_deref(),
        )
    }

    pub(super) fn parse(name: Option<&OsStr>, port: Option<&OsStr>) -> Self {
        let Some(name) = name.filter(|s| !s.is_empty()) else {
            return Self::Off;
        };
        let Some(name) = name.to_str() else {
            return Self::Invalid;
        };
        // 本地名称[:标签]，不解释注册表、路径或 URL，不修剪后重新解释无效值。
        let part = |s: &str| {
            s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        };
        let (base, tag) = name
            .split_once(':')
            .map_or((name, None), |(n, t)| (n, Some(t)));
        if name.len() > 128 || !part(base) || !tag.is_none_or(part) {
            return Self::Invalid;
        }
        let port = port
            .unwrap_or(OsStr::new("11434"))
            .to_str()
            .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|s| s.parse::<NonZeroU16>().ok());
        match port {
            Some(port) => Self::Local {
                name: name.into(),
                port,
            },
            None => Self::Invalid,
        }
    }

    pub(super) fn endpoint(&self) -> Result<(&str, NonZeroU16), &'static str> {
        match self {
            Self::Off => Err("模型未启用"),
            Self::Invalid => Err("模型配置无效"),
            Self::Local { name, port } => Ok((name, *port)),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Api {
    Embed,
    Generate,
}

/// 单次往返的绝对截止时间包括连接、写入、响应头、整个响应体；分块到达不续期。
/// 私有短命运行时也能被已在 Tokio 中的同步 MCP 调用；退出时套接字与挂起任务一起释放。
/// 错误、响应体与请求内容都不进日志或错误链。
pub(super) fn post<T: DeserializeOwned>(
    api: Api,
    port: NonZeroU16,
    json: Value,
    end: Instant,
) -> Option<T> {
    let bytes = serde_json::to_vec(&json).ok()?;
    if bytes.len() > 16 * 1024 || Instant::now() >= end {
        return None;
    }
    let worker = thread::Builder::new()
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .ok()?;
            rt.block_on(async {
                if Instant::now() >= end {
                    return None;
                }
                tokio::time::timeout_at(end.into(), exchange(api, port, bytes))
                    .await
                    .ok()?
            })
        })
        .ok()?;
    let response = worker.join().ok()??;
    if Instant::now() >= end {
        return None;
    }
    serde_json::from_slice(&response)
        .ok()
        .filter(|_| Instant::now() < end)
}

async fn exchange(api: Api, port: NonZeroU16, bytes: Vec<u8>) -> Option<Bytes> {
    let (path, limit) = match api {
        Api::Embed => ("/api/embed", 1024 * 1024),
        Api::Generate => ("/api/generate", 32 * 1024),
    };
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port.get());
    let tcp = tokio::net::TcpStream::connect(addr).await.ok()?;
    let (mut sender, conn) = http1::Builder::new()
        .max_buf_size(16 * 1024)
        .max_headers(32)
        .handshake(TokioIo::new(tcp))
        .await
        .ok()?;
    tokio::spawn(conn);
    let req = Request::post(path)
        .header("Host", addr.to_string())
        .header("Content-Type", "application/json")
        .header("Connection", "close")
        .body(Full::new(Bytes::from(bytes)))
        .ok()?;
    let res = sender.send_request(req).await.ok()?;
    let headers: usize = res
        .headers()
        .iter()
        .map(|(k, v)| k.as_str().len() + v.len() + 4)
        .sum();
    if res.status() != StatusCode::OK
        || headers > 16 * 1024
        || res.body().size_hint().lower() > limit as u64
    {
        return None;
    }
    Some(
        Limited::new(res.into_body(), limit)
            .collect()
            .await
            .ok()?
            .to_bytes(),
    )
}
