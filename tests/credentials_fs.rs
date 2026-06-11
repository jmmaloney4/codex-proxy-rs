use std::os::unix::fs::PermissionsExt;

use codex_proxy_rs::credentials::{CredentialsError, FsAuthFile, OAuthCredentials};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

fn creds(access: &str, refresh: &str, expires: i64, user: &str) -> OAuthCredentials {
    OAuthCredentials {
        access_token: access.to_string(),
        refresh_token: refresh.to_string(),
        expires_at_ms: expires,
        user_id: user.to_string(),
    }
}

#[tokio::test]
async fn init_read_round_trip_with_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("auth.json");
    let file = FsAuthFile::new(&path);

    file.init(&creds("tok-a", "ref-a", 1_700_000_000_000, "acct-1"))
        .await
        .expect("init succeeds");

    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600, "file mode");
    assert_eq!(mode(path.parent().unwrap()), 0o700, "parent dir mode");

    let read = file.read().await.expect("read succeeds");
    assert_eq!(read.access_token, "tok-a");
    assert_eq!(read.refresh_token, "ref-a");
    assert_eq!(read.expires_at_ms, 1_700_000_000_000);
    assert_eq!(read.user_id, "acct-1");

    // On-disk shape is Go's fsAuth exactly.
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(raw["tokens"]["access_token"], "tok-a");
    assert_eq!(raw["tokens"]["refresh_token"], "ref-a");
    assert_eq!(raw["tokens"]["account_id"], "acct-1");
    assert_eq!(raw["tokens"]["expiresAt"], 1_700_000_000_000_i64);
}

#[tokio::test]
async fn deep_created_ancestors_are_all_0700() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a").join("b").join("c").join("auth.json");
    FsAuthFile::new(&path)
        .init(&creds("t", "r", 1, "acct"))
        .await
        .expect("init succeeds");

    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let mut cursor = path.parent().unwrap();
    while cursor != dir.path() {
        assert_eq!(mode(cursor), 0o700, "dir {} not 0700", cursor.display());
        cursor = cursor.parent().unwrap();
    }
}

#[tokio::test]
async fn id_token_fallback_when_access_token_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    std::fs::write(
        &path,
        json!({"tokens": {"id_token": "idtok", "account_id": "acct-2"}}).to_string(),
    )
    .unwrap();

    let read = FsAuthFile::new(&path).read().await.expect("read succeeds");
    assert_eq!(read.access_token, "idtok");
    assert_eq!(read.user_id, "acct-2");
}

#[tokio::test]
async fn missing_token_or_account_id_is_an_error() {
    let dir = tempfile::tempdir().unwrap();

    let no_token = dir.path().join("no_token.json");
    std::fs::write(
        &no_token,
        json!({"tokens": {"account_id": "a"}}).to_string(),
    )
    .unwrap();
    assert!(matches!(
        FsAuthFile::new(&no_token).read().await,
        Err(CredentialsError::Unavailable(_))
    ));

    let no_account = dir.path().join("no_account.json");
    std::fs::write(
        &no_account,
        json!({"tokens": {"access_token": "t"}}).to_string(),
    )
    .unwrap();
    assert!(matches!(
        FsAuthFile::new(&no_account).read().await,
        Err(CredentialsError::Unavailable(_))
    ));
}

#[tokio::test]
async fn update_tokens_preserves_id_token_and_account_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    std::fs::write(
        &path,
        json!({"tokens": {
            "id_token": "keep-me",
            "access_token": "old",
            "refresh_token": "old-r",
            "account_id": "acct-3",
            "expiresAt": 1,
        }})
        .to_string(),
    )
    .unwrap();

    let file = FsAuthFile::new(&path);
    file.update_tokens("new", "new-r", 99)
        .await
        .expect("update");

    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(raw["tokens"]["id_token"], "keep-me");
    assert_eq!(raw["tokens"]["account_id"], "acct-3");
    assert_eq!(raw["tokens"]["access_token"], "new");
    assert_eq!(raw["tokens"]["refresh_token"], "new-r");
    assert_eq!(raw["tokens"]["expiresAt"], 99);
    // Atomic write leaves no tmp file behind.
    let residue = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .count();
    assert_eq!(residue, 0, "tmp residue left behind");
}

#[tokio::test]
async fn update_tokens_errors_when_file_absent() {
    let dir = tempfile::tempdir().unwrap();
    let file = FsAuthFile::new(dir.path().join("missing.json"));
    assert!(matches!(
        file.update_tokens("a", "b", 1).await,
        Err(CredentialsError::Storage(_))
    ));
}
