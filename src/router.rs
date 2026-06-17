//! Router mode (ADR 006 §3a / ADR 007): front the single-account codex-proxy
//! pods, pin each conversation to one account, and reverse-proxy the request to
//! that account's pod.
//!
//! The backend pods already emit final OpenAI-format responses, so the router
//! does a plain byte reverse-proxy (no transform/relay-rewrite). Account
//! affinity is best-effort: a Redis miss/outage degrades to stateless
//! round-robin, never a failed request (ADR 006 §5c).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::Response;
use serde_json::Value;

use crate::affinity::Pin;
use crate::conversation::resolve_conversation_key;
use crate::server::AppState;
use crate::server::error::ApiError;
use crate::server::stream::proxy_response;

/// How long a slug is skipped for new picks after it returns 429/5xx.
const COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub slug: String,
    /// Base URL of the account's pod, e.g.
    /// `http://codex-proxy-main.codex-proxy.svc.cluster.local:9879`.
    pub url: String,
}

/// The set of backend accounts the router can route to, with round-robin
/// selection for new conversations and a short cooldown on failing accounts.
pub struct AccountPool {
    accounts: Vec<Account>,
    next: AtomicUsize,
    cooldown_until: Mutex<HashMap<String, Instant>>,
}

impl AccountPool {
    /// Parse the `slug=url` comma-separated spec from `CODEX_PROXY_ACCOUNTS`,
    /// e.g. `main=http://...:9879,codex2=http://...:9879`. Trailing slashes on
    /// URLs are trimmed so path joining is unambiguous.
    pub fn parse(spec: &str) -> anyhow::Result<Self> {
        let mut accounts = Vec::new();
        for entry in spec.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (slug, url) = entry
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("invalid account entry (want slug=url): {entry}"))?;
            let slug = slug.trim();
            let url = url.trim().trim_end_matches('/');
            if slug.is_empty() || url.is_empty() {
                anyhow::bail!("invalid account entry (empty slug or url): {entry}");
            }
            accounts.push(Account {
                slug: slug.to_string(),
                url: url.to_string(),
            });
        }
        if accounts.is_empty() {
            anyhow::bail!("CODEX_PROXY_ACCOUNTS must list at least one slug=url account");
        }
        Ok(Self {
            accounts,
            next: AtomicUsize::new(0),
            cooldown_until: Mutex::new(HashMap::new()),
        })
    }

    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    fn is_cooling(&self, slug: &str) -> bool {
        self.cooldown_until
            .lock()
            .unwrap()
            .get(slug)
            .is_some_and(|until| *until > Instant::now())
    }

    /// Mark a slug as cooling-down after it failed (429/5xx/connection error).
    pub fn cooldown(&self, slug: &str) {
        self.cooldown_until
            .lock()
            .unwrap()
            .insert(slug.to_string(), Instant::now() + COOLDOWN);
    }

    /// The pinned account, if `slug` is known and not currently cooling.
    pub fn pinned(&self, slug: &str) -> Option<Account> {
        if self.is_cooling(slug) {
            return None;
        }
        self.accounts.iter().find(|a| a.slug == slug).cloned()
    }

    /// Pick an account for a new (or re-pinned) conversation: round-robin over
    /// healthy accounts, skipping `exclude`. Falls back to ignoring cooldown,
    /// then to any account, so a request is never dropped for lack of a pick.
    pub fn pick(&self, exclude: Option<&str>) -> Option<Account> {
        let n = self.accounts.len();
        if n == 0 {
            return None;
        }
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        // Pass 1: healthy and not excluded.
        for i in 0..n {
            let a = &self.accounts[(start + i) % n];
            if Some(a.slug.as_str()) == exclude || self.is_cooling(&a.slug) {
                continue;
            }
            return Some(a.clone());
        }
        // Pass 2: not excluded (cooldown ignored — better to try than fail).
        for i in 0..n {
            let a = &self.accounts[(start + i) % n];
            if Some(a.slug.as_str()) == exclude {
                continue;
            }
            return Some(a.clone());
        }
        // Pass 3: only the excluded account remains.
        self.accounts.first().cloned()
    }
}

/// Forward the (already OpenAI-shaped) request to a backend pod verbatim. No
/// codex headers and no credential refresh — the pod owns those. The shared
/// `ADMIN_API_KEY` bearer authenticates router→pod (same key the router itself
/// is gated on).
async fn proxy_to_pod(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    bearer: &str,
    body: Bytes,
) -> Result<reqwest::Response, reqwest::Error> {
    client
        .post(format!("{base_url}{path}"))
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
}

fn is_retryable(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Router-mode handler for `/v1/chat/completions` and `/v1/responses`: resolve
/// the conversation key, pick/pin an account, and reverse-proxy to its pod with
/// a single re-pin retry on failure.
pub async fn proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pool = state.accounts.as_ref().ok_or(ApiError::Internal(
        "router mode: no account pool configured",
    ))?;
    let bearer = state
        .admin_api_key
        .as_ref()
        .ok_or(ApiError::AdminNotConfigured)?
        .clone();
    let path = uri.path().to_string();

    let request: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("Failed to parse request body".to_string()))?;
    let conversation = resolve_conversation_key(&headers, &request);
    let conversation_key = conversation.map(|c| c.key);
    let conversation_key_fp = conversation_key
        .as_deref()
        .map(crate::request::hash_to_uuid)
        .unwrap_or_default();
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Look up an existing pin (best-effort).
    let pinned_slug = match (&state.affinity, &conversation_key) {
        (Some(store), Some(key)) => store.get(key).await.map(|p| p.slug),
        _ => None,
    };

    // Primary account: the live pin if usable, else a fresh pick.
    let (account, mut source) = match pinned_slug.as_deref().and_then(|s| pool.pinned(s)) {
        Some(a) => (a, "pinned"),
        None => (
            pool.pick(None)
                .ok_or(ApiError::Internal("router mode: account pool is empty"))?,
            if state.affinity.is_some() {
                "new"
            } else {
                "fallback"
            },
        ),
    };

    let first = proxy_to_pod(&state.http, &account.url, &path, &bearer, body.clone()).await;
    let first_ok_terminal = matches!(&first, Ok(resp) if !is_retryable(resp.status()));

    if first_ok_terminal {
        maybe_pin(&state, &conversation_key, &account.slug, &model, source).await;
        let resp = first.expect("checked Ok");
        log_route(
            &conversation_key_fp,
            &account.slug,
            source,
            resp.status().as_u16(),
        );
        return Ok(proxy_response(resp));
    }

    // Primary failed (429/5xx or connection error) → cool it down, re-pin once.
    pool.cooldown(&account.slug);
    if let (Some(store), Some(key)) = (&state.affinity, &conversation_key) {
        store.clear(key).await;
    }

    match pool.pick(Some(&account.slug)) {
        // A genuinely different account is available — try it.
        Some(alt) if alt.slug != account.slug => {
            source = "repinned";
            match proxy_to_pod(&state.http, &alt.url, &path, &bearer, body).await {
                Ok(resp) => {
                    maybe_pin(&state, &conversation_key, &alt.slug, &model, source).await;
                    log_route(
                        &conversation_key_fp,
                        &alt.slug,
                        source,
                        resp.status().as_u16(),
                    );
                    Ok(proxy_response(resp))
                }
                // Re-pin send failed: stream the first response if we have one,
                // else surface a gateway error.
                Err(err) => fallback_or_error(first, &conversation_key_fp, &account.slug, err),
            }
        }
        // Only one account in the pool — nothing to fail over to.
        _ => fallback_or_error_single(first, &conversation_key_fp, &account.slug),
    }
}

/// Persist/refresh the pin unless this was the no-store fallback path.
async fn maybe_pin(
    state: &AppState,
    conversation_key: &Option<String>,
    slug: &str,
    model: &str,
    source: &str,
) {
    if source == "fallback" {
        return;
    }
    if let (Some(store), Some(key)) = (&state.affinity, conversation_key) {
        store
            .put(
                key,
                &Pin {
                    slug: slug.to_string(),
                    model: model.to_string(),
                },
            )
            .await;
    }
}

fn log_route(conversation_key_fp: &str, slug: &str, source: &str, status: u16) {
    tracing::info!(
        conversation_key_fp,
        account_slug = slug,
        account_source = source,
        status,
        "router proxied request",
    );
}

fn fallback_or_error(
    first: Result<reqwest::Response, reqwest::Error>,
    conversation_key_fp: &str,
    first_slug: &str,
    repin_err: reqwest::Error,
) -> Result<Response, ApiError> {
    match first {
        Ok(resp) => {
            tracing::warn!(error = %repin_err, "re-pin send failed; streaming primary response");
            log_route(
                conversation_key_fp,
                first_slug,
                "repin_failed",
                resp.status().as_u16(),
            );
            Ok(proxy_response(resp))
        }
        Err(first_err) => {
            tracing::error!(primary = %first_err, repin = %repin_err, "router: both accounts unreachable");
            Err(ApiError::Internal("router: all codex accounts unreachable"))
        }
    }
}

fn fallback_or_error_single(
    first: Result<reqwest::Response, reqwest::Error>,
    conversation_key_fp: &str,
    slug: &str,
) -> Result<Response, ApiError> {
    match first {
        // Single-account pool: stream whatever the one account returned (even a
        // 429/5xx) rather than fail — the client sees the real upstream status.
        Ok(resp) => {
            log_route(conversation_key_fp, slug, "single", resp.status().as_u16());
            Ok(proxy_response(resp))
        }
        Err(first_err) => {
            tracing::error!(error = %first_err, "router: only account unreachable");
            Err(ApiError::Internal("router: codex account unreachable"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_account_spec() {
        let p = AccountPool::parse(" main=http://a:9879 , codex2=http://b:9879/ ").unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p.pinned("main").unwrap().url, "http://a:9879");
        // trailing slash trimmed
        assert_eq!(p.pinned("codex2").unwrap().url, "http://b:9879");
    }

    #[test]
    fn rejects_empty_or_malformed_spec() {
        assert!(AccountPool::parse("").is_err());
        assert!(AccountPool::parse("noequals").is_err());
        assert!(AccountPool::parse("=http://x").is_err());
        assert!(AccountPool::parse("slug=").is_err());
    }

    #[test]
    fn pinned_returns_none_for_unknown_or_cooling() {
        let p = AccountPool::parse("main=http://a,codex2=http://b").unwrap();
        assert!(p.pinned("nope").is_none());
        assert!(p.pinned("main").is_some());
        p.cooldown("main");
        assert!(
            p.pinned("main").is_none(),
            "cooling slug is not a valid pin"
        );
        assert!(p.pinned("codex2").is_some());
    }

    #[test]
    fn pick_skips_cooling_and_excluded() {
        let p = AccountPool::parse("a=http://a,b=http://b,c=http://c").unwrap();
        p.cooldown("a");
        // Over several picks, never returns the cooling account 'a'.
        for _ in 0..10 {
            assert_ne!(p.pick(None).unwrap().slug, "a");
        }
        // Excluding 'b' while 'a' cools leaves only 'c' as healthy.
        assert_eq!(p.pick(Some("b")).unwrap().slug, "c");
    }

    #[test]
    fn pick_falls_back_when_all_cooling() {
        let p = AccountPool::parse("a=http://a,b=http://b").unwrap();
        p.cooldown("a");
        p.cooldown("b");
        // No healthy accounts, but a pick is still returned (pass 2).
        assert!(p.pick(None).is_some());
    }
}
