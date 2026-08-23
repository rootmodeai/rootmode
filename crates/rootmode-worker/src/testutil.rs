//! Test scaffolding, compiled for this crate's tests and for anyone who
//! enables the `testutil` feature (the desktop client does, to run its
//! end-to-end test against a real worker).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A canned HTTP response.
#[derive(Clone)]
pub struct StubResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

/// A one-shot HTTP server that replays canned responses in order.
///
/// Deliberately dumb: it reads the request, ignores it, and writes the next
/// response. That is enough to pin down how a backend adapter behaves against
/// a server that is slow, broken, or returning nonsense, without a mock
/// framework in the dependency tree.
pub struct StubHttp {
    addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
}

impl StubHttp {
    pub fn json(status: u16, body: &str) -> StubResponse {
        StubResponse {
            status,
            content_type: "application/json",
            body: body.to_string(),
        }
    }

    pub fn sse(body: &str) -> StubResponse {
        StubResponse {
            status: 200,
            content_type: "text/event-stream",
            body: body.to_string(),
        }
    }

    pub fn bytes(status: u16, content_type: &'static str, body: Vec<u8>) -> StubResponse {
        StubResponse {
            status,
            content_type,
            // Latin-1 round-trips arbitrary bytes through String unharmed.
            body: body.iter().map(|b| *b as char).collect(),
        }
    }

    pub async fn start(responses: Vec<StubResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let queue = queue.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 64 * 1024];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    seen.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buf[..n]).to_string());

                    // The last response repeats, so a test does not have to
                    // count polls it does not care about.
                    let response = {
                        let mut q = queue.lock().unwrap();
                        if q.len() > 1 {
                            q.pop_front()
                        } else {
                            q.front().cloned()
                        }
                    };
                    let Some(r) = response else { return };

                    let body = r.body.chars().map(|c| c as u8).collect::<Vec<u8>>();
                    let head = format!(
                        "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        r.status,
                        r.content_type,
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                    let _ = stream.flush().await;
                });
            }
        });

        Self { addr, requests }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Raw request text, for asserting what an adapter actually sent.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}
