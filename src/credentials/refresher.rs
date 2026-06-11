//! OAuth-refreshing wrapper around the filesystem store. Port of Go
//! `internal/auth/fetcher.go` (`OAuthFetcher`).
//!
//! All credential operations run under one async mutex, exactly like Go's
//! exclusive lock: concurrent `get_credentials` calls during an expiry window
//! produce a single refresh (the waiters re-read the freshly persisted state
//! under the lock).

use std::sync::Arc;

use super::fs::FsAuthFile;
use super::oauth::{self, now_ms, token_expired};
use super::{Credentials, CredentialsError, CredentialsFetcher, CredentialsKind, OAuthCredentials};

pub struct OAuthFetcher {
    store: FsAuthFile,
    http: reqwest::Client,
    /// Injectable for tests; [`oauth::OAUTH_TOKEN_URL`] in production.
    token_url: String,
    lock: tokio::sync::Mutex<()>,
}

impl OAuthFetcher {
    pub fn new(store: FsAuthFile, http: reqwest::Client) -> Self {
        Self::with_token_url(store, http, oauth::OAUTH_TOKEN_URL)
    }

    pub fn with_token_url(
        store: FsAuthFile,
        http: reqwest::Client,
        token_url: impl Into<String>,
    ) -> Self {
        Self {
            store,
            http,
            token_url: token_url.into(),
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Go `CalculateExpiresAt`: now + expires_in seconds, in ms. A
    /// non-positive lifetime is rejected — persisting an already-expired
    /// token would turn every subsequent request into a refresh attempt.
    fn expires_at_from(expires_in_secs: i64) -> Result<i64, CredentialsError> {
        if expires_in_secs <= 0 {
            return Err(CredentialsError::Refresh(format!(
                "token endpoint returned a non-positive expiry: {expires_in_secs}"
            )));
        }
        Ok(now_ms() + expires_in_secs * 1000)
    }

    /// Refresh + persist. Caller must hold the lock.
    async fn refresh_and_persist(
        &self,
        current: &OAuthCredentials,
    ) -> Result<OAuthCredentials, CredentialsError> {
        let new_tokens =
            oauth::refresh_token(&self.http, &self.token_url, &current.refresh_token).await?;
        let expires_at_ms = Self::expires_at_from(new_tokens.expires_in)?;

        let refreshed = OAuthCredentials {
            access_token: new_tokens.access_token,
            refresh_token: new_tokens.refresh_token,
            expires_at_ms,
            user_id: current.user_id.clone(),
        };
        self.store
            .update_tokens(
                &refreshed.access_token,
                &refreshed.refresh_token,
                refreshed.expires_at_ms,
            )
            .await?;
        Ok(refreshed)
    }

    /// 10-minute proactive refresh loop (Go `backgroundRefresh`). Errors are
    /// logged and the loop continues; the per-request path is the backstop.
    pub fn spawn_background_refresh(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let fetcher = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10 * 60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick fires immediately; skip it so startup validation
            // owns the first look at the file.
            interval.tick().await;
            loop {
                interval.tick().await;
                fetcher.check_and_refresh().await;
            }
        })
    }

    /// One background tick (Go `checkAndRefreshToken`). Public for tests.
    pub async fn check_and_refresh(&self) {
        let _guard = self.lock.lock().await;
        let creds = match self.store.read().await {
            Ok(creds) => creds,
            Err(err) => {
                tracing::error!(error = %err, "background refresh: failed to get credentials");
                return;
            }
        };
        if !token_expired(creds.expires_at_ms, now_ms()) {
            return;
        }
        tracing::info!("background refresh: token expiring soon, refreshing");
        match self.refresh_and_persist(&creds).await {
            Ok(_) => tracing::info!("background refresh: token refreshed successfully"),
            Err(err) => tracing::error!(error = %err, "background refresh failed"),
        }
    }
}

#[async_trait::async_trait]
impl CredentialsFetcher for OAuthFetcher {
    fn kind(&self) -> CredentialsKind {
        CredentialsKind::OAuth
    }

    /// Go `GetCredentials`: refresh when within the expiry buffer. On refresh
    /// failure return the stale token (the 401-retry path is the backstop);
    /// on persist failure return the new token anyway.
    async fn get_credentials(&self) -> Result<Credentials, CredentialsError> {
        let _guard = self.lock.lock().await;
        let creds = self.store.read().await.map_err(|e| {
            CredentialsError::Unavailable(format!("failed to get full credentials: {e}"))
        })?;

        if !token_expired(creds.expires_at_ms, now_ms()) {
            return Ok(Credentials {
                token: creds.access_token,
                account_id: creds.user_id,
            });
        }

        tracing::info!(
            minutes_until_expiry = (creds.expires_at_ms - now_ms()) / 60_000,
            "OAuth token expired or expiring soon, refreshing",
        );
        let new_tokens = match oauth::refresh_token(
            &self.http,
            &self.token_url,
            &creds.refresh_token,
        )
        .await
        {
            Ok(tokens) => tokens,
            Err(err) => {
                tracing::error!(error = %err, "failed to refresh OAuth token, using stale token");
                return Ok(Credentials {
                    token: creds.access_token,
                    account_id: creds.user_id,
                });
            }
        };
        let expires_at_ms = match Self::expires_at_from(new_tokens.expires_in) {
            Ok(ms) => ms,
            Err(err) => {
                tracing::error!(error = %err, "rejecting refreshed token, using stale token");
                return Ok(Credentials {
                    token: creds.access_token,
                    account_id: creds.user_id,
                });
            }
        };
        if let Err(err) = self
            .store
            .update_tokens(
                &new_tokens.access_token,
                &new_tokens.refresh_token,
                expires_at_ms,
            )
            .await
        {
            tracing::error!(error = %err, "failed to persist refreshed tokens, using them anyway");
        } else {
            tracing::info!("OAuth token refreshed successfully");
        }
        Ok(Credentials {
            token: new_tokens.access_token,
            account_id: creds.user_id,
        })
    }

    /// Go `RefreshCredentials`: forced refresh, errors propagate (the
    /// 401-retry caller turns them into `RefreshFailed`).
    async fn refresh_credentials(&self) -> Result<(), CredentialsError> {
        let _guard = self.lock.lock().await;
        let creds = self.store.read().await?;
        self.refresh_and_persist(&creds).await?;
        Ok(())
    }

    async fn full_credentials(&self) -> Result<OAuthCredentials, CredentialsError> {
        self.store.read().await
    }

    /// Pass-through to the store, creating the file when absent — the
    /// `/admin/credentials` bootstrap path (Go `InitFromOAuth`).
    async fn update_tokens(
        &self,
        access_token: String,
        refresh_token: String,
        expires_at_ms: i64,
        user_id: Option<String>,
    ) -> Result<(), CredentialsError> {
        let _guard = self.lock.lock().await;
        if self.store.exists() {
            self.store
                .update_tokens(&access_token, &refresh_token, expires_at_ms)
                .await
        } else {
            // Bootstrapping without an account id would persist a file that
            // every subsequent read rejects — fail the push instead.
            let user_id = user_id.filter(|id| !id.is_empty()).ok_or_else(|| {
                CredentialsError::Unavailable(
                    "userID is required when bootstrapping the filesystem credential store"
                        .to_string(),
                )
            })?;
            self.store
                .init(&OAuthCredentials {
                    access_token,
                    refresh_token,
                    expires_at_ms,
                    user_id,
                })
                .await
        }
    }
}
