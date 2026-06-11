//! /admin/credentials endpoints — ports of Go `credentialsHandler` and
//! `credentialsStatusHandler` (`server.go:884-992`).

use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::error::ApiError;
use crate::credentials::{CredentialsError, CredentialsKind};

#[derive(Debug, Deserialize)]
pub struct UpdateCredentialsBody {
    #[serde(rename = "accessToken", default)]
    access_token: String,
    #[serde(rename = "refreshToken", default)]
    refresh_token: String,
    #[serde(rename = "expiresAt", default)]
    expires_at: i64,
    #[serde(rename = "userID", default)]
    user_id: Option<String>,
}

pub async fn update_credentials(
    State(state): State<AppState>,
    Json(body): Json<UpdateCredentialsBody>,
) -> Result<Json<Value>, ApiError> {
    if body.access_token.is_empty() || body.refresh_token.is_empty() || body.expires_at == 0 {
        return Err(ApiError::BadRequest(
            "Missing required fields: accessToken, refreshToken, expiresAt".to_string(),
        ));
    }

    state
        .creds
        .update_tokens(
            body.access_token,
            body.refresh_token,
            body.expires_at,
            body.user_id,
        )
        .await
        .map_err(|err| match err {
            CredentialsError::OAuthUnsupported => ApiError::BadRequest(
                "Credentials fetcher does not support OAuth token updates".to_string(),
            ),
            other => {
                tracing::error!(error = %other, "failed to update credentials");
                ApiError::Internal("Failed to update credentials")
            }
        })?;

    tracing::info!("credentials updated via admin endpoint");
    Ok(Json(json!({
        "status": "success",
        "message": "Credentials updated successfully",
    })))
}

pub async fn credentials_status(State(state): State<AppState>) -> Json<Value> {
    match state.creds.kind() {
        CredentialsKind::Basic => match state.creds.get_credentials().await {
            Ok(creds) => Json(json!({
                "type": "basic",
                "hasCredentials": !creds.token.is_empty(),
                "userID": creds.account_id,
            })),
            Err(err) => Json(json!({
                "type": "basic",
                "hasCredentials": false,
                "error": err.to_string(),
            })),
        },
        CredentialsKind::OAuth => match state.creds.full_credentials().await {
            Ok(full) => {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let minutes_until_expiry = (full.expires_at_ms - now_ms) / 60_000;
                Json(json!({
                    "type": "oauth",
                    "hasCredentials": true,
                    "userID": full.user_id,
                    "expiresAt": full.expires_at_ms,
                    "minutesUntilExpiry": minutes_until_expiry,
                    "isExpired": minutes_until_expiry <= 0,
                    "needsRefreshSoon": minutes_until_expiry <= 60,
                }))
            }
            Err(err) => Json(json!({
                "type": "oauth",
                "hasCredentials": false,
                "error": err.to_string(),
            })),
        },
    }
}
