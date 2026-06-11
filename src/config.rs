//! CLI + environment configuration (clap; figment is pinned-but-deferred per
//! issue #3) and tracing setup matching Go `internal/logger`.
//!
//! Legacy env names (`PORT`, `ADMIN_API_KEY`, `ANTHROPIC_API_KEY`,
//! `CLAUDE_USER_ID`, `ENV`) are honored for drop-in parity with the Go
//! deployment; new variables are `CODEX_PROXY_`-prefixed.

use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CredsStore {
    /// Static token from ANTHROPIC_API_KEY / CLAUDE_USER_ID.
    Env,
}

#[derive(Debug, Parser)]
#[command(name = "codex-proxy", version, about)]
pub struct Config {
    /// Listen port.
    #[arg(long, env = "PORT", default_value_t = 9879)]
    pub port: u16,

    /// API key required on the data-plane and /admin routes.
    #[arg(long, env = "ADMIN_API_KEY", hide_env_values = true)]
    pub admin_api_key: Option<String>,

    /// Log mode: "development"/"dev"/"" → pretty console, else JSON.
    #[arg(long, env = "ENV", default_value = "development")]
    pub env: String,

    /// Credential store mode.
    #[arg(long = "creds-store", env = "CODEX_PROXY_CREDS_STORE", value_enum, default_value_t = CredsStore::Env)]
    pub creds_store: CredsStore,

    /// SSE keepalive interval in seconds (0 disables).
    #[arg(long, env = "CODEX_PROXY_KEEPALIVE_SECS", default_value_t = 15)]
    pub keepalive_secs: u64,

    /// Static bearer token for the env credential store (legacy name).
    #[arg(
        long,
        env = "ANTHROPIC_API_KEY",
        hide_env_values = true,
        default_value = ""
    )]
    pub anthropic_api_key: String,

    /// Account ID for the env credential store (legacy name).
    #[arg(long, env = "CLAUDE_USER_ID", default_value = "")]
    pub claude_user_id: String,
}

/// Go logger parity: ENV of ""/"dev"/"development" → pretty console output,
/// anything else → JSON to stderr.
pub fn init_tracing(env: &str) {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let dev = matches!(env, "" | "dev" | "development");
    if dev {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    }
}
