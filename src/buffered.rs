//! Non-streaming chat completions: consume an upstream Codex SSE stream
//! through [`SSETransformer`] and aggregate the emitted
//! `chat.completion.chunk` frames into one classic `chat.completion` object.
//! Port of Go `chat_completions_buffered.go`.
//!
//! ## Intentional divergence from Go (ADR 004)
//!
//! Go aggregates only `role`/`content`/`finish_reason` — its non-streaming
//! response silently drops tool calls and usage, so non-streaming tool use is
//! broken upstream. This port additionally aggregates `delta.tool_calls`
//! (arguments concatenated per tool index) and `usage` (last non-null wins).
//! Both are additive JSON fields; clients that ignore them see Go's exact
//! shape.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncBufRead;

use crate::relay::SseEventReader;
use crate::transform::{SSETransformer, TransformError, TransformResult};

#[derive(Debug, thiserror::Error)]
pub enum BufferError {
    #[error("error scanning SSE stream: {0}")]
    Read(#[from] std::io::Error),
    #[error("failed to transform SSE event: {0}")]
    Transform(#[from] TransformError),
}

#[derive(Debug, Default, Deserialize)]
struct StreamingChunk {
    #[serde(default)]
    id: String,
    #[serde(default)]
    created: i64,
    #[serde(default)]
    model: String,
    #[serde(default)]
    choices: Vec<StreamingChoice>,
    #[serde(default)]
    usage: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct StreamingChoice {
    #[serde(default)]
    delta: StreamingDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct StreamingDelta {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "type")]
    call_type: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Debug, Default, Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

/// Aggregate an upstream Codex SSE stream into a single `chat.completion`
/// JSON value. Mirrors Go `bufferChatCompletionFromSSE` (frames that fail to
/// parse as chunks are skipped, first non-empty id/model/created win, content
/// concatenated, finish_reason last-non-empty, `[DONE]` does not stop the
/// scan) plus the tool_calls/usage aggregation documented above.
pub async fn buffer_chat_completion<R: AsyncBufRead + Unpin>(
    upstream: R,
    model: &str,
) -> Result<Value, BufferError> {
    let mut reader = SseEventReader::new(upstream);
    let mut transformer = SSETransformer::new(model);

    let mut response_id = String::new();
    let mut stream_model = String::new();
    let mut created: i64 = 0;
    let mut role = String::new();
    let mut content = String::new();
    let mut finish_reason = String::new();
    let mut tool_calls: BTreeMap<usize, ToolCallAccumulator> = BTreeMap::new();
    let mut usage: Option<Value> = None;

    while let Some(event) = reader.next_event().await? {
        let frames = match transformer.transform(&event)? {
            TransformResult::Emitted(frames) => frames,
            // Go keeps scanning after [DONE]; the upstream closes shortly.
            TransformResult::Done | TransformResult::Swallowed => continue,
        };
        for frame in frames {
            // Skip frames that don't parse as chunks rather than failing the
            // whole request (Go parity).
            let Ok(chunk) = serde_json::from_slice::<StreamingChunk>(&frame) else {
                continue;
            };
            if response_id.is_empty() && !chunk.id.is_empty() {
                response_id = chunk.id;
            }
            if stream_model.is_empty() && !chunk.model.is_empty() {
                stream_model = chunk.model;
            }
            if created == 0 && chunk.created != 0 {
                created = chunk.created;
            }
            if chunk.usage.is_some() {
                usage = chunk.usage;
            }
            for choice in chunk.choices {
                if role.is_empty()
                    && let Some(r) = choice.delta.role
                    && !r.is_empty()
                {
                    role = r;
                }
                if let Some(c) = choice.delta.content {
                    content.push_str(&c);
                }
                if let Some(f) = choice.finish_reason
                    && !f.is_empty()
                {
                    finish_reason = f;
                }
                for tc in choice.delta.tool_calls.into_iter().flatten() {
                    let acc = tool_calls.entry(tc.index).or_default();
                    if let Some(id) = tc.id
                        && !id.is_empty()
                    {
                        acc.id = id;
                    }
                    let _ = tc.call_type;
                    if let Some(func) = tc.function {
                        if let Some(name) = func.name
                            && !name.is_empty()
                        {
                            acc.name = name;
                        }
                        if let Some(args) = func.arguments {
                            acc.arguments.push_str(&args);
                        }
                    }
                }
            }
        }
    }

    // Defaults, matching Go.
    if response_id.is_empty() {
        response_id = "chatcmpl-buffered".to_string();
    }
    if created == 0 {
        created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
    }
    let model = if stream_model.is_empty() {
        model
    } else {
        &stream_model
    };
    if role.is_empty() {
        role = "assistant".to_string();
    }
    if finish_reason.is_empty() {
        finish_reason = "stop".to_string();
    }

    let mut message = json!({ "role": role, "content": content });
    if !tool_calls.is_empty() {
        let calls: Vec<Value> = tool_calls
            .into_values()
            .map(|acc| {
                json!({
                    "id": acc.id,
                    "type": "function",
                    "function": { "name": acc.name, "arguments": acc.arguments },
                })
            })
            .collect();
        message["tool_calls"] = Value::Array(calls);
    }

    let mut response = json!({
        "id": response_id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
    });
    if let Some(usage) = usage {
        response["usage"] = usage;
    }
    Ok(response)
}
