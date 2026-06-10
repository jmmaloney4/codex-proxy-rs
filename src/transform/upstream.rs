use serde::Deserialize;
use serde_json::Value;

/// Minimal envelope extracted before variant dispatch.
/// We do NOT use `#[serde(tag = "type")]` because reasoning events
/// require prefix matching on the type string (e.g. `response.reasoning*`).
#[derive(Debug, Deserialize)]
pub struct EventEnvelope {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub sequence_number: Option<u64>,
    // Keep the full payload for events that need targeted extraction
    // rather than forcing every field into a rigid struct.
    #[serde(flatten)]
    pub extra: Value,
}

/// Typed payload for `response.created`.
#[derive(Debug, Deserialize)]
pub struct CreatedPayload {
    pub response: CreatedResponse,
}

#[derive(Debug, Deserialize)]
pub struct CreatedResponse {
    pub id: String,
}

/// Typed payload for `response.output_text.delta`.
#[derive(Debug, Deserialize)]
pub struct OutputTextDeltaPayload {
    #[serde(default)]
    pub delta: String,
}

/// Typed payload for `response.completed`.
#[derive(Debug, Deserialize)]
pub struct CompletedPayload {
    #[serde(default)]
    pub response: CompletedResponse,
}

#[derive(Debug, Default, Deserialize)]
pub struct CompletedResponse {
    #[serde(default)]
    pub usage: Option<UpstreamUsage>,
}

/// Upstream usage payload.
/// Codex uses `input_tokens`/`output_tokens`; OpenAI uses
/// `prompt_tokens`/`completion_tokens`. We accept both.
#[derive(Debug, Deserialize)]
pub struct UpstreamUsage {
    #[serde(default)]
    pub prompt_tokens: Option<i64>,
    #[serde(default)]
    pub completion_tokens: Option<i64>,
    #[serde(default)]
    pub input_tokens: Option<i64>,
    #[serde(default)]
    pub output_tokens: Option<i64>,
}

impl UpstreamUsage {
    pub fn to_openai(&self) -> (i64, i64) {
        let pt = self.prompt_tokens.or(self.input_tokens).unwrap_or(0);
        let ct = self.completion_tokens.or(self.output_tokens).unwrap_or(0);
        (pt, ct)
    }
}

/// Typed payload for `response.output_item.added` (function-call start).
#[derive(Debug, Deserialize)]
pub struct OutputItemAddedPayload {
    #[serde(default)]
    pub item: Option<FunctionCallItem>,
}

#[derive(Debug, Deserialize)]
pub struct FunctionCallItem {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub call_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(rename = "type", default)]
    pub item_type: String,
}

/// Typed payload for `response.function_call_arguments.delta`.
#[derive(Debug, Deserialize)]
pub struct FunctionCallArgsDeltaPayload {
    #[serde(default)]
    pub item_id: String,
    #[serde(default)]
    pub delta: String,
}

/// Reasoning event extraction helpers.
/// Reasoning payloads are too variant for a single struct; we extract
/// the text content via a cascade matching the Go `extractReasoningContent`.
impl EventEnvelope {
    pub fn extract_reasoning_content(&self) -> Option<String> {
        let extra = &self.extra;
        // 1. Direct "delta" field
        if let Some(delta) = extra.get("delta").and_then(Value::as_str)
            && !delta.is_empty()
        {
            return Some(delta.to_string());
        }
        // 2. Direct "text" field
        if let Some(text) = extra.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            return Some(text.to_string());
        }
        // 3. Nested "part.text"
        if let Some(part) = extra.get("part").and_then(Value::as_object)
            && let Some(t) = part.get("text").and_then(Value::as_str)
            && !t.is_empty()
        {
            return Some(t.to_string());
        }
        // 4. Nested "item.encrypted_content" → skip entirely
        if let Some(item) = extra.get("item").and_then(Value::as_object) {
            if item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some()
            {
                return None;
            }
            // 4b. item.summary[].text
            if let Some(text) = extract_summary_text(item.get("summary")) {
                return Some(text);
            }
        }
        // 5. Top-level "summary[].text"
        if let Some(text) = extract_summary_text(extra.get("summary")) {
            return Some(text);
        }
        None
    }

    pub fn output_index(&self) -> Option<u64> {
        self.extra.get("output_index").and_then(Value::as_u64)
    }
}

fn extract_summary_text(summary: Option<&Value>) -> Option<String> {
    let arr = summary?.as_array()?;
    for entry in arr {
        if let Some(sm) = entry.as_object()
            && let Some(t) = sm.get("text").and_then(Value::as_str)
            && !t.is_empty()
        {
            return Some(t.to_string());
        }
    }
    None
}
