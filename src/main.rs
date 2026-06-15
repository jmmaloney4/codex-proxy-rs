use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use codex_proxy_rs::config::{Config, CredsStore, init_tracing};
use codex_proxy_rs::credentials::{CredentialsFetcher, EnvCredentials, FsAuthFile, OAuthFetcher};
use codex_proxy_rs::relay::RelayConfig;
use codex_proxy_rs::server::{AppState, router};
use codex_proxy_rs::upstream::{UPSTREAM_URL, build_upstream_client};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    // `Some` only when OTLP export is configured; held for an explicit flush on
    // shutdown so the batch processor isn't dropped mid-export (ADR 005 §8).
    let tracer_provider = init_tracing(&config.env);

    let http = build_upstream_client();
    let creds: Arc<dyn CredentialsFetcher> = match config.creds_store {
        CredsStore::Env => Arc::new(EnvCredentials::new(
            config.anthropic_api_key.clone(),
            config.claude_user_id.clone(),
        )),
        CredsStore::Fs => {
            let path = match config.creds_path.clone() {
                Some(path) => {
                    if !path.is_absolute() {
                        anyhow::bail!("--creds-path must be an absolute path");
                    }
                    path
                }
                None => default_creds_path()?,
            };
            tracing::info!(path = %path.display(), "using filesystem credential store");
            let fetcher = Arc::new(OAuthFetcher::new(FsAuthFile::new(path), http.clone()));
            fetcher.spawn_background_refresh();
            fetcher
        }
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
    let serve_result = axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await;

    // Drain buffered spans before exit — and before propagating any serve
    // error, since a failing serve/shutdown is exactly when the last spans
    // matter most. `global::shutdown_tracer_provider` was removed in the 0.x
    // line, so flush the held provider directly. Errors here are non-fatal:
    // the process is already shutting down.
    if let Some(provider) = tracer_provider {
        if let Err(err) = provider.force_flush() {
            tracing::warn!(error = %err, "failed to flush OTLP spans on shutdown");
        }
        if let Err(err) = provider.shutdown() {
            tracing::warn!(error = %err, "failed to shut down tracer provider");
        }
    }

    serve_result?;
    Ok(())
}

/// Go `DefaultCredsPath`: $XDG_CONFIG_HOME/codex-proxy/auth.json, with the
/// ~/.config fallback. Unlike Go (which ignores a missing HOME and silently
/// produces a relative path), an environment with neither variable set is an
/// error — rotated tokens must never land in the working directory.
fn default_creds_path() -> anyhow::Result<std::path::PathBuf> {
    // Relative values are ignored per the XDG base-directory spec ("all
    // paths must be absolute") — they would land auth.json under the
    // working directory.
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty() && p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty() && p.is_absolute())
                .map(|home| home.join(".config"))
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "--creds-path is required when neither XDG_CONFIG_HOME nor HOME is set to an absolute path"
            )
        })?;
    Ok(base.join("codex-proxy").join("auth.json"))
}

/// SIGINT or SIGTERM (the k8s stop signal).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl-c handler");
    };
    #[cfg(unix)]
    {
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
    }
    #[cfg(not(unix))]
    ctrl_c.await;
    tracing::info!("shutdown signal received");
}
