use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Json;
use axum::routing::post;
use codex_proxy_rs::credentials::oauth::{TOKEN_EXPIRY_BUFFER_MS, now_ms};
use codex_proxy_rs::credentials::{
    CredentialsError, CredentialsFetcher, FsAuthFile, OAuthCredentials, OAuthFetcher,
};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

/// Mock https://auth.openai.com/oauth/token. Counts hits; `fail` makes every
/// response a 500.
struct MockTokenEndpoint {
    url: String,
    hits: Arc<AtomicUsize>,
    requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
}

async fn start_token_endpoint(fail: bool) -> MockTokenEndpoint {
    let hits = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let (hits_c, requests_c) = (hits.clone(), requests.clone());

    let app = axum::Router::new().route(
        "/oauth/token",
        post(move |Json(body): Json<Value>| {
            let hits = hits_c.clone();
            let requests = requests_c.clone();
            async move {
                let n = hits.fetch_add(1, Ordering::SeqCst);
                requests.lock().await.push(body);
                if fail {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "boom"})),
                    );
                }
                (
                    axum::http::StatusCode::OK,
                    Json(json!({
                        "access_token": format!("fresh-access-{n}"),
                        "refresh_token": format!("fresh-refresh-{n}"),
                        "expires_in": 3600 * 8,
                    })),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    MockTokenEndpoint {
        url: format!("http://{addr}/oauth/token"),
        hits,
        requests,
    }
}

fn seeded_store(dir: &tempfile::TempDir, expires_at_ms: i64) -> FsAuthFile {
    let path = dir.path().join("auth.json");
    std::fs::write(
        &path,
        json!({"tokens": {
            "access_token": "seed-access",
            "refresh_token": "seed-refresh",
            "account_id": "acct-1",
            "expiresAt": expires_at_ms,
        }})
        .to_string(),
    )
    .unwrap();
    FsAuthFile::new(path)
}

fn fetcher(store: FsAuthFile, token_url: &str) -> OAuthFetcher {
    OAuthFetcher::with_token_url(store, reqwest::Client::new(), token_url)
}

#[tokio::test]
async fn valid_token_is_returned_without_refresh() {
    let endpoint = start_token_endpoint(false).await;
    let dir = tempfile::tempdir().unwrap();
    let fetcher = fetcher(
        seeded_store(&dir, now_ms() + 8 * 3600 * 1000),
        &endpoint.url,
    );

    let creds = fetcher.get_credentials().await.expect("get succeeds");
    assert_eq!(creds.token, "seed-access");
    assert_eq!(creds.account_id, "acct-1");
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn expired_token_refreshes_rotates_and_persists() {
    let endpoint = start_token_endpoint(false).await;
    let dir = tempfile::tempdir().unwrap();
    // Inside the 60-min buffer → must refresh.
    let store = seeded_store(&dir, now_ms() + TOKEN_EXPIRY_BUFFER_MS / 2);
    let path = store.path().to_path_buf();
    let fetcher = fetcher(store, &endpoint.url);

    let creds = fetcher.get_credentials().await.expect("get succeeds");
    assert_eq!(creds.token, "fresh-access-0");
    assert_eq!(creds.account_id, "acct-1");
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 1);

    // The refresh grant carried the Go request shape.
    let requests = endpoint.requests.lock().await;
    assert_eq!(requests[0]["grant_type"], "refresh_token");
    assert_eq!(requests[0]["refresh_token"], "seed-refresh");
    assert_eq!(requests[0]["client_id"], "app_EMoamEEZ73f0CkXaXp7hrann");
    assert_eq!(requests[0]["scope"], "openid profile email");

    // Rotation persisted to disk.
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(raw["tokens"]["access_token"], "fresh-access-0");
    assert_eq!(raw["tokens"]["refresh_token"], "fresh-refresh-0");
    assert!(raw["tokens"]["expiresAt"].as_i64().unwrap() > now_ms());
}

#[tokio::test]
async fn refresh_failure_returns_stale_token() {
    let endpoint = start_token_endpoint(true).await;
    let dir = tempfile::tempdir().unwrap();
    let fetcher = fetcher(seeded_store(&dir, now_ms() - 1000), &endpoint.url);

    // Go parity: stale token returned, no error — the 401 retry is the backstop.
    let creds = fetcher.get_credentials().await.expect("get succeeds");
    assert_eq!(creds.token, "seed-access");
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn forced_refresh_propagates_errors() {
    let endpoint = start_token_endpoint(true).await;
    let dir = tempfile::tempdir().unwrap();
    let fetcher = fetcher(seeded_store(&dir, now_ms() - 1000), &endpoint.url);

    assert!(matches!(
        fetcher.refresh_credentials().await,
        Err(CredentialsError::Refresh(_))
    ));
}

#[tokio::test]
async fn concurrent_gets_single_flight_one_refresh() {
    let endpoint = start_token_endpoint(false).await;
    let dir = tempfile::tempdir().unwrap();
    let fetcher = Arc::new(fetcher(seeded_store(&dir, now_ms() - 1000), &endpoint.url));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let f = Arc::clone(&fetcher);
        handles.push(tokio::spawn(async move {
            f.get_credentials().await.expect("get succeeds").token
        }));
    }
    let mut tokens = Vec::new();
    for h in handles {
        tokens.push(h.await.unwrap());
    }

    // One refresh; the waiters re-read the persisted fresh state.
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 1);
    assert!(tokens.iter().all(|t| t == "fresh-access-0"), "{tokens:?}");
}

#[tokio::test]
async fn background_tick_refreshes_only_when_expiring() {
    let endpoint = start_token_endpoint(false).await;
    let dir = tempfile::tempdir().unwrap();
    let store = seeded_store(&dir, now_ms() + 8 * 3600 * 1000);
    let path = store.path().to_path_buf();
    let fetcher = fetcher(store, &endpoint.url);

    // Valid token: tick is a no-op.
    fetcher.check_and_refresh().await;
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 0);

    // Rewrite the file as expiring; tick refreshes and persists.
    std::fs::write(
        &path,
        json!({"tokens": {
            "access_token": "seed-access",
            "refresh_token": "seed-refresh",
            "account_id": "acct-1",
            "expiresAt": now_ms() - 1000,
        }})
        .to_string(),
    )
    .unwrap();
    fetcher.check_and_refresh().await;
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 1);
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(raw["tokens"]["access_token"], "fresh-access-0");
}

#[tokio::test]
async fn update_tokens_bootstraps_missing_file() {
    let endpoint = start_token_endpoint(false).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fresh").join("auth.json");
    let fetcher = fetcher(FsAuthFile::new(&path), &endpoint.url);

    // The /admin/credentials push path on an empty volume.
    // Expiry well outside the 60-min refresh buffer, so the follow-up
    // get_credentials must NOT refresh.
    fetcher
        .update_tokens(
            "pushed-access".to_string(),
            "pushed-refresh".to_string(),
            now_ms() + 8 * 3600 * 1000,
            Some("acct-9".to_string()),
        )
        .await
        .expect("bootstrap succeeds");

    let full = fetcher.full_credentials().await.expect("file readable");
    assert_eq!(full.access_token, "pushed-access");
    assert_eq!(full.user_id, "acct-9");

    let status_creds = fetcher.get_credentials().await.expect("get succeeds");
    assert_eq!(status_creds.token, "pushed-access");
    assert_eq!(status_creds.account_id, "acct-9");
    assert_eq!(endpoint.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn oauth_status_shape_via_trait() {
    let endpoint = start_token_endpoint(false).await;
    let dir = tempfile::tempdir().unwrap();
    let fetcher = fetcher(
        seeded_store(&dir, now_ms() + 2 * 3600 * 1000),
        &endpoint.url,
    );

    use codex_proxy_rs::credentials::CredentialsKind;
    assert_eq!(fetcher.kind(), CredentialsKind::OAuth);
    let full = fetcher.full_credentials().await.expect("full");
    assert_eq!(full.refresh_token, "seed-refresh");
    assert_eq!(full.user_id, "acct-1");
}

#[tokio::test]
async fn missing_file_get_credentials_is_unavailable() {
    let endpoint = start_token_endpoint(false).await;
    let dir = tempfile::tempdir().unwrap();
    let fetcher = fetcher(FsAuthFile::new(dir.path().join("none.json")), &endpoint.url);
    assert!(matches!(
        fetcher.get_credentials().await,
        Err(CredentialsError::Unavailable(_))
    ));
}

// Silence unused-field warning pattern: OAuthCredentials used via constructor
// in other tests.
#[allow(dead_code)]
fn _types(_: OAuthCredentials) {}
