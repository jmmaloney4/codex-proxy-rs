use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use codex_proxy_rs::affinity::{AffinityStore, RedisAffinityStore};
use codex_proxy_rs::config::{Config, CredsStore, ProxyMode, init_tracing};
use codex_proxy_rs::credentials::{CredentialsFetcher, EnvCredentials, FsAuthFile, OAuthFetcher};
use codex_proxy_rs::relay::RelayConfig;
use codex_proxy_rs::router::AccountPool;
use codex_proxy_rs::server::{AppState, router};
use codex_proxy_rs::upstream::{UPSTREAM_URL, build_upstream_client};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    // `Some` only when OTLP export is configured; held for an explicit flush on
    // shutdown so the batch processor isn't dropped mid-export (ADR 005 §8).
    let tracer_provider = init_tracing(&config.env);

    let http = build_upstream_client();
    // Router mode never calls the codex backend (the backend pods do), so it
    // needs no credential store — and must not spawn the fs OAuth refresh
    // loop. Use empty static creds there; only backend mode builds a real store.
    let creds: Arc<dyn CredentialsFetcher> = match config.mode {
        ProxyMode::Router => Arc::new(EnvCredentials::new(String::new(), String::new())),
        ProxyMode::Backend => match config.creds_store {
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
        },
    };

    // Backend-mode startup validation, warn-only like Go. Router mode does not
    // use credentials (the backend pods own those), so skip it there.
    if config.mode == ProxyMode::Backend {
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
    }

    // Router mode: build the account pool and (best-effort) affinity store.
    let (accounts, affinity): (Option<Arc<AccountPool>>, Option<Arc<dyn AffinityStore>>) =
        match config.mode {
            ProxyMode::Backend => (None, None),
            ProxyMode::Router => {
                let pool = AccountPool::parse(&config.codex_accounts)?;
                tracing::info!(accounts = pool.len(), "router mode: account pool loaded");
                let affinity: Option<Arc<dyn AffinityStore>> = match config.redis_url.as_deref() {
                    Some(url) => {
                        match RedisAffinityStore::connect(url, config.affinity_ttl_secs).await {
                            Ok(store) => {
                                tracing::info!("router mode: affinity store connected");
                                Some(Arc::new(store))
                            }
                            // Best-effort (ADR 006 §5c): start anyway, statelessly.
                            Err(err) => {
                                tracing::warn!(error = %err, "router mode: affinity store unavailable; routing statelessly");
                                None
                            }
                        }
                    }
                    None => {
                        tracing::warn!(
                            "router mode: no CODEX_PROXY_REDIS_URL; routing statelessly"
                        );
                        None
                    }
                };
                (Some(Arc::new(pool)), affinity)
            }
        };

    // Stable, non-secret account alias for the subscription metrics' `account`
    // label (ADR 008). Blank (unset env) → "unknown" so series are still
    // well-formed; never derive this from a credential.
    let account: Arc<str> = match config.account.trim() {
        "" => Arc::from("unknown"),
        alias => Arc::from(alias),
    };

    let state = AppState {
        mode: config.mode,
        creds,
        http,
        relay: RelayConfig {
            keepalive_interval: Duration::from_secs(config.keepalive_secs),
        },
        upstream_url: UPSTREAM_URL.into(),
        admin_api_key: config.admin_api_key.clone().map(Into::into),
        accounts,
        affinity,
        metrics: Arc::new(codex_proxy_rs::metrics::Metrics::new()),
        account,
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
