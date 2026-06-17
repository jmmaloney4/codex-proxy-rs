//! Conversation identity (ADR 006 §2).
//!
//! Resolves a stable key for a chat-completions conversation. Later phases use
//! it to pin account affinity and persisted reasoning (ADR 006 §1, §3a); for
//! now nothing consumes it beyond observability (`crate::server::chat` emits it
//! as a tracing field). This module only *resolves* a key — it holds no state
//! and has no side effects.
//!
//! Resolution order:
//! 1. An explicit client key: the `x-conversation-id` header. The OpenAI `user`
//!    field is deliberately **not** used — it identifies an end *user*, not a
//!    conversation, so keying on it would merge a user's separate chats under
//!    one identity (and later, one reasoning history). This refines ADR 006 §2,
//!    which listed `user` as a candidate.
//! 2. A model-independent hash of the conversation *head* (system instructions
//!    + first user message), with length-prefixed field boundaries so embedded
//!    newlines cannot make distinct heads collide. Model is deliberately
//!    excluded so the key survives a mid-conversation model switch — which
//!    invalidates only the persisted reasoning (account × model scoping, ADR
//!    006 §1), not the conversation's identity.
//!
//! The tool-call-ID token (ADR 006 §2.2) is deferred to a later PR.

use axum::http::HeaderMap;
use serde_json::Value;

use crate::request::{extract_first_user_text, extract_instructions, hash_to_uuid};

const HEADER_NAME: &str = "x-conversation-id";

/// Upper bound on an accepted explicit key. It becomes a store key in later
/// phases, so reject absurdly long client-supplied values rather than trust them.
const MAX_EXPLICIT_KEY_LEN: usize = 256;

/// An explicit `x-conversation-id` is usable only if non-empty, bounded, and
/// free of control characters; otherwise we fall through to the head-hash.
fn valid_explicit_key(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_EXPLICIT_KEY_LEN && !s.chars().any(char::is_control)
}

/// How a [`ConversationKey`] was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// Client-supplied via the `x-conversation-id` header.
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
    // 1. Explicit `x-conversation-id` header (the only client-supplied key — see
    //    module docs on why `user` is excluded).
    if let Some(v) = headers.get(HEADER_NAME).and_then(|h| h.to_str().ok()) {
        let v = v.trim();
        if valid_explicit_key(v) {
            return Some(ConversationKey {
                key: v.to_string(),
                source: KeySource::Explicit,
            });
        }
    }
    // 2. Head-hash: instructions + first user text, model excluded. Fields are
    //    length-prefixed so embedded newlines can't make distinct heads collide.
    let instructions = extract_instructions(request);
    let instructions = instructions.trim();
    let first_user = extract_first_user_text(request);
    let first_user = first_user.trim();
    if instructions.is_empty() && first_user.is_empty() {
        return None;
    }
    let payload = format!(
        "instructions:{}:{instructions}\nfirst_user:{}:{first_user}",
        instructions.len(),
        first_user.len(),
    );
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
    fn explicit_header_is_used_verbatim() {
        let k = resolve_conversation_key(
            &headers(&[("x-conversation-id", "  header-id  ")]),
            &req("gpt-5.4", "sys", "hi"),
        )
        .expect("resolves");
        assert_eq!(k.key, "header-id"); // trimmed
        assert_eq!(k.source, KeySource::Explicit);
    }

    #[test]
    fn user_field_is_ignored_falls_through_to_head_hash() {
        // `user` is a per-end-user id, not a conversation id — it must not key
        // the conversation (would merge separate chats). Falls through to head.
        let mut r = req("gpt-5.4", "sys", "hi");
        r["user"] = json!("user-field-id");
        let k = resolve_conversation_key(&HeaderMap::new(), &r).expect("resolves");
        assert_eq!(k.source, KeySource::HeadHash);
        assert_ne!(k.key, "user-field-id");
    }

    #[test]
    fn head_hash_resists_field_boundary_collisions() {
        // Length-prefixing means moving a newline across the field boundary
        // yields distinct keys (naive "a\nb" concat would collide).
        let a = resolve_conversation_key(&HeaderMap::new(), &req("gpt-5.4", "x", "y\nz"))
            .expect("resolves");
        let b = resolve_conversation_key(&HeaderMap::new(), &req("gpt-5.4", "x\ny", "z"))
            .expect("resolves");
        assert_ne!(a.key, b.key);
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
    fn oversized_explicit_key_falls_through_to_head_hash() {
        let big = "x".repeat(MAX_EXPLICIT_KEY_LEN + 1);
        let mut h = HeaderMap::new();
        h.insert(
            HeaderName::from_static("x-conversation-id"),
            HeaderValue::from_str(&big).unwrap(),
        );
        let k = resolve_conversation_key(&h, &req("gpt-5.4", "sys", "hi")).expect("resolves");
        assert_eq!(k.source, KeySource::HeadHash);
    }

    #[test]
    fn blank_header_falls_through_to_head_hash() {
        let k = resolve_conversation_key(
            &headers(&[("x-conversation-id", "  ")]),
            &req("gpt-5.4", "sys", "hi"),
        )
        .expect("resolves");
        assert_eq!(k.source, KeySource::HeadHash);
    }
}
