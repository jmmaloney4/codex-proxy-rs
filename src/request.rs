//! Request-side transformation: rewrite incoming OpenAI Chat Completions and
//! Responses API requests into the ChatGPT Codex backend body.
//!
//! This is a faithful port of the *live* request functions in Go
//! `internal/server/transform.go` and `transform_responses.go`:
//! `buildCodexRequestBody` (chat completions) and `transformResponsesRequestBody`
//! (responses), plus the helpers they reach. The Go `transformSystemPrompt`,
//! `transformMessages`, and `extractUserText` functions are intentionally NOT
//! ported — they have no callers in the Go source (dead code).
//!
//! Like the Go code, these operate on loosely-typed JSON (`serde_json::Value`,
//! mirroring Go's `map[string]interface{}`). `serde_json::Map` is sorted by key,
//! matching Go's `encoding/json` map marshalling, so serialized bodies stay
//! byte-comparable.
//!
//! ## Known byte-parity divergences
//!
//! Go's `encoding/json` HTML-escapes `<`, `>`, `&` in string values as
//! `\u003c`, `\u003e`, `\u0026`. `serde_json` does not. This is a cosmetic
//! difference — both are valid JSON per RFC 8259 — but means serialized output
//! is not byte-identical to Go for strings containing these characters. Downstream
//! consumers parse both forms identically.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::{model, prompts};

/// Names rewritten to "Codex" anywhere they appear in prompt/message text.
/// Order and duplicates match Go `namesToReplace` exactly.
const NAMES_TO_REPLACE: &[&str] = &[
    "Zed",
    "Cline",
    "Roo",
    "GitHub Copilot",
    "Copilot",
    "Cursor",
    "Microsoft",
    "Copilot",
];

/// Replace every known third-party agent/vendor name with "Codex".
/// Port of Go `replaceNames`.
pub fn replace_names(input: &str) -> String {
    let mut out = input.to_string();
    for name in NAMES_TO_REPLACE {
        out = out.replace(name, "Codex");
    }
    out
}

/// Port of Go `resolveRequestModel`: the request `model`, trimmed, or `gpt-5`.
pub(crate) fn resolve_request_model(request: &Value) -> String {
    request
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map_or_else(|| model::GPT5.to_string(), str::to_string)
}

/// Port of Go `resolveReasoningEffort`. Resolution order: explicit
/// `reasoning_effort`, then `reasoning.effort`, then a `-<effort>` suffix on the
/// model name. Empty string when none apply.
pub(crate) fn resolve_reasoning_effort(request: &Value) -> String {
    if let Some(effort) = request.get("reasoning_effort").and_then(Value::as_str) {
        let effort = effort.trim();
        if !effort.is_empty() {
            return effort.to_string();
        }
    }
    if let Some(effort) = request
        .get("reasoning")
        .and_then(Value::as_object)
        .and_then(|r| r.get("effort"))
        .and_then(Value::as_str)
    {
        let effort = effort.trim();
        if !effort.is_empty() {
            return effort.to_string();
        }
    }
    if let Some(model_str) = request.get("model").and_then(Value::as_str) {
        let lower = model_str.trim().to_lowercase();
        for effort in ["xhigh", "high", "medium", "low", "minimal"] {
            if lower.ends_with(&format!("-{effort}")) {
                return effort.to_string();
            }
        }
    }
    String::new()
}

/// Port of Go `resolveReasoningSummary`. Returns the explicit
/// `reasoning.summary` value, or `"auto"` when absent. An explicit JSON `null`
/// summary resolves to `None` (Go's `nil`), which the callers then omit.
fn resolve_reasoning_summary(request: &Value) -> Option<Value> {
    if let Some(reasoning) = request.get("reasoning").and_then(Value::as_object)
        && let Some(summary) = reasoning.get("summary")
    {
        return if summary.is_null() {
            None
        } else {
            Some(summary.clone())
        };
    }
    Some(Value::String("auto".to_string()))
}

/// Port of Go `buildReasoningSettings`: `{effort?, summary?}` with the effort
/// clamped to the backend model's allowed set.
fn build_reasoning_settings(request: &Value) -> Value {
    let requested = resolve_reasoning_effort(request);
    let normalized = model::normalize_reasoning_effort(&requested);
    let backend = model::normalize_model(&resolve_request_model(request));
    let clamped = model::clamp_reasoning_effort_for_model(normalized, backend);
    let summary = resolve_reasoning_summary(request);

    let mut settings = Map::new();
    if !clamped.is_empty() {
        settings.insert("effort".to_string(), Value::String(clamped));
    }
    if let Some(summary) = summary {
        settings.insert("summary".to_string(), summary);
    }
    Value::Object(settings)
}

/// Port of Go `derivePromptCacheKey` + `formatUUID`: SHA-256 of
/// `model\ninstructions\nfirstUserText` (each trimmed) rendered as a
/// version-5/RFC-4122 UUID. Empty string when all three inputs are empty.
pub fn derive_prompt_cache_key(model: &str, instructions: &str, first_user_text: &str) -> String {
    let model = model.trim();
    let instructions = instructions.trim();
    let first_user_text = first_user_text.trim();
    if model.is_empty() && instructions.is_empty() && first_user_text.is_empty() {
        return String::new();
    }
    let payload = format!("{model}\n{instructions}\n{first_user_text}");
    let digest = Sha256::digest(payload.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // `from_sha1_bytes` sets version 5 + RFC-4122 variant, identical to the Go
    // bit-twiddling (`b[6] = (b[6] & 0x0f) | 0x50; b[8] = (b[8] & 0x3f) | 0x80`).
    uuid::Builder::from_sha1_bytes(bytes)
        .into_uuid()
        .to_string()
}

/// Port of Go `extractInstructions`: concatenate all `system`-role message text
/// (names replaced), joining content segments with `\n` and messages with
/// `\n\n`.
fn extract_instructions(request: &Value) -> String {
    let Some(msgs) = request.get("messages").and_then(Value::as_array) else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for msg in msgs {
        let Some(mm) = msg.as_object() else { continue };
        if mm.get("role").and_then(Value::as_str) != Some("system") {
            continue;
        }
        match mm.get("content") {
            Some(Value::String(text)) => {
                if !text.is_empty() {
                    parts.push(replace_names(text));
                }
            }
            Some(Value::Array(items)) => {
                let mut segs: Vec<String> = Vec::new();
                for item in items {
                    if let Some(im) = item.as_object()
                        && let Some(text) = im.get("text").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        segs.push(replace_names(text));
                    }
                }
                if !segs.is_empty() {
                    parts.push(segs.join("\n"));
                }
            }
            _ => {}
        }
    }
    parts.join("\n\n").trim().to_string()
}

/// Port of Go `extractFirstUserText`: first non-empty `user` text from the
/// codex `input` array, falling back to chat-completions `messages`. Names
/// replaced.
fn extract_first_user_text(body: &Value) -> String {
    if let Some(input) = body.get("input").and_then(Value::as_array) {
        for entry in input {
            let Some(em) = entry.as_object() else {
                continue;
            };
            if em.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            if let Some(content) = em.get("content").and_then(Value::as_array) {
                for item in content {
                    if let Some(im) = item.as_object()
                        && let Some(text) = im.get("text").and_then(Value::as_str)
                        && !text.trim().is_empty()
                    {
                        return replace_names(text);
                    }
                }
            }
        }
    }

    if let Some(msgs) = body.get("messages").and_then(Value::as_array) {
        for msg in msgs {
            let Some(mm) = msg.as_object() else { continue };
            if mm.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            match mm.get("content") {
                Some(Value::String(text)) => {
                    if !text.trim().is_empty() {
                        return replace_names(text);
                    }
                }
                Some(Value::Array(items)) => {
                    for item in items {
                        if let Some(im) = item.as_object()
                            && let Some(text) = im.get("text").and_then(Value::as_str)
                            && !text.trim().is_empty()
                        {
                            return replace_names(text);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    String::new()
}

/// Port of Go `collectTextSegments`. A string yields one trimmed segment; an
/// array yields each item's non-empty (untrimmed) `text`. Names replaced when
/// `apply_replace`.
fn collect_text_segments(content: Option<&Value>, apply_replace: bool) -> Vec<String> {
    let maybe_replace = |t: &str| {
        if apply_replace {
            replace_names(t)
        } else {
            t.to_string()
        }
    };
    match content {
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.is_empty() {
                Vec::new()
            } else {
                vec![maybe_replace(text)]
            }
        }
        Some(Value::Array(items)) => {
            let mut texts = Vec::new();
            for item in items {
                let Some(im) = item.as_object() else { continue };
                let text = im.get("text").and_then(Value::as_str).unwrap_or("");
                if text.is_empty() {
                    continue;
                }
                texts.push(maybe_replace(text));
            }
            texts
        }
        _ => Vec::new(),
    }
}

/// Port of Go `extractArgumentsString`: pass strings through, render anything
/// else as compact JSON, empty for null/absent.
fn extract_arguments_string(arg: Option<&Value>) -> String {
    match arg {
        Some(Value::String(v)) => v.clone(),
        None | Some(Value::Null) => String::new(),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Port of Go `collectToolOutput`: strings pass through; arrays join non-empty
/// `text` with `\n`; anything else renders as compact JSON.
fn collect_tool_output(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(v)) => v.clone(),
        Some(Value::Array(items)) => {
            let mut parts = Vec::new();
            for item in items {
                if let Some(im) = item.as_object()
                    && let Some(text) = im.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    parts.push(text.to_string());
                }
            }
            parts.join("\n")
        }
        None | Some(Value::Null) => String::new(),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Port of Go `mapToolsToCodex`: map OpenAI `type:function` tools to Codex tool
/// objects. Returns JSON `null` when there is no `tools` array (matching Go's
/// nil-slice marshalling); otherwise an array (possibly empty).
fn map_tools_to_codex(request: &Value) -> Value {
    let Some(tools) = request.get("tools").and_then(Value::as_array) else {
        return Value::Null;
    };
    let mut out = Vec::with_capacity(tools.len());
    for tool in tools {
        let Some(tm) = tool.as_object() else { continue };
        if tm.get("type").and_then(Value::as_str) != Some("function") {
            continue;
        }
        let Some(func) = tm.get("function").and_then(Value::as_object) else {
            continue;
        };
        let name = func.get("name").and_then(Value::as_str).unwrap_or("");
        let desc = func
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let params = func.get("parameters").cloned().unwrap_or(Value::Null);
        out.push(json!({
            "type": "function",
            "name": name,
            "description": desc,
            "strict": false,
            "parameters": params,
        }));
    }
    Value::Array(out)
}

/// Port of Go `buildCodexInputMessages`: convert OpenAI `messages` into the
/// Codex `input` array, leading with a `developer` message carrying the
/// concatenated system instructions.
fn build_codex_input_messages(request: &Value) -> Vec<Value> {
    let system_prompt = extract_instructions(request);
    let mut input: Vec<Value> = vec![json!({
        "type": "message",
        "id": null,
        "role": "developer",
        "content": [{ "type": "input_text", "text": system_prompt }],
    })];

    let Some(msgs) = request.get("messages").and_then(Value::as_array) else {
        return input;
    };

    for msg in msgs {
        let Some(mm) = msg.as_object() else { continue };
        let role = mm.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "user" => {
                let texts = collect_text_segments(mm.get("content"), true);
                if texts.is_empty() {
                    continue;
                }
                let contents: Vec<Value> = texts
                    .into_iter()
                    .map(|t| json!({ "type": "input_text", "text": t }))
                    .collect();
                input.push(json!({
                    "type": "message",
                    "id": mm.get("id").cloned().unwrap_or(Value::Null),
                    "role": "user",
                    "content": contents,
                }));
            }
            "assistant" => {
                let texts = collect_text_segments(mm.get("content"), true);
                if !texts.is_empty() {
                    let contents: Vec<Value> = texts
                        .into_iter()
                        .map(|t| json!({ "type": "output_text", "text": t }))
                        .collect();
                    input.push(json!({
                        "type": "message",
                        "id": mm.get("id").cloned().unwrap_or(Value::Null),
                        "role": "assistant",
                        "content": contents,
                    }));
                }
                if let Some(tool_calls) = mm.get("tool_calls").and_then(Value::as_array) {
                    for tc in tool_calls {
                        let Some(tcm) = tc.as_object() else { continue };
                        let call_id = tcm.get("id").and_then(Value::as_str).unwrap_or("");
                        let func = tcm.get("function").and_then(Value::as_object);
                        let name = func
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let arguments =
                            extract_arguments_string(func.and_then(|f| f.get("arguments")));
                        input.push(json!({
                            "type": "function_call",
                            "name": name,
                            "call_id": call_id,
                            "arguments": arguments,
                        }));
                    }
                }
            }
            "tool" => {
                let call_id = mm.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
                if call_id.is_empty() {
                    continue;
                }
                let output = collect_tool_output(mm.get("content"));
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                }));
            }
            _ => {}
        }
    }
    input
}

/// Port of Go `buildCodexRequestBody`: transform an OpenAI Chat Completions
/// request into the ChatGPT Codex backend body.
pub fn build_codex_request_body(request: &Value) -> Value {
    let prefix = prompts::CODEX_INSTRUCTIONS_PREFIX;
    let resolved_model = resolve_request_model(request);
    let normalized_model = model::normalize_model(&resolved_model);

    let mut body = Map::new();
    body.insert("model".to_string(), json!(normalized_model));
    body.insert("instructions".to_string(), json!(prefix));
    body.insert("store".to_string(), json!(false));
    body.insert("stream".to_string(), json!(true));

    // Prepend the override "developer" greeting to the codex input messages.
    let initial_greeting = json!({
        "type": "message",
        "id": null,
        "role": "developer",
        "content": [{ "type": "input_text", "text": prompts::INVERSE_PROMPT }],
    });
    let input_msgs = build_codex_input_messages(request);
    if !input_msgs.is_empty() {
        let mut full = Vec::with_capacity(input_msgs.len() + 1);
        full.push(initial_greeting);
        full.extend(input_msgs);
        body.insert("input".to_string(), Value::Array(full));
    }

    // Tools (always present; JSON null when the request had no tools).
    body.insert("tools".to_string(), map_tools_to_codex(request));

    // Like Go, only the string form of `tool_choice` is honored; the object
    // form (forcing a named function) falls back to "auto".
    let tool_choice = request
        .get("tool_choice")
        .and_then(Value::as_str)
        .filter(|tc| !tc.is_empty())
        .unwrap_or("auto");
    body.insert("tool_choice".to_string(), json!(tool_choice));

    let parallel = request
        .get("parallel_tool_calls")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    body.insert("parallel_tool_calls".to_string(), json!(parallel));

    body.insert("reasoning".to_string(), build_reasoning_settings(request));
    body.insert(
        "include".to_string(),
        json!(["reasoning.encrypted_content"]),
    );

    // prompt_cache_key: derived only if not already set (it never is, but mirror
    // the Go guard). `extract_first_user_text` reads the codex `input` we built.
    if body
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .is_none()
    {
        let first = extract_first_user_text(&Value::Object(body.clone()));
        let key = derive_prompt_cache_key(normalized_model, prefix, &first);
        if !key.is_empty() {
            body.insert("prompt_cache_key".to_string(), json!(key));
        }
    }

    Value::Object(body)
}

/// Port of Go `transformResponsesRequestBody`: rewrite a Responses API request
/// body in place. Returns `(normalized_model, clamped_effort)`.
pub fn transform_responses_request_body(
    body: &mut Value,
    requested_model: &str,
    requested_effort: &str,
) -> (String, String) {
    let Some(obj) = body.as_object_mut() else {
        return (String::new(), String::new());
    };

    let normalized_model = model::normalize_model(requested_model).to_string();
    obj.insert("model".to_string(), json!(normalized_model));
    obj.insert("store".to_string(), json!(false));

    // Pull any top-level `instructions` aside; it is re-applied below.
    let mut user_instr = String::new();
    if let Some(existing) = obj.get("instructions").and_then(Value::as_str) {
        user_instr = existing.trim().to_string();
    }
    obj.remove("instructions");

    // Split `system`-role messages out of `input` into `system_text`. Like Go,
    // each system message overwrites `system_text` (last one wins) and only
    // array-shaped `content` is read — string content is dropped.
    let mut system_text = String::new();
    let mut all_instructions: Vec<Value> = Vec::new();
    if let Some(existing_input) = obj.get("input").and_then(Value::as_array) {
        for msg in existing_input {
            let Some(mm) = msg.as_object() else {
                all_instructions.push(msg.clone());
                continue;
            };
            if mm.get("role").and_then(Value::as_str) == Some("system") {
                let mut parts: Vec<String> = Vec::new();
                if let Some(contents) = mm.get("content").and_then(Value::as_array) {
                    for item in contents {
                        if let Some(im) = item.as_object()
                            && let Some(text) = im.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            parts.push(text.to_string());
                        }
                    }
                }
                system_text = parts.join("\n\n");
            } else {
                all_instructions.push(msg.clone());
            }
        }
    }

    // Re-apply instructions per Go's precedence; system text may become a
    // leading `developer` message when an explicit instruction also exists.
    if !user_instr.is_empty() && !system_text.is_empty() {
        obj.insert("instructions".to_string(), json!(user_instr));
        let developer_msg = json!({ "role": "developer", "content": replace_names(&system_text) });
        all_instructions.insert(0, developer_msg);
    } else if !user_instr.is_empty() {
        obj.insert("instructions".to_string(), json!(user_instr));
    } else if !system_text.is_empty() {
        obj.insert(
            "instructions".to_string(),
            json!(replace_names(&system_text)),
        );
    } else {
        obj.insert("instructions".to_string(), json!(""));
    }

    // Go's `allInstructions` is a nil slice that only becomes non-nil via
    // `append`, so an empty result marshals as JSON null — even when the body
    // *had* an `input` key (e.g. `[]`, or only system messages with no
    // developer prepend). Verified empirically against the Go package.
    let input_value = if all_instructions.is_empty() {
        Value::Null
    } else {
        Value::Array(all_instructions)
    };
    obj.insert("input".to_string(), input_value);
    sanitize_responses_input(obj);

    obj.insert(
        "include".to_string(),
        json!(["reasoning.encrypted_content"]),
    );

    if !obj.contains_key("tool_choice") {
        obj.insert("tool_choice".to_string(), json!("auto"));
    }
    if !obj.contains_key("parallel_tool_calls") {
        obj.insert("parallel_tool_calls".to_string(), json!(false));
    }

    obj.remove("max_output_tokens");
    obj.remove("max_tokens");

    let normalized_effort = model::normalize_reasoning_effort(requested_effort);
    let clamped_effort =
        model::clamp_reasoning_effort_for_model(normalized_effort, &normalized_model);
    let summary = resolve_reasoning_summary(&Value::Object(obj.clone()));
    let mut reasoning_settings = Map::new();
    if let Some(summary) = summary {
        reasoning_settings.insert("summary".to_string(), summary);
    }
    if !clamped_effort.is_empty() {
        reasoning_settings.insert("effort".to_string(), json!(clamped_effort));
    }
    if reasoning_settings.is_empty() {
        obj.remove("reasoning");
    } else {
        obj.insert("reasoning".to_string(), Value::Object(reasoning_settings));
    }

    obj.remove("reasoning_effort");

    if obj
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .is_none()
    {
        let instructions = obj
            .get("instructions")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let first_text = extract_first_user_text(&Value::Object(obj.clone()));
        let key = derive_prompt_cache_key(&normalized_model, &instructions, &first_text);
        if !key.is_empty() {
            obj.insert("prompt_cache_key".to_string(), json!(key));
        }
    }

    (normalized_model, clamped_effort)
}

/// Port of Go `sanitizeResponsesInput`: drop `system`-role messages from
/// `input` and run `replace_names` over remaining message text in place.
fn sanitize_responses_input(obj: &mut Map<String, Value>) {
    let Some(input) = obj.get("input").and_then(Value::as_array) else {
        return;
    };
    let mut filtered: Vec<Value> = Vec::with_capacity(input.len());
    for msg in input {
        let Some(mm) = msg.as_object() else {
            filtered.push(msg.clone());
            continue;
        };
        if mm.get("role").and_then(Value::as_str) == Some("system") {
            continue;
        }
        if mm.get("content").and_then(Value::as_array).is_none() {
            filtered.push(msg.clone());
            continue;
        }
        let mut new_msg = mm.clone();
        if let Some(contents) = new_msg.get_mut("content").and_then(Value::as_array_mut) {
            for item in contents.iter_mut() {
                if let Some(im) = item.as_object_mut()
                    && let Some(text) = im.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    let replaced = replace_names(text);
                    im.insert("text".to_string(), Value::String(replaced));
                }
            }
        }
        filtered.push(Value::Object(new_msg));
    }
    obj.insert("input".to_string(), Value::Array(filtered));
}
