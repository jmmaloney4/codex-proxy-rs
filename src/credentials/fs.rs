//! Filesystem credential store: Go's `auth.json` format on a writable volume
//! (a PVC in the k8s deployment). Port of Go `credentials/fs.go`.
//!
//! Unlike Go's plain `os.WriteFile`, writes go through a tmp-file + rename so
//! a crash mid-write can't truncate the only copy of the refresh token.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{CredentialsError, OAuthCredentials};

/// Go `fsAuth` exactly: `{"tokens": {id_token, access_token, refresh_token,
/// account_id, expiresAt}}`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct FsAuth {
    tokens: FsTokens,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FsTokens {
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    account_id: String,
    #[serde(rename = "expiresAt", default, skip_serializing_if = "is_zero")]
    expires_at: i64,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

#[derive(Debug, Clone)]
pub struct FsAuthFile {
    path: PathBuf,
}

impl FsAuthFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    async fn read_raw(&self) -> Result<FsAuth, CredentialsError> {
        let bytes = tokio::fs::read(&self.path)
            .await
            .map_err(CredentialsError::Storage)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Full OAuth state. Token = `access_token`, falling back to `id_token`;
    /// missing token or `account_id` is an error (Go parity).
    pub async fn read(&self) -> Result<OAuthCredentials, CredentialsError> {
        let auth = self.read_raw().await?;
        let token = if auth.tokens.access_token.is_empty() {
            auth.tokens.id_token.clone()
        } else {
            auth.tokens.access_token.clone()
        };
        if token.is_empty() || auth.tokens.account_id.is_empty() {
            return Err(CredentialsError::Unavailable(
                "missing token or account_id in credentials file".to_string(),
            ));
        }
        Ok(OAuthCredentials {
            access_token: token,
            refresh_token: auth.tokens.refresh_token,
            expires_at_ms: auth.tokens.expires_at,
            user_id: auth.tokens.account_id,
        })
    }

    /// Update tokens in place, preserving `id_token`/`account_id` from the
    /// existing file (Go `UpdateTokens`). Errors if the file doesn't exist —
    /// use [`FsAuthFile::init`] to bootstrap.
    pub async fn update_tokens(
        &self,
        access_token: &str,
        refresh_token: &str,
        expires_at_ms: i64,
    ) -> Result<(), CredentialsError> {
        let mut auth = self.read_raw().await?;
        auth.tokens.access_token = access_token.to_string();
        auth.tokens.refresh_token = refresh_token.to_string();
        auth.tokens.expires_at = expires_at_ms;
        self.write_raw(&auth).await
    }

    /// Create the file from scratch (Go `InitFromOAuth`): parent dirs 0700,
    /// file 0600.
    pub async fn init(&self, creds: &OAuthCredentials) -> Result<(), CredentialsError> {
        let mut auth = FsAuth::default();
        auth.tokens.access_token = creds.access_token.clone();
        auth.tokens.refresh_token = creds.refresh_token.clone();
        auth.tokens.expires_at = creds.expires_at_ms;
        auth.tokens.account_id = creds.user_id.clone();
        self.write_raw(&auth).await
    }

    async fn write_raw(&self, auth: &FsAuth) -> Result<(), CredentialsError> {
        use std::os::unix::fs::PermissionsExt;

        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(CredentialsError::Storage)?;
            tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .await
                .map_err(CredentialsError::Storage)?;
        }

        let data = serde_json::to_vec_pretty(auth)?;
        // Unique sibling temp file: a fixed name could race a writer outside
        // this process's mutex (out-of-design for the 1-replica deployment,
        // but cheap to rule out). Created 0600 atomically — a write-then-chmod
        // sequence would expose the tokens for a moment under a permissive
        // umask.
        let tmp = self
            .path
            .with_extension(format!("json.tmp.{}", uuid::Uuid::new_v4()));
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .await
            .map_err(CredentialsError::Storage)?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &data)
            .await
            .map_err(CredentialsError::Storage)?;
        file.sync_all().await.map_err(CredentialsError::Storage)?;
        drop(file);
        tokio::fs::rename(&tmp, &self.path)
            .await
            .map_err(CredentialsError::Storage)?;
        // fsync the parent directory: rename alone doesn't persist the
        // directory entry, and this file holds the only copy of the rotated
        // refresh token.
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            let dir = tokio::fs::File::open(parent)
                .await
                .map_err(CredentialsError::Storage)?;
            dir.sync_all().await.map_err(CredentialsError::Storage)?;
        }
        Ok(())
    }
}
