//! Static-token credentials, port of Go `credentials/env.go`.
//!
//! Go reads `ANTHROPIC_API_KEY` / `CLAUDE_USER_ID` on every call; here the
//! values are snapshotted at startup (clap captures them into `Config`) — a
//! documented divergence that is irrelevant in a k8s pod, where env vars are
//! fixed for the pod's lifetime.

use super::{Credentials, CredentialsError, CredentialsFetcher, CredentialsKind};

#[derive(Debug, Clone)]
pub struct EnvCredentials {
    token: String,
    account_id: String,
}

impl EnvCredentials {
    pub fn new(token: String, account_id: String) -> Self {
        Self { token, account_id }
    }
}

#[async_trait::async_trait]
impl CredentialsFetcher for EnvCredentials {
    fn kind(&self) -> CredentialsKind {
        CredentialsKind::Basic
    }

    async fn get_credentials(&self) -> Result<Credentials, CredentialsError> {
        // Go returns whatever the env vars hold, empty strings included; the
        // upstream then rejects the request. Mirror that rather than erroring
        // here.
        Ok(Credentials {
            token: self.token.clone(),
            account_id: self.account_id.clone(),
        })
    }

    async fn refresh_credentials(&self) -> Result<(), CredentialsError> {
        // Go env fetcher: no-op.
        Ok(())
    }
}
