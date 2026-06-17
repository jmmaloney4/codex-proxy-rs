//! Conversation identity (ADR 006 §2).
//!
//! Resolves a stable key for a chat-completions conversation. Later phases use
//! it to pin account affinity and persisted reasoning (ADR 006 §1, §3a); for
//! now nothing consumes it beyond observability (`crate::server::chat` emits it
//! as a tracing field). This module only *resolves* a key — it holds no state
//! and has no side effects.
//!
//! Resolution order:
//! 1. An explicit client key: the `x-conversation-id` header, else the
//!    top-level `user` body field.
//! 2. A model-independent hash of the conversation *head* (system instructions
//!    + first user message). Model is deliberately excluded so the key survives
//!    a mid-conversation model switch — which invalidates only the persisted
//!    reasoning (account × model scoping, ADR 006 §1), not the conversation's
//!    identity.
//!
//! The tool-call-ID token (ADR 006 §2.2) is deferred to a later PR.

use axum::http::HeaderMap;
use serde_json::Value;

use crate::request::{extract_first_user_text, extract_instructions, hash_to_uuid};

const HEADER_NAME: &str = "x-conversation-id";

/// How a [`ConversationKey`] was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// Client-supplied via the `x-conversation-id` header or `user` body field.
    Explicit,
    /// Derived by hashing the (model-independent) conversation head.
    HeadHash,
}

impl KeySource {
    /// Stable lowercase label for logs/metrics.
    pub fn as_str(&self) -> &'static str {
        match self {
            KeySource::Explicit => "explicit",
            KeySource::HeadHash => "head_hash",
        }
    }
}

/// A resolved conversation key and how it was obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationKey {
    pub key: String,
    pub source: KeySource,
}

/// Resolve a stable conversation key per ADR 006 §2. Returns `None` only when
/// there is neither an explicit key nor any usable conversation head.
pub fn resolve_conversation_key(headers: &HeaderMap, request: &Value) -> Option<ConversationKey> {
    // 1a. Explicit `x-conversation-id` header.
    if let Some(v) = headers.get(HEADER_NAME).and_then(|h| h.to_str().ok()) {
        let v = v.trim();
        if !v.is_empty() {
            return Some(ConversationKey {
                key: v.to_string(),
                source: KeySource::Explicit,
            });
        }
    }
    // 1b. Explicit `user` body field.
    if let Some(user) = request.get("user").and_then(Value::as_str) {
        let user = user.trim();
        if !user.is_empty() {
            return Some(ConversationKey {
                key: user.to_string(),
                source: KeySource::Explicit,
            });
        }
    }
    // 2. Head-hash: instructions + first user text, model excluded.
    let instructions = extract_instructions(request);
    let first_user = extract_first_user_text(request);
    if instructions.trim().is_empty() && first_user.trim().is_empty() {
        return None;
    }
    let payload = format!("{}\n{}", instructions.trim(), first_user.trim());
    Some(ConversationKey {
        key: hash_to_uuid(&payload),
        source: KeySource::HeadHash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderName, HeaderValue};
    use serde_json::json;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_static(k),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn req(model: &str, system: &str, user: &str) -> serde_json::Value {
        json!({
            "model": model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        })
    }

    #[test]
    fn explicit_header_wins_over_user_field() {
        let mut r = req("gpt-5.4", "sys", "hi");
        r["user"] = json!("user-field-id");
        let k = resolve_conversation_key(&headers(&[("x-conversation-id", "header-id")]), &r)
            .expect("resolves");
        assert_eq!(k.key, "header-id");
        assert_eq!(k.source, KeySource::Explicit);
    }

    #[test]
    fn user_field_used_when_no_header() {
        let mut r = req("gpt-5.4", "sys", "hi");
        r["user"] = json!("  user-field-id  ");
        let k = resolve_conversation_key(&HeaderMap::new(), &r).expect("resolves");
        assert_eq!(k.key, "user-field-id"); // trimmed
        assert_eq!(k.source, KeySource::Explicit);
    }

    #[test]
    fn head_hash_is_deterministic_and_discriminating() {
        let a = resolve_conversation_key(&HeaderMap::new(), &req("gpt-5.4", "sys", "hello"))
            .expect("resolves");
        let a2 = resolve_conversation_key(&HeaderMap::new(), &req("gpt-5.4", "sys", "hello"))
            .expect("resolves");
        let b = resolve_conversation_key(&HeaderMap::new(), &req("gpt-5.4", "sys", "different"))
            .expect("resolves");
        assert_eq!(a.source, KeySource::HeadHash);
        assert_eq!(a.key, a2.key, "same head → same key");
        assert_ne!(a.key, b.key, "different first user → different key");
    }

    #[test]
    fn head_hash_ignores_model() {
        // A mid-conversation model switch must not change the conversation key
        // (it invalidates only persisted reasoning, ADR 006 §1).
        let a = resolve_conversation_key(&HeaderMap::new(), &req("gpt-5.4", "sys", "hello"))
            .expect("resolves");
        let b = resolve_conversation_key(&HeaderMap::new(), &req("gpt-5.2-codex", "sys", "hello"))
            .expect("resolves");
        assert_eq!(a.key, b.key);
    }

    #[test]
    fn none_when_no_head_and_no_explicit_key() {
        let r = json!({ "model": "gpt-5.4", "messages": [] });
        assert!(resolve_conversation_key(&HeaderMap::new(), &r).is_none());
    }

    #[test]
    fn blank_explicit_values_fall_through_to_head_hash() {
        let mut r = req("gpt-5.4", "sys", "hi");
        r["user"] = json!("   ");
        let k = resolve_conversation_key(&headers(&[("x-conversation-id", "  ")]), &r)
            .expect("resolves");
        assert_eq!(k.source, KeySource::HeadHash);
    }
}
