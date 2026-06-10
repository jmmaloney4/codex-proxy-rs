//! Static system-prompt constants, ported verbatim from the Go `codex-proxy`
//! (`internal/server/transform.go`).
//!
//! Both strings are embedded via `include_str!` from byte-exact extracts of the
//! Go source so the request body sent upstream — and the `prompt_cache_key`
//! derived from it — stays identical to the Go implementation. Editing these
//! changes upstream behaviour; keep them in sync with the Go source.

/// The Codex CLI agent system prompt, sent as the request `instructions`.
/// Verbatim copy of Go `codexInstructionsPrefix()`.
pub const CODEX_INSTRUCTIONS_PREFIX: &str = include_str!("prompts/codex_instructions_prefix.txt");

/// The "inverse"/override prompt prepended as the initial `developer` message.
/// Verbatim copy of Go `inversePrompt`.
pub const INVERSE_PROMPT: &str = include_str!("prompts/inverse_prompt.txt");
