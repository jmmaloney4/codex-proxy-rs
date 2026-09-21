//! Router mode (ADR 006 §3a / ADR 007): front the single-account codex-proxy
//! pods, pin each conversation to one account, and reverse-proxy the request to
//! that account's pod.
//!
//! The backend pods already emit final OpenAI-format responses, so the router
//! does a plain byte reverse-proxy (no transform/relay-rewrite). Account
//! affinity is best-effort: a Redis miss/outage degrades to stateless
//! round-robin, never a failed request (ADR 006 §5c).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use serde_json::Value;

use crate::affinity::Pin;
use crate::conversation::resolve_conversation_key;
use crate::server::AppState;
use crate::server::error::ApiError;
use crate::server::stream::{proxy_response, sanitized_headers};

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
    /// Cooldown scoped to (slug, model) rather than the whole account: a
    /// model-tier-gate 400 means this account structurally does not support
    /// this model, not that the account itself is unhealthy. Cooling the
    /// account globally would incorrectly deprioritize it for models it DOES
    /// support (issue #23).
    model_cooldown_until: Mutex<HashMap<(String, String), Instant>>,
}

impl AccountPool {
    /// Parse the `slug=url` comma-separated spec from `CODEX_PROXY_ACCOUNTS`,
    /// e.g. `main=http://...:9879,codex2=http://...:9879`. Trailing slashes on
    /// URLs are trimmed so path joining is unambiguous.
    pub fn parse(spec: &str) -> anyhow::Result<Self> {
        let mut accounts = Vec::new();
        let mut seen_slugs = HashSet::new();
        let mut seen_urls = HashSet::new();
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
            // Fail loudly at startup on a malformed URL rather than at request
            // time. Must be a bare http(s) origin: `proxy_to_pod` builds the
            // target as `{base_url}{path_and_query}`, so a path/query/fragment on
            // the base (e.g. `https://pod/v1`) would corrupt every target.
            match reqwest::Url::parse(url) {
                Ok(parsed) if !matches!(parsed.scheme(), "http" | "https") => anyhow::bail!(
                    "account url scheme must be http/https, got {}: {entry}",
                    parsed.scheme()
                ),
                Ok(parsed)
                    if parsed.path() != "/"
                        || parsed.query().is_some()
                        || parsed.fragment().is_some() =>
                {
                    anyhow::bail!(
                        "account url must be a bare origin (no path/query/fragment): {entry}"
                    )
                }
                Ok(_) => {}
                Err(err) => anyhow::bail!("invalid account url ({err}): {entry}"),
            }
            // Duplicate slugs would make pins resolve ambiguously (a pin stores
            // only the slug). Duplicate URLs (distinct slugs → the same pod)
            // defeat failover — cooling one slug just re-routes to the same
            // failing pod. Reject both.
            if !seen_slugs.insert(slug.to_string()) {
                anyhow::bail!("duplicate account slug: {slug}");
            }
            if !seen_urls.insert(url.to_string()) {
                anyhow::bail!("duplicate account url ({url}) — slugs must point at distinct pods");
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
            model_cooldown_until: Mutex::new(HashMap::new()),
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

    fn is_model_cooling(&self, slug: &str, model: &str) -> bool {
        if model.is_empty() {
            return false;
        }
        self.model_cooldown_until
            .lock()
            .unwrap()
            .get(&(slug.to_string(), model.to_string()))
            .is_some_and(|until| *until > Instant::now())
    }

    /// Mark a (slug, model) pair as cooling after a model-tier-gate 400 (this
    /// account's ChatGPT plan does not include that model) — see
    /// `is_model_cooling`. A no-op for an empty model string, which would
    /// otherwise let requests with no `model` field pollute the cooldown map
    /// with a meaningless `(slug, "")` entry.
    pub fn cooldown_model(&self, slug: &str, model: &str) {
        if model.is_empty() {
            return;
        }
        self.model_cooldown_until.lock().unwrap().insert(
            (slug.to_string(), model.to_string()),
            Instant::now() + COOLDOWN,
        );
    }

    /// The pinned account, if `slug` is known and not currently cooling —
    /// account-wide, or for this specific `model` (issue #23).
    pub fn pinned(&self, slug: &str, model: &str) -> Option<Account> {
        if self.is_cooling(slug) || self.is_model_cooling(slug, model) {
            return None;
        }
        self.accounts.iter().find(|a| a.slug == slug).cloned()
    }

    /// Pick an account for a new (or re-pinned) conversation: round-robin over
    /// healthy accounts, skipping `exclude` and any account known to
    /// tier-gate `model`. Falls back to ignoring the (transient) account-wide
    /// cooldown, then to any account, so a request is never dropped for lack
    /// of a pick — but a model-tier-gate is a structural fact, not a transient
    /// blip, so it is never ignored while a healthier pick might still exist
    /// (issue #23).
    pub fn pick(&self, exclude: Option<&str>, model: &str) -> Option<Account> {
        let n = self.accounts.len();
        if n == 0 {
            return None;
        }
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        // Pass 1: healthy (account- and model-wise) and not excluded.
        for i in 0..n {
            let a = &self.accounts[(start + i) % n];
            if Some(a.slug.as_str()) == exclude
                || self.is_cooling(&a.slug)
                || self.is_model_cooling(&a.slug, model)
            {
                continue;
            }
            return Some(a.clone());
        }
        // Pass 2: not excluded, account-cooldown ignored (better to try than
        // fail), but a known tier-gate is still skipped — retrying it can
        // only fail again.
        for i in 0..n {
            let a = &self.accounts[(start + i) % n];
            if Some(a.slug.as_str()) == exclude || self.is_model_cooling(&a.slug, model) {
                continue;
            }
            return Some(a.clone());
        }
        // Pass 3: every account either is excluded or tier-gates `model` —
        // still return something so a request is never dropped for lack of a
        // pick (the caller will see the real, correct 400 if truly no
        // account supports this model).
        self.accounts.first().cloned()
    }
}

/// W3C trace-context headers forwarded to the backend pod so its spans nest in
/// the same trace as the router/LiteLLM call (ADR 005 distributed tracing).
const FORWARDED_HEADERS: [&str; 2] = ["traceparent", "tracestate"];

/// Forward the (already OpenAI-shaped) request to a backend pod verbatim. The
/// router sets `authorization` (the shared `ADMIN_API_KEY`, which gates the pod)
/// and `content-type` itself, and forwards W3C trace context; everything else is
/// intentionally not forwarded (codex headers are pod-generated; hop-by-hop
/// headers must not cross the proxy). `path_and_query` preserves any query
/// string on the original request target.
async fn proxy_to_pod(
    client: &reqwest::Client,
    base_url: &str,
    path_and_query: &str,
    bearer: &str,
    inbound: &HeaderMap,
    body: Bytes,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut req = client
        .post(format!("{base_url}{path_and_query}"))
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json");
    for name in FORWARDED_HEADERS {
        if let Some(value) = inbound.get(name) {
            req = req.header(name, value.clone());
        }
    }
    req.body(body).send().await
}

fn is_retryable(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Substring identifying a model-tier-gate 400 from the ChatGPT backend — an
/// account-capability fact ("this ChatGPT plan doesn't include this model"),
/// not a request problem (issue #23). Matched as a substring of the raw body
/// regardless of JSON envelope shape: there is no stable machine-readable
/// error code to key on, mirroring how this crate already treats the
/// backend's other bespoke error bodies (`request.rs`'s `{"detail": "..."}`
/// handling for the stream-flag and `user`-param rejections).
const TIER_GATE_MARKER: &str = "model is not supported when using Codex with a ChatGPT account";

/// A 400 response whose body has already been read to classify it. Building
/// the final `Response` is deferred to `into_response` so the caller can
/// decide first whether to emit it or discard it in favor of a retry.
struct Buffered400 {
    headers: HeaderMap,
    body: Bytes,
    /// Whether the body matched `TIER_GATE_MARKER`. `false` also covers the
    /// (very rare) case where the body failed to read — see `classify_400`.
    tier_gate: bool,
}

impl Buffered400 {
    fn into_response(self) -> Response {
        let mut response = Response::new(Body::from(self.body));
        *response.status_mut() = StatusCode::BAD_REQUEST;
        *response.headers_mut() = self.headers;
        response
    }
}

/// Read and classify a 400 response's body. A body-read failure degrades to
/// "not a tier-gate" (terminal, empty body) rather than a guess either way:
/// we genuinely don't know, and treating an unreadable body as retryable
/// risks a reroute loop on a connection that is already misbehaving.
async fn classify_400(resp: reqwest::Response) -> Buffered400 {
    let headers = sanitized_headers(resp.headers());
    let body = resp.bytes().await.unwrap_or_default();
    let tier_gate = String::from_utf8_lossy(&body).contains(TIER_GATE_MARKER);
    Buffered400 {
        headers,
        body,
        tier_gate,
    }
}

/// Material to fall back to if a sibling-account retry also fails or can't be
/// attempted: either the primary's still-unconsumed response (429/5xx —
/// stream lazily, unchanged from before issue #23) or its already-buffered
/// body (a model-tier-gate 400 — the body was read to classify it, so it must
/// be replayed from the buffer instead of re-streamed).
enum FallbackBody {
    Response(reqwest::Response),
    Buffered(Buffered400),
}

impl FallbackBody {
    fn into_response(self) -> Response {
        match self {
            FallbackBody::Response(resp) => proxy_response(resp),
            FallbackBody::Buffered(buffered) => buffered.into_response(),
        }
    }
}

/// `Ok` mirrors the two "we have something to emit" cases above; `Err` is a
/// primary attempt that failed to connect at all.
type Fallback = Result<FallbackBody, reqwest::Error>;

/// The outcome of one attempt against one account.
enum Primary {
    /// Ready to return now — success, or a terminal (non-retryable,
    /// non-tier-gate) status.
    Terminal(Response),
    /// Not final: 429/5xx/connection-error (account-wide problem) or a
    /// model-tier-gate 400 (this (slug, model) pairing only — `tier_gate`).
    Retry { fallback: Fallback, tier_gate: bool },
}

/// Classify a completed attempt. Only a 400 needs its body read (to tell a
/// genuine bad request apart from a model-tier-gate entitlement error); every
/// other status is decided from the status line alone, exactly as before
/// issue #23.
async fn classify(result: Result<reqwest::Response, reqwest::Error>) -> Primary {
    let resp = match result {
        Ok(resp) => resp,
        Err(err) => {
            return Primary::Retry {
                fallback: Err(err),
                tier_gate: false,
            };
        }
    };
    if resp.status() == reqwest::StatusCode::BAD_REQUEST {
        let buffered = classify_400(resp).await;
        return if buffered.tier_gate {
            Primary::Retry {
                fallback: Ok(FallbackBody::Buffered(buffered)),
                tier_gate: true,
            }
        } else {
            Primary::Terminal(buffered.into_response())
        };
    }
    if is_retryable(resp.status()) {
        Primary::Retry {
            fallback: Ok(FallbackBody::Response(resp)),
            tier_gate: false,
        }
    } else {
        Primary::Terminal(proxy_response(resp))
    }
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
    // Preserve any query string on the request target, not just the path.
    let target = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| uri.path())
        .to_string();

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
    let (account, mut source) = match pinned_slug.as_deref().and_then(|s| pool.pinned(s, &model)) {
        Some(a) => (a, "pinned"),
        None => (
            pool.pick(None, &model)
                .ok_or(ApiError::Internal("router mode: account pool is empty"))?,
            if state.affinity.is_some() {
                "new"
            } else {
                "fallback"
            },
        ),
    };

    let first = proxy_to_pod(
        &state.http,
        &account.url,
        &target,
        &bearer,
        &headers,
        body.clone(),
    )
    .await;

    let (fallback, tier_gate) = match classify(first).await {
        Primary::Terminal(response) => {
            maybe_pin(&state, &conversation_key, &account.slug, &model, source).await;
            log_route(
                &conversation_key_fp,
                &account.slug,
                source,
                response.status().as_u16(),
            );
            return Ok(response);
        }
        Primary::Retry {
            fallback,
            tier_gate,
        } => (fallback, tier_gate),
    };

    // Primary failed: 429/5xx/connection-error is an account-wide problem —
    // cool the whole account, as before. A model-tier-gate 400 is scoped to
    // this (slug, model) pairing only (issue #23).
    if tier_gate {
        pool.cooldown_model(&account.slug, &model);
    } else {
        pool.cooldown(&account.slug);
    }
    if let (Some(store), Some(key)) = (&state.affinity, &conversation_key) {
        store.clear(key).await;
    }

    match pool.pick(Some(&account.slug), &model) {
        // A genuinely different account is available — try it.
        Some(alt) if alt.slug != account.slug => {
            let alt_result =
                proxy_to_pod(&state.http, &alt.url, &target, &bearer, &headers, body).await;
            match classify(alt_result).await {
                // Only pin the alt if it actually succeeded — pinning a target
                // that *also* failed would route future turns to a failing
                // account.
                Primary::Terminal(response) => {
                    source = "repinned";
                    maybe_pin(&state, &conversation_key, &alt.slug, &model, source).await;
                    log_route(
                        &conversation_key_fp,
                        &alt.slug,
                        source,
                        response.status().as_u16(),
                    );
                    Ok(response)
                }
                // The sibling failed too — cool it down at the same scope as
                // the primary above, leave the conversation unpinned (the
                // next turn re-picks); still surface a response so the client
                // sees the real upstream status.
                Primary::Retry {
                    fallback: alt_fallback,
                    tier_gate: alt_tier_gate,
                } => {
                    // Matches the pre-#23 asymmetry: only a completed (not
                    // connection-failed) attempt cools the alt down — a
                    // pod we couldn't even reach tells us nothing to cool.
                    if alt_fallback.is_ok() {
                        if alt_tier_gate {
                            pool.cooldown_model(&alt.slug, &model);
                        } else {
                            pool.cooldown(&alt.slug);
                        }
                    }
                    match alt_fallback {
                        Err(err) => {
                            fallback_or_error(fallback, &conversation_key_fp, &account.slug, err)
                        }
                        Ok(body) => {
                            let response = body.into_response();
                            log_route(
                                &conversation_key_fp,
                                &alt.slug,
                                "repin_failed",
                                response.status().as_u16(),
                            );
                            Ok(response)
                        }
                    }
                }
            }
        }
        // Only one account in the pool — nothing to fail over to.
        _ => fallback_or_error_single(fallback, &conversation_key_fp, &account.slug),
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
    first: Fallback,
    conversation_key_fp: &str,
    first_slug: &str,
    repin_err: reqwest::Error,
) -> Result<Response, ApiError> {
    match first {
        Ok(body) => {
            tracing::warn!(error = %repin_err, "re-pin send failed; streaming primary response");
            let response = body.into_response();
            log_route(
                conversation_key_fp,
                first_slug,
                "repin_failed",
                response.status().as_u16(),
            );
            Ok(response)
        }
        Err(first_err) => {
            tracing::error!(primary = %first_err, repin = %repin_err, "router: both accounts unreachable");
            Err(ApiError::Internal("router: all codex accounts unreachable"))
        }
    }
}

fn fallback_or_error_single(
    first: Fallback,
    conversation_key_fp: &str,
    slug: &str,
) -> Result<Response, ApiError> {
    match first {
        // Single-account pool: stream whatever the one account returned (even a
        // 429/5xx, or an already-buffered tier-gate 400) rather than fail —
        // the client sees the real upstream status.
        Ok(body) => {
            let response = body.into_response();
            log_route(
                conversation_key_fp,
                slug,
                "single",
                response.status().as_u16(),
            );
            Ok(response)
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
        assert_eq!(p.pinned("main", "").unwrap().url, "http://a:9879");
        // trailing slash trimmed
        assert_eq!(p.pinned("codex2", "").unwrap().url, "http://b:9879");
    }

    #[test]
    fn rejects_empty_or_malformed_spec() {
        assert!(AccountPool::parse("").is_err());
        assert!(AccountPool::parse("noequals").is_err());
        assert!(AccountPool::parse("=http://x").is_err());
        assert!(AccountPool::parse("slug=").is_err());
        // URL must carry an http(s) scheme.
        assert!(AccountPool::parse("slug=ftp://x").is_err());
        assert!(AccountPool::parse("slug=host:9879").is_err());
        // Duplicate slugs are ambiguous for pin resolution.
        assert!(AccountPool::parse("main=http://a,main=http://b").is_err());
        // Duplicate URLs defeat failover.
        assert!(AccountPool::parse("main=http://a,codex2=http://a").is_err());
        // Base URL must be a bare origin (no path/query/fragment) — proxy_to_pod
        // appends the request path.
        assert!(AccountPool::parse("slug=http://pod/v1").is_err());
        assert!(AccountPool::parse("slug=http://pod?x=1").is_err());
    }

    #[test]
    fn pinned_returns_none_for_unknown_or_cooling() {
        let p = AccountPool::parse("main=http://a,codex2=http://b").unwrap();
        assert!(p.pinned("nope", "").is_none());
        assert!(p.pinned("main", "").is_some());
        p.cooldown("main");
        assert!(
            p.pinned("main", "").is_none(),
            "cooling slug is not a valid pin"
        );
        assert!(p.pinned("codex2", "").is_some());
    }

    #[test]
    fn pick_skips_cooling_and_excluded() {
        let p = AccountPool::parse("a=http://a,b=http://b,c=http://c").unwrap();
        p.cooldown("a");
        // Over several picks, never returns the cooling account 'a'.
        for _ in 0..10 {
            assert_ne!(p.pick(None, "").unwrap().slug, "a");
        }
        // Excluding 'b' while 'a' cools leaves only 'c' as healthy.
        assert_eq!(p.pick(Some("b"), "").unwrap().slug, "c");
    }

    #[test]
    fn pick_falls_back_when_all_cooling() {
        let p = AccountPool::parse("a=http://a,b=http://b").unwrap();
        p.cooldown("a");
        p.cooldown("b");
        // No healthy accounts, but a pick is still returned (pass 2).
        assert!(p.pick(None, "").is_some());
    }

    // ---- issue #23: model-tier-gate cooldown is scoped, not account-wide ----

    #[test]
    fn model_cooldown_is_scoped_to_the_pairing_not_the_whole_account() {
        let p = AccountPool::parse("a=http://a,b=http://b").unwrap();
        p.cooldown_model("a", "gpt-6-astra");
        // "a" is unusable for the tier-gated model...
        assert!(p.pinned("a", "gpt-6-astra").is_none());
        // ...but still perfectly fine for any other model, unlike the
        // account-wide `cooldown` above.
        assert!(p.pinned("a", "gpt-5.6-luna").is_some());
        assert!(p.pinned("a", "").is_some());
    }

    #[test]
    fn cooldown_model_is_a_noop_for_an_empty_model() {
        let p = AccountPool::parse("a=http://a").unwrap();
        p.cooldown_model("a", "");
        assert!(
            p.pinned("a", "").is_some(),
            "an empty model must never be cooled down"
        );
    }

    #[test]
    fn pick_skips_a_model_tier_gate_but_still_picks_the_account_for_other_models() {
        let p = AccountPool::parse("a=http://a,b=http://b").unwrap();
        p.cooldown_model("a", "gpt-6-astra");
        // Every pick for the tier-gated model must land on "b".
        for _ in 0..10 {
            assert_eq!(p.pick(None, "gpt-6-astra").unwrap().slug, "b");
        }
        // But "a" is still in rotation for a different model.
        let mut saw_a = false;
        for _ in 0..10 {
            if p.pick(None, "gpt-5.6-luna").unwrap().slug == "a" {
                saw_a = true;
            }
        }
        assert!(saw_a, "model cooldown must not leak into other models");
    }

    #[test]
    fn pick_never_ignores_a_model_tier_gate_even_when_all_cool() {
        let p = AccountPool::parse("a=http://a,b=http://b").unwrap();
        p.cooldown_model("a", "gpt-6-astra");
        p.cooldown_model("b", "gpt-6-astra");
        // Unlike the transient account-wide cooldown (pass 2 ignores it), a
        // structural tier-gate is never worth retrying — but a pick is still
        // returned so a request is never dropped for lack of one.
        assert!(p.pick(None, "gpt-6-astra").is_some());
    }
}
