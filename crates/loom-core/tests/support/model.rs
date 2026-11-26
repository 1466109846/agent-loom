//! 跨 CLI / 核心测试复用的多请求回环替身。请求体和请求头不参与断言的调试输出。
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

pub struct Request {
    pub route: String,
    pub auth: Option<String>,
    pub body: Value,
}
pub struct Server {
    pub port: u16,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    pub fn new(mut handle: impl FnMut(&mut TcpStream, Request) + Send + 'static) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !flag.load(Ordering::Acquire) {
                let (mut tcp, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => panic!("回环监听失败"),
                };
                tcp.set_nonblocking(false).unwrap();
                tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut reader = BufReader::new(&tcp);
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let route = first
                    .split_whitespace()
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" ");
                let mut len = 0;
                let mut auth = None;
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        if k.eq_ignore_ascii_case("content-length") {
                            len = v.trim().parse().unwrap();
                        }
                        if k.eq_ignore_ascii_case("authorization") {
                            auth = Some(v.trim().to_owned());
                        }
                    }
                }
                assert!(len <= 1 << 20);
                let mut bytes = vec![0; len];
                reader.read_exact(&mut bytes).unwrap();
                let body = if len == 0 {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).expect("替身需要 JSON")
                };
                handle(&mut tcp, Request { route, auth, body });
            }
        });
        Self {
            port,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if self.worker.take().unwrap().join().is_err() && !thread::panicking() {
            panic!("替身请求不符合预期");
        }
    }
}
pub fn reply(s: &mut TcpStream, status: u16, body: &str) {
    let _ = write!(
        s,
        "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
}
pub fn embeddings(input: &Value, vector: impl Fn(&str) -> Vec<f32>) -> Value {
    let data: Vec<_> = input["input"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .rev()
        .map(|(i, v)| json!({"index":i,"embedding":vector(v.as_str().unwrap())}))
        .collect();
    json!({"data":data})
}
