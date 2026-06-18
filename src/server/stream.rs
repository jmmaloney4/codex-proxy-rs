//! Bridges between the upstream `reqwest::Response`, the relay, and the
//! downstream axum response body. Port of Go `writeResponse`
//! (`server.go:547-657`), built on the ADR 003 pipe pattern.

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use futures_util::TryStreamExt;
use tokio::io::{AsyncBufRead, BufReader};
use tokio_util::io::{ReaderStream, StreamReader};
use tracing::Instrument as _;

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
/// implicitly; axum does not), plus `set-cookie` (backend session cookies
/// must not cross the proxy boundary — hardening divergence, ADR 004) and
/// `content-encoding` (this client never advertises accept-encoding, and if
/// a transitive feature ever enables reqwest decompression, forwarding the
/// stale encoding would corrupt the already-decoded body).
fn sanitized_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in upstream {
        if matches!(
            name,
            &header::CONTENT_LENGTH
                | &header::TRANSFER_ENCODING
                | &header::CONNECTION
                | &header::SET_COOKIE
                | &header::CONTENT_ENCODING
        ) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    headers
}

/// Is the upstream response an SSE stream? Go's `writeResponse` makes the
/// same media-type check (`mime.ParseMediaType` → `text/event-stream`).
pub fn is_event_stream(resp: &reqwest::Response) -> bool {
    resp.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            ct.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("text/event-stream")
        })
        .unwrap_or(false)
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
    // The relay runs in a detached task that outlives this function — carry the
    // current span into it so the streaming duration (codex-proxy's unique trace
    // contribution) is captured instead of ending when the handler returns
    // (ADR 005 §7).
    tokio::spawn(
        async move {
            let result = match mode {
                RelayMode::Rewrite { model } => {
                    rewrite_sse_stream(upstream, tx, &model, &relay).await
                }
                RelayMode::PassThrough => pass_through_sse_stream(upstream, tx, &relay).await,
            };
            if let Err(err) = result {
                // Port of Go server.go:646-654 — but the stream itself was
                // already terminated correctly by the relay (ADR 002).
                tracing::error!(error = %err, "SSE relay terminated with error");
            }
        }
        .instrument(tracing::Span::current()),
    );

    let mut response = Response::new(Body::from_stream(ReaderStream::new(rx)));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// Router mode (ADR 007): stream a sibling pod's response back verbatim. The
/// pod already emits final OpenAI-format bytes (SSE *or* JSON), so there is no
/// relay/rewrite — just copy status + sanitized headers and pipe the body. The
/// body stream is lazy, so the pod's own keepalives/`[DONE]` flow through and a
/// client disconnect drops the stream (cancelling the upstream request).
pub fn proxy_response(resp: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = sanitized_headers(resp.headers());
    let mut response = Response::new(Body::from_stream(resp.bytes_stream()));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// Error path of Go `writeResponse` (status != 200): buffer the body, log it,
/// and mirror status + headers + body downstream.
pub async fn mirror_error_response(resp: reqwest::Response) -> Response {
    mirror_response(resp, true).await
}

/// Mirror a non-SSE success response verbatim — the `/v1/responses`
/// non-streaming path. (Go pushes these through `PassThroughSSEStream`, which
/// silently empties any non-SSE body; that bug is not ported — ADR 004.)
pub async fn mirror_success_response(resp: reqwest::Response) -> Response {
    mirror_response(resp, false).await
}

async fn mirror_response(resp: reqwest::Response, warn: bool) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = sanitized_headers(resp.headers());
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = match resp.bytes().await {
        Ok(body) => body,
        Err(err) => {
            // Upstream broke mid-body: fail closed with a gateway error
            // rather than mirroring a misleading empty body under the
            // original status. (Go mirrors the empty body — divergence.)
            tracing::error!(
                error = %err,
                upstream_status = status.as_u16(),
                "failed to read upstream response body",
            );
            let mut response = Response::new(Body::from(
                serde_json::json!({"error": "Failed to read upstream response body"}).to_string(),
            ));
            *response.status_mut() = StatusCode::BAD_GATEWAY;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                "application/json".parse().expect("static"),
            );
            return response;
        }
    };

    if warn {
        let preview: String = String::from_utf8_lossy(&body).chars().take(1200).collect();
        tracing::warn!(
            status_code = status.as_u16(),
            content_type = %content_type,
            response_body = %preview,
            "received error response from upstream API",
        );
    }

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
