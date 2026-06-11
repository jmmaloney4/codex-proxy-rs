//! Bridges between the upstream `reqwest::Response`, the relay, and the
//! downstream axum response body. Port of Go `writeResponse`
//! (`server.go:547-657`), built on the ADR 003 pipe pattern.

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use futures_util::TryStreamExt;
use tokio::io::{AsyncBufRead, BufReader};
use tokio_util::io::{ReaderStream, StreamReader};

use crate::relay::{RelayConfig, pass_through_sse_stream, rewrite_sse_stream};

/// Which relay consumes the upstream stream.
pub enum RelayMode {
    /// Codex SSE → OpenAI chunks (`/v1/chat/completions` streaming).
    Rewrite { model: String },
    /// Verbatim events (`/v1/responses`).
    PassThrough,
}

/// Adapt a reqwest body into the relay's `AsyncBufRead` input.
pub fn response_reader(resp: reqwest::Response) -> impl AsyncBufRead + Unpin {
    BufReader::new(StreamReader::new(
        resp.bytes_stream().map_err(std::io::Error::other),
    ))
}

/// Copy upstream headers, dropping hop-by-hop and length headers that hyper
/// must own for a streamed body (Go's http.ResponseWriter strips these
/// implicitly; axum does not), plus `set-cookie`: backend session cookies
/// must not cross the proxy boundary to clients (Go forwards them verbatim —
/// hardening divergence, ADR 004).
fn sanitized_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in upstream {
        if matches!(
            name,
            &header::CONTENT_LENGTH
                | &header::TRANSFER_ENCODING
                | &header::CONNECTION
                | &header::SET_COOKIE
        ) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    headers
}

/// Success path of Go `writeResponse`: forced SSE headers, then the relay
/// streams into the response body via an in-memory pipe. The relay task is
/// self-terminating: a client disconnect drops the body, the relay's next
/// write fails with `BrokenPipe`, and dropping the upstream reader cancels
/// the reqwest request.
pub fn relay_response(resp: reqwest::Response, mode: RelayMode, relay: RelayConfig) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
    let mut headers = sanitized_headers(resp.headers());
    headers.insert(
        header::CONTENT_TYPE,
        "text/event-stream; charset=utf-8".parse().expect("static"),
    );
    headers.insert(header::CACHE_CONTROL, "no-cache".parse().expect("static"));

    let upstream = response_reader(resp);
    // duplex, not simplex: dropping a simplex WriteHalf does NOT surface EOF
    // to its ReadHalf (the split halves never shut the stream down), so the
    // response body would hang forever after the relay finished. Dropping one
    // duplex end closes the other. The unused reverse direction is the cost.
    let (tx, rx) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let result = match mode {
            RelayMode::Rewrite { model } => rewrite_sse_stream(upstream, tx, &model, &relay).await,
            RelayMode::PassThrough => pass_through_sse_stream(upstream, tx, &relay).await,
        };
        if let Err(err) = result {
            // Port of Go server.go:646-654 — but the stream itself was
            // already terminated correctly by the relay (ADR 002).
            tracing::error!(error = %err, "SSE relay terminated with error");
        }
    });

    let mut response = Response::new(Body::from_stream(ReaderStream::new(rx)));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// Error path of Go `writeResponse` (status != 200): buffer the body, log it,
/// and mirror status + headers + body downstream.
pub async fn mirror_error_response(resp: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = sanitized_headers(resp.headers());
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.bytes().await.unwrap_or_default();

    let preview: String = String::from_utf8_lossy(&body).chars().take(1200).collect();
    tracing::warn!(
        status_code = status.as_u16(),
        content_type = %content_type,
        response_body = %preview,
        "received error response from upstream API",
    );

    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

#[cfg(test)]
mod bridge_tests {
    use super::*;
    use http_body_util::BodyExt;

    /// Regression test for the pipe choice: a simplex-based bridge hangs
    /// forever here because the body never sees EOF after the relay task
    /// drops its write half.
    #[tokio::test]
    async fn relay_response_bridge_terminates() {
        let upstream = axum::http::Response::builder()
            .status(200)
            .body(reqwest::Body::from("data: [DONE]\n\n"))
            .unwrap();
        let resp = reqwest::Response::from(upstream);
        let response = relay_response(
            resp,
            RelayMode::Rewrite {
                model: "gpt-5".to_string(),
            },
            RelayConfig::default(),
        );
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            response.into_body().collect(),
        )
        .await
        .expect("bridge did not terminate")
        .unwrap()
        .to_bytes();
        assert_eq!(&body[..], b"data: [DONE]\n\n");
    }
}
