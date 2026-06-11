//! Upstream HTTP client for the ChatGPT Codex backend. Port of Go
//! `internal/server/client.go` (client construction) and the
//! `makeChatGPTRequest` / `makeChatGPTRequestWithRetry` pair
//! (`server.go:439-545`).
//!
//! The WebSocket transport for `gpt-5.3-codex-spark` is intentionally not
//! ported (ADR 004 — spark support dropped); every model goes over HTTP.

use std::sync::Arc;
use std::time::Duration;

use crate::credentials::{CredentialsError, CredentialsFetcher};

/// The one backend endpoint both proxy routes forward to.
pub const UPSTREAM_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("failed to get credentials: {0}")]
    Credentials(#[source] CredentialsError),
    #[error("token expired and refresh failed: {0}")]
    RefreshFailed(#[source] CredentialsError),
    #[error("failed to send request: {0}")]
    Request(#[from] reqwest::Error),
}

/// Go `client.go` parity: 10s connect timeout, 30s TCP keepalive, HTTP/2 via
/// ALPN, 90s idle pool — and **no total or read timeout**, which SSE streams
/// require. Proxy-from-env is reqwest's default behavior.
pub fn build_upstream_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(100)
        .build()
        .expect("static client config is valid")
}

/// Strip a pre-existing case-insensitive `Bearer ` prefix so the header never
/// doubles up.
fn bare_token(token: &str) -> &str {
    let token = token.trim();
    if token.len() >= 7 && token[..7].eq_ignore_ascii_case("bearer ") {
        token[7..].trim()
    } else {
        token
    }
}

/// One upstream POST with the exact Go header set (`server.go:452-463`).
async fn send_codex_request(
    client: &reqwest::Client,
    url: &str,
    body: bytes::Bytes,
    token: &str,
    account_id: &str,
) -> Result<reqwest::Response, reqwest::Error> {
    let bare = bare_token(token);
    let turn_metadata = format!(
        r#"{{"turn_id":"{}","sandbox":"none"}}"#,
        uuid::Uuid::new_v4()
    );
    client
        .post(url)
        .header("authorization", format!("Bearer {bare}"))
        .header("version", "0.125.0")
        .header("openai-beta", "responses=experimental")
        .header("session_id", uuid::Uuid::new_v4().to_string())
        .header("accept", "text/event-stream")
        .header("content-type", "application/json")
        .header("chatgpt-account-id", account_id)
        .header("originator", "codex_cli_rs")
        .header(
            "user-agent",
            "codex_cli_rs/0.125.0 (Mac OS 26.3.0; arm64) Apple_Terminal/466",
        )
        .header(
            "x-codex-beta-features",
            "multi_agent,apps,prevent_idle_sleep",
        )
        .header("x-codex-turn-metadata", turn_metadata)
        .body(body)
        .send()
        .await
}

/// Port of Go `makeChatGPTRequestWithRetry`: fetch credentials, send, and on
/// 401 refresh + re-fetch + retry exactly once. A second 401 is returned
/// as-is for the caller to mirror downstream.
pub async fn send_with_retry(
    client: &reqwest::Client,
    creds: &Arc<dyn CredentialsFetcher>,
    url: &str,
    body: bytes::Bytes,
) -> Result<reqwest::Response, UpstreamError> {
    let initial = creds
        .get_credentials()
        .await
        .map_err(UpstreamError::Credentials)?;

    let resp = send_codex_request(
        client,
        url,
        body.clone(),
        &initial.token,
        &initial.account_id,
    )
    .await?;
    if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(resp);
    }

    tracing::warn!("received 401 Unauthorized, attempting token refresh");
    drop(resp);

    creds
        .refresh_credentials()
        .await
        .map_err(UpstreamError::RefreshFailed)?;

    let refreshed = creds
        .get_credentials()
        .await
        .map_err(UpstreamError::Credentials)?;
    let resp =
        send_codex_request(client, url, body, &refreshed.token, &refreshed.account_id).await?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        tracing::error!("still received 401 after token refresh, giving up");
    } else {
        tracing::info!("request succeeded after token refresh");
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::bare_token;

    #[test]
    fn bare_token_strips_bearer_prefix() {
        assert_eq!(bare_token("Bearer abc"), "abc");
        assert_eq!(bare_token("bearer abc"), "abc");
        assert_eq!(bare_token("  BEARER   abc  "), "abc");
        assert_eq!(bare_token("abc"), "abc");
        assert_eq!(bare_token("Bearerabc"), "Bearerabc");
    }
}
