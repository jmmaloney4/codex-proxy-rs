//! Conversation→account affinity store (ADR 006 §4 / ADR 007).
//!
//! Maps a conversation key to the codex account it is pinned to, so every turn
//! of a conversation reaches the same account (a prerequisite for reasoning
//! persistence — the Codex `encrypted_content` blob is account×model-scoped).
//!
//! The store is **best-effort**: every operation swallows backend errors and a
//! lookup miss is indistinguishable from "unpinned". A Redis outage therefore
//! degrades to no-affinity (each turn re-picks an account) — never a failed
//! request (ADR 006 §5c).

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// What a conversation is pinned to. `model` is recorded for the reasoning
/// phase (PR3) — the blob is invalid across a model change — and is unused by
/// affinity routing itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pin {
    pub slug: String,
    pub model: String,
}

/// A conversation→account pin store. Implementations must be cheap to clone or
/// be held behind an `Arc`. All methods are best-effort and infallible by
/// contract — they log and swallow backend errors.
#[async_trait::async_trait]
pub trait AffinityStore: Send + Sync {
    /// The pin for `conversation_key`, or `None` if unpinned / unavailable.
    async fn get(&self, conversation_key: &str) -> Option<Pin>;
    /// Pin `conversation_key`, (re)setting its TTL. Errors are swallowed.
    async fn put(&self, conversation_key: &str, pin: &Pin);
    /// Drop a pin (used when re-pinning after an account fails). Swallowed.
    async fn clear(&self, conversation_key: &str);
}

fn redis_key(conversation_key: &str) -> String {
    format!("conv:{conversation_key}")
}

/// Redis-backed store (production). Keys expire after `ttl_secs` since the last
/// `put` — conversations are ephemeral, so the TTL is the eviction story.
pub struct RedisAffinityStore {
    conn: redis::aio::ConnectionManager,
    ttl_secs: u64,
}

impl RedisAffinityStore {
    /// Connect and build a pooled, auto-reconnecting manager. Fails only on an
    /// unparseable URL or an initial-connection error; callers may treat a
    /// failure as "no affinity store" and fall back to stateless routing.
    pub async fn connect(url: &str, ttl_secs: u64) -> anyhow::Result<Self> {
        let client = redis::Client::open(url)?;
        let conn = redis::aio::ConnectionManager::new(client).await?;
        Ok(Self { conn, ttl_secs })
    }
}

#[async_trait::async_trait]
impl AffinityStore for RedisAffinityStore {
    async fn get(&self, conversation_key: &str) -> Option<Pin> {
        let mut conn = self.conn.clone();
        let raw: Option<String> = redis::cmd("GET")
            .arg(redis_key(conversation_key))
            .query_async(&mut conn)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(error = %err, "affinity GET failed; treating as miss");
                None
            });
        raw.and_then(|s| serde_json::from_str(&s).ok())
    }

    async fn put(&self, conversation_key: &str, pin: &Pin) {
        let Ok(value) = serde_json::to_string(pin) else {
            return;
        };
        let mut conn = self.conn.clone();
        let res: redis::RedisResult<()> = redis::cmd("SET")
            .arg(redis_key(conversation_key))
            .arg(value)
            .arg("EX")
            .arg(self.ttl_secs)
            .query_async(&mut conn)
            .await;
        if let Err(err) = res {
            tracing::warn!(error = %err, "affinity SET failed; pin not persisted");
        }
    }

    async fn clear(&self, conversation_key: &str) {
        let mut conn = self.conn.clone();
        let res: redis::RedisResult<()> = redis::cmd("DEL")
            .arg(redis_key(conversation_key))
            .query_async(&mut conn)
            .await;
        if let Err(err) = res {
            tracing::warn!(error = %err, "affinity DEL failed");
        }
    }
}

/// In-memory store — used by tests and usable as a single-replica fallback. Not
/// the production choice (a router restart loses all pins; no cross-replica
/// sharing), which is why prod uses [`RedisAffinityStore`].
#[derive(Default)]
pub struct InMemoryAffinityStore {
    map: Mutex<HashMap<String, Pin>>,
}

#[async_trait::async_trait]
impl AffinityStore for InMemoryAffinityStore {
    async fn get(&self, conversation_key: &str) -> Option<Pin> {
        self.map.lock().unwrap().get(conversation_key).cloned()
    }

    async fn put(&self, conversation_key: &str, pin: &Pin) {
        self.map
            .lock()
            .unwrap()
            .insert(conversation_key.to_string(), pin.clone());
    }

    async fn clear(&self, conversation_key: &str) {
        self.map.lock().unwrap().remove(conversation_key);
    }
}
