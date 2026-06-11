use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use codex_proxy_rs::config::{Config, CredsStore, init_tracing};
use codex_proxy_rs::credentials::{CredentialsFetcher, EnvCredentials};
use codex_proxy_rs::relay::RelayConfig;
use codex_proxy_rs::server::{AppState, router};
use codex_proxy_rs::upstream::{UPSTREAM_URL, build_upstream_client};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    init_tracing(&config.env);

    let http = build_upstream_client();
    let creds: Arc<dyn CredentialsFetcher> = match config.creds_store {
        CredsStore::Env => Arc::new(EnvCredentials::new(
            config.anthropic_api_key.clone(),
            config.claude_user_id.clone(),
        )),
    };

    // Startup validation, warn-only like Go.
    match creds.get_credentials().await {
        // Identifiers and token material stay out of the logs (tighter than
        // Go, which logs the account id and a token preview).
        Ok(c) => tracing::info!(
            token_set = !c.token.is_empty(),
            account_id_set = !c.account_id.is_empty(),
            "credentials loaded",
        ),
        Err(err) => tracing::warn!(error = %err, "could not load credentials at startup"),
    }

    let state = AppState {
        creds,
        http,
        relay: RelayConfig {
            keepalive_interval: Duration::from_secs(config.keepalive_secs),
        },
        upstream_url: UPSTREAM_URL.into(),
        admin_api_key: config.admin_api_key.clone().map(Into::into),
    };

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.port)).await?;
    tracing::info!(port = config.port, "starting codex-proxy");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// SIGINT or SIGTERM (the k8s stop signal).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl-c handler");
    };
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
