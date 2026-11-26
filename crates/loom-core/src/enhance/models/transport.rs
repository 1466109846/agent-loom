use super::{Failure, Service};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    Request,
    body::{Body, Bytes, Incoming},
    client::conn::http1,
    header::{AUTHORIZATION, HeaderValue},
};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    future::Future,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Instant,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    time::timeout_at,
};
use tokio_rustls::{
    TlsConnector,
    rustls::{ClientConfig, RootCertStore, pki_types::ServerName},
};

// 独立运行时兼容同步 CLI 和已有 Tokio 的会话；DNS 系统调用不能拖住超时返回。
fn run<T: Send>(f: impl Future<Output = Result<T, Failure>> + Send) -> Result<T, Failure> {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .spawn_scoped(scope, || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                    .map_err(|_| Failure::Connect)?;
                let result = rt.block_on(f);
                rt.shutdown_background();
                result
            })
            .map_err(|_| Failure::Connect)?
            .join()
            .map_err(|_| Failure::Connect)?
    })
}

async fn send(
    io: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    req: Request<Full<Bytes>>,
) -> Result<Incoming, Failure> {
    let (mut tx, conn) = http1::Builder::new()
        .max_buf_size(16 << 10)
        .max_headers(64)
        .handshake(TokioIo::new(io))
        .await
        .map_err(|_| Failure::Connect)?;
    tokio::spawn(conn);
    let response = tx.send_request(req).await.map_err(|e| {
        if e.is_parse() {
            Failure::Format
        } else {
            Failure::Connect
        }
    })?;
    match response.status().as_u16() {
        200..=299 => Ok(response.into_body()),
        401 | 403 => Err(Failure::Auth),
        404 => Err(Failure::NotFound),
        _ => Err(Failure::Service),
    }
}

impl Service {
    async fn request(&self, route: &str, body: Option<Value>) -> Result<Incoming, Failure> {
        let path = if route.starts_with('/') {
            route.into()
        } else {
            format!("{}/{route}", self.path)
        };
        let mut req = if body.is_some() {
            Request::post(path)
        } else {
            Request::get(path)
        };
        if let Some(key) = self.endpoint.api_key() {
            let mut value =
                HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| Failure::Auth)?;
            value.set_sensitive(true);
            req = req.header(AUTHORIZATION, value);
        }
        let bytes = body
            .map(|v| serde_json::to_vec(&v))
            .transpose()
            .map_err(|_| Failure::Format)?
            .unwrap_or_default();
        let req = req
            .header("Host", self.origin.split_once("://").unwrap().1)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("Connection", "close")
            .body(Full::new(Bytes::from(bytes)))
            .map_err(|_| Failure::Address)?;
        let end = (Instant::now() + self.endpoint.timeout).into();
        let tcp = timeout_at(end, async {
            if self.host == "localhost" {
                let addrs = [
                    SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)),
                    SocketAddr::from((Ipv6Addr::LOCALHOST, self.port)),
                ];
                TcpStream::connect(&addrs[..]).await
            } else {
                TcpStream::connect((self.host.as_str(), self.port)).await
            }
        })
        .await
        .map_err(|_| Failure::Timeout)?
        .map_err(|_| Failure::Connect)?;
        if !self.tls {
            return send(tcp, req).await;
        }
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = ServerName::try_from(self.host.clone()).map_err(|_| Failure::Address)?;
        let tls = timeout_at(end, TlsConnector::from(Arc::new(config)).connect(name, tcp))
            .await
            .map_err(|_| Failure::Timeout)?
            .map_err(|_| Failure::Connect)?;
        send(tls, req).await
    }

    pub(super) fn json<T: DeserializeOwned + Send>(
        &self,
        route: &str,
        body: Option<Value>,
        limit: usize,
    ) -> Result<T, Failure> {
        let end = Instant::now() + self.endpoint.timeout;
        run(async {
            timeout_at(end.into(), async {
                let response = self.request(route, body).await?;
                if response.size_hint().lower() > limit as u64 {
                    return Err(Failure::Format);
                }
                let bytes = Limited::new(response, limit)
                    .collect()
                    .await
                    .map_err(|_| Failure::Format)?
                    .to_bytes();
                let data = serde_json::from_slice(&bytes).map_err(|_| Failure::Format)?;
                if Instant::now() >= end {
                    Err(Failure::Timeout)
                } else {
                    Ok(data)
                }
            })
            .await
            .map_err(|_| Failure::Timeout)?
        })
    }

    pub(super) fn download(&self, mut progress: impl FnMut(u64) + Send) -> Result<(), Failure> {
        #[derive(Deserialize)]
        struct Event {
            status: String,
            completed: Option<u64>,
            total: Option<u64>,
            error: Option<String>,
        }
        let mut event = |line: &[u8]| {
            let e: Event = serde_json::from_slice(line).map_err(|_| Failure::Format)?;
            if e.error.is_some() {
                return Err(Failure::Service);
            }
            if let (Some(done), Some(total)) = (e.completed, e.total.filter(|&n| n > 0)) {
                progress((u128::from(done.min(total)) * 100 / u128::from(total)) as u64);
            }
            Ok(e.status == "success")
        };
        run(async {
            let mut body = self
                .request(
                    "/api/pull",
                    Some(json!({"model":self.endpoint.model,"stream":true})),
                )
                .await?;
            let mut line = Vec::new();
            while let Some(frame) = body.frame().await {
                if let Ok(data) = frame.map_err(|_| Failure::Connect)?.into_data() {
                    for chunk in data.split_inclusive(|&b| b == b'\n') {
                        if line.len() + chunk.len() > 16 << 10 {
                            return Err(Failure::Format);
                        }
                        line.extend_from_slice(chunk);
                        if line.ends_with(b"\n") {
                            if event(&line)? {
                                return Ok(());
                            }
                            line.clear();
                        }
                    }
                }
            }
            if !line.is_empty() && event(&line)? {
                Ok(())
            } else {
                Err(Failure::Format)
            }
        })
    }
}
