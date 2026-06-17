//! Axum HTTP server: routes, shared state, and the Go `server.go` handler
//! ports. See ADR 004 for the architecture and the divergence register.

pub mod admin;
pub mod chat;
pub mod error;
pub mod middleware;
pub mod misc;
pub mod responses;
pub mod stream;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};

use crate::affinity::AffinityStore;
use crate::config::ProxyMode;
use crate::credentials::CredentialsFetcher;
use crate::relay::RelayConfig;
use crate::router::AccountPool;

#[derive(Clone)]
pub struct AppState {
    /// Backend (default) or router. Selects the route table (see [`router`]).
    pub mode: ProxyMode,
    pub creds: Arc<dyn CredentialsFetcher>,
    pub http: reqwest::Client,
    pub relay: RelayConfig,
    /// `upstream::UPSTREAM_URL` in production; a mock server URL in tests.
    pub upstream_url: Arc<str>,
    /// Snapshot of `ADMIN_API_KEY`; `None` → 500 on gated routes (Go parity).
    pub admin_api_key: Option<Arc<str>>,
    /// Router mode only: the backend accounts to route across.
    pub accounts: Option<Arc<AccountPool>>,
    /// Router mode only: conversation→account affinity store. `None` → the
    /// router routes statelessly (no pinning).
    pub affinity: Option<Arc<dyn AffinityStore>>,
}

/// Build the full router. Route table mirrors Go `setupRoutes`
/// (`server.go:57-65`): the data plane and `/admin/*` sit behind the admin
/// gate; `/v1/models` and `/health` are open; everything is logged.
pub fn router(state: AppState) -> Router {
    // Router mode fronts the backend pods: the data plane reverse-proxies and
    // there is no local credential store, so the /admin/credentials routes are
    // omitted. Both modes keep the admin-key gate on the data plane.
    let gated = match state.mode {
        ProxyMode::Backend => Router::new()
            .route("/v1/chat/completions", post(chat::chat_completions))
            .route("/v1/responses", post(responses::responses))
            .route("/admin/credentials", post(admin::update_credentials))
            .route("/admin/credentials/status", get(admin::credentials_status)),
        ProxyMode::Router => Router::new()
            .route("/v1/chat/completions", post(crate::router::proxy))
            .route("/v1/responses", post(crate::router::proxy)),
    }
    .route_layer(axum::middleware::from_fn_with_state(
        state.clone(),
        middleware::admin_auth,
    ));

    Router::new()
        .merge(gated)
        .route("/v1/models", get(misc::models))
        .route("/health", get(misc::health))
        .fallback(misc::not_found)
        .layer(axum::middleware::from_fn(middleware::log_requests))
        .with_state(state)
}
