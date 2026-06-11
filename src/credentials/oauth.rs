//! OAuth token refresh against auth.openai.com. Port of Go `internal/auth`
//! (`oauth.go` constants + `RefreshToken`, `TokenExpired`).

use serde::{Deserialize, Serialize};

use super::CredentialsError;

pub const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Refresh when within this buffer of expiry (Go `TokenExpiryBuffer`, 60min).
pub const TOKEN_EXPIRY_BUFFER_MS: i64 = 60 * 60 * 1000;

/// Go `TokenExpired`: expired when `now >= expiresAt - buffer`.
pub fn token_expired(expires_at_ms: i64, now_ms: i64) -> bool {
    now_ms >= expires_at_ms - TOKEN_EXPIRY_BUFFER_MS
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Serialize)]
struct TokenRefreshRequest<'a> {
    grant_type: &'static str,
    refresh_token: &'a str,
    client_id: &'static str,
    scope: &'static str,
}

#[derive(Debug, Deserialize)]
pub struct TokenRefreshResponse {
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds until expiry.
    pub expires_in: i64,
}

/// POST the refresh grant. `token_url` is injectable for tests; production
/// passes [`OAUTH_TOKEN_URL`].
pub async fn refresh_token(
    http: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<TokenRefreshResponse, CredentialsError> {
    let request = TokenRefreshRequest {
        grant_type: "refresh_token",
        refresh_token,
        client_id: CLIENT_ID,
        scope: "openid profile email",
    };
    let resp = http
        .post(token_url)
        // The shared upstream client deliberately has no read timeout (SSE);
        // the token endpoint is a small JSON exchange, so bound it here.
        .timeout(std::time::Duration::from_secs(30))
        .json(&request)
        .send()
        .await
        .map_err(|e| CredentialsError::Refresh(format!("request failed: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(CredentialsError::Refresh(format!(
            "token endpoint returned {status}: {body}"
        )));
    }
    let parsed = resp
        .json::<TokenRefreshResponse>()
        .await
        .map_err(|e| CredentialsError::Refresh(format!("invalid token response: {e}")))?;
    if parsed.access_token.is_empty() {
        return Err(CredentialsError::Refresh(
            "token endpoint returned an empty access_token".to_string(),
        ));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const HOUR_MS: i64 = 60 * 60 * 1000;

    #[rstest]
    // Expires in 2h → not within the 1h buffer.
    #[case(2 * HOUR_MS, false)]
    // Expires in exactly 1h → boundary is inclusive (Go: now >= expires - buffer).
    #[case(HOUR_MS, true)]
    // Expires in 30min → within buffer.
    #[case(HOUR_MS / 2, true)]
    // Already expired.
    #[case(-HOUR_MS, true)]
    fn token_expired_boundary(#[case] delta_ms: i64, #[case] expired: bool) {
        let now = 1_700_000_000_000;
        assert_eq!(token_expired(now + delta_ms, now), expired);
    }
}
