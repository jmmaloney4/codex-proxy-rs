#![allow(dead_code)]

//! Shared test infrastructure: an in-process mock upstream (real axum server
//! on 127.0.0.1:0 — reqwest needs a socket) and credential doubles.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::any;
use codex_proxy_rs::credentials::{
    Credentials, CredentialsError, CredentialsFetcher, CredentialsKind,
};
use codex_proxy_rs::relay::RelayConfig;
use codex_proxy_rs::server::AppState;
use tokio::sync::{Mutex, mpsc};
use tokio_stream::wrappers::ReceiverStream;

pub const TEST_ADMIN_KEY: &str = "test-admin-key";

/// One scripted upstream reply.
pub enum MockResponse {
    /// 200 `text/event-stream` with a canned body.
    Sse(String),
    /// Arbitrary status with a JSON body.
    Status(u16, String),
    /// 200 `text/event-stream` fed from a channel (for stall/keepalive and
    /// disconnect tests).
    Stream(mpsc::Receiver<Result<Bytes, std::io::Error>>),
}

pub struct MockUpstream {
    pub url: String,
    pub hits: Arc<AtomicUsize>,
    pub headers: Arc<Mutex<Vec<HeaderMap>>>,
}

impl MockUpstream {
    /// Start a mock upstream whose nth request (0-based) is answered by
    /// `script[n]`; requests beyond the script get 500.
    pub async fn start(script: Vec<MockResponse>) -> Self {
        let hits = Arc::new(AtomicUsize::new(0));
        let headers = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(script.into_iter().map(Some).collect::<Vec<_>>()));

        let hits_clone = hits.clone();
        let headers_clone = headers.clone();
        let app = Router::new().fallback(any(move |req: axum::extract::Request| {
            let hits = hits_clone.clone();
            let headers = headers_clone.clone();
            let script = script.clone();
            async move {
                let n = hits.fetch_add(1, Ordering::SeqCst);
                headers.lock().await.push(req.headers().clone());
                let mut script = script.lock().await;
                let entry = script.get_mut(n).and_then(Option::take);
                match entry {
                    Some(MockResponse::Sse(body)) => Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(body))
                        .unwrap(),
                    Some(MockResponse::Status(code, body)) => Response::builder()
                        .status(code)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                    Some(MockResponse::Stream(rx)) => Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(ReceiverStream::new(rx)))
                        .unwrap(),
                    None => Response::builder()
                        .status(500)
                        .body(Body::from("mock script exhausted"))
                        .unwrap(),
                }
            }
        }));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self {
            url: format!("http://{addr}"),
            hits,
            headers,
        }
    }

    pub fn hit_count(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

/// Static credentials with a refresh counter.
pub struct StaticCredentials {
    pub token: String,
    pub account_id: String,
    pub refreshes: Arc<AtomicUsize>,
}

impl StaticCredentials {
    pub fn new(token: &str, account_id: &str) -> Self {
        Self {
            token: token.to_string(),
            account_id: account_id.to_string(),
            refreshes: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl CredentialsFetcher for StaticCredentials {
    fn kind(&self) -> CredentialsKind {
        CredentialsKind::Basic
    }

    async fn get_credentials(&self) -> Result<Credentials, CredentialsError> {
        Ok(Credentials {
            token: self.token.clone(),
            account_id: self.account_id.clone(),
        })
    }

    async fn refresh_credentials(&self) -> Result<(), CredentialsError> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// AppState wired to a mock upstream with a short keepalive for tests.
pub fn test_state(upstream_url: &str, creds: Arc<dyn CredentialsFetcher>) -> AppState {
    AppState {
        creds,
        http: codex_proxy_rs::upstream::build_upstream_client(),
        relay: RelayConfig {
            keepalive_interval: std::time::Duration::from_millis(50),
        },
        upstream_url: upstream_url.into(),
        admin_api_key: Some(TEST_ADMIN_KEY.into()),
    }
}

/// Canned upstream Codex SSE stream: created → text deltas → completed → DONE.
pub fn codex_sse_fixture() -> String {
    [
        r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_test1"}}"#,
        r#"{"type":"response.output_text.delta","sequence_number":1,"delta":"Hello"}"#,
        r#"{"type":"response.output_text.delta","sequence_number":2,"delta":" world"}"#,
        r#"{"type":"response.completed","sequence_number":3,"response":{"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#,
        "[DONE]",
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect()
}
