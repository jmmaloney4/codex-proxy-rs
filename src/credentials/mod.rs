//! Credential stores for authenticating against the ChatGPT backend.
//!
//! Port of Go `internal/credentials` reshaped for the Kubernetes deployment
//! this proxy targets (ADR 004): only the `env` (static token) and, in Phase
//! 5, `fs` (auth.json on a writable volume with OAuth self-refresh) stores
//! exist. The Go keychain store, legacy-path migration, and `auto` mode are
//! intentionally not ported.
//!
//! Go expresses the OAuth surface as an optional second interface reached by
//! type assertion (`OAuthCredentialsFetcher`). Rust folds it into the one
//! trait with `OAuthUnsupported`-by-default methods — same capability check,
//! no downcasting.

pub mod env;

pub use env::EnvCredentials;

/// What a store can do — drives the `/admin/credentials/status` response
/// shape (Go switches on the type assertion instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialsKind {
    /// Static token, no OAuth lifecycle.
    Basic,
    /// OAuth tokens with refresh + rotation.
    OAuth,
}

/// The pair every upstream request needs.
#[derive(Debug, Clone)]
pub struct Credentials {
    /// Bearer token for the `authorization` header.
    pub token: String,
    /// Value for the `chatgpt-account-id` header.
    pub account_id: String,
}

/// Full OAuth state, mirroring Go `credentials.OAuthCredentials`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuthCredentials {
    pub access_token: String,
    pub refresh_token: String,
    /// Milliseconds since the Unix epoch.
    pub expires_at_ms: i64,
    pub user_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialsError {
    #[error("credential store does not support OAuth operations")]
    OAuthUnsupported,
    #[error("credentials unavailable: {0}")]
    Unavailable(String),
    #[error("token refresh failed: {0}")]
    Refresh(String),
    #[error("credential storage error: {0}")]
    Storage(#[source] std::io::Error),
    #[error("credentials file is invalid: {0}")]
    Parse(#[from] serde_json::Error),
}

/// Port of Go `credentials.CredentialsFetcher` (+ the OAuth extension
/// interface, folded in with default implementations).
#[async_trait::async_trait]
pub trait CredentialsFetcher: Send + Sync {
    fn kind(&self) -> CredentialsKind;

    /// Token + account ID for one upstream request. OAuth stores refresh
    /// here when the token is within the expiry buffer.
    async fn get_credentials(&self) -> Result<Credentials, CredentialsError>;

    /// Force a refresh — the 401-retry path. No-op for static stores.
    async fn refresh_credentials(&self) -> Result<(), CredentialsError>;

    /// Full OAuth state for `/admin/credentials/status`.
    async fn full_credentials(&self) -> Result<OAuthCredentials, CredentialsError> {
        Err(CredentialsError::OAuthUnsupported)
    }

    /// Replace tokens — the `/admin/credentials` push path.
    async fn update_tokens(
        &self,
        _access_token: String,
        _refresh_token: String,
        _expires_at_ms: i64,
        _user_id: Option<String>,
    ) -> Result<(), CredentialsError> {
        Err(CredentialsError::OAuthUnsupported)
    }
}
