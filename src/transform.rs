use std::collections::HashMap;

use serde_json::{Value, json};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum TransformError {
    #[error("invalid upstream JSON chunk: {0}")]
    InvalidJson(String),
    #[error("failed to marshal chunk: {0}")]
    MarshalError(String),
}

/// Stateful SSE transformer converting upstream Codex events into
/// OpenAI chat-completion chunk JSON. Single-stream, no locking.
pub struct SSETransformer {
    pub model: String,
    pub response_id: String,
    pub role_sent: bool,
    pub tool_index_by_item_id: HashMap<String, usize>,
    pub tool_id_by_item_id: HashMap<String, String>,
    pub tool_name_by_item_id: HashMap<String, String>,
    pub next_tool_index: usize,
    pub saw_tool_calls: bool,
}

const DEFAULT_MODEL: &str = "gpt-5";

impl SSETransformer {
    pub fn new(model: &str) -> Self {
        let model = model.trim();
        let model = if model.is_empty() {
            DEFAULT_MODEL
        } else {
            model
        };
        Self {
            model: model.to_string(),
            response_id: String::new(),
            role_sent: false,
            tool_index_by_item_id: HashMap::new(),
            tool_id_by_item_id: HashMap::new(),
            tool_name_by_item_id: HashMap::new(),
            next_tool_index: 0,
            saw_tool_calls: false,
        }
    }

    fn send_role(&mut self, seq: &Value) -> Result<Option<Vec<u8>>, TransformError> {
        if self.role_sent {
            return Ok(None);
        }
        let chunk = json!({
            "id": self.response_id,
            "object": "chat.completion.chunk",
            "created": seq,
            "model": self.model,
            "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]
        });
        let bytes =
            serde_json::to_vec(&chunk).map_err(|e| TransformError::MarshalError(e.to_string()))?;
        self.role_sent = true;
        Ok(Some(bytes))
    }

    pub fn transform(&mut self, data: &[u8]) -> Result<(Vec<u8>, bool), TransformError> {
        let trimmed = data.trim_ascii();
        if trimmed.is_empty() {
            return Ok((Vec::new(), false));
        }
        if trimmed == b"[DONE]" {
            return Ok((Vec::new(), true));
        }

        let upstream: Value = serde_json::from_slice(trimmed)
            .map_err(|e| TransformError::InvalidJson(e.to_string()))?;
        let event_type = upstream["type"].as_str().unwrap_or("");
        let seq = upstream
            .get("sequence_number")
            .cloned()
            .unwrap_or(Value::Null);

        match event_type {
            "response.created" => self.handle_created(&upstream),
            "response.output_text.delta" => self.handle_text_delta(&upstream, &seq),
            "response.completed" => self.handle_completed(&upstream, &seq),
            _ => Ok((Vec::new(), false)),
        }
    }

    fn handle_created(&mut self, upstream: &Value) -> Result<(Vec<u8>, bool), TransformError> {
        if let Some(resp) = upstream
            .get("response")
            .and_then(|r| r.get("id"))
            .and_then(|id| id.as_str())
        {
            self.response_id = format!("chatcmpl-{resp}");
        }
        Ok((Vec::new(), false))
    }

    fn handle_text_delta(
        &mut self,
        upstream: &Value,
        seq: &Value,
    ) -> Result<(Vec<u8>, bool), TransformError> {
        let mut chunks = Vec::new();
        if let Some(role) = self.send_role(seq)? {
            chunks.push(role);
        }

        let delta = upstream.get("delta").and_then(Value::as_str).unwrap_or("");
        let chunk = json!({
            "id": self.response_id,
            "object": "chat.completion.chunk",
            "created": seq,
            "model": self.model,
            "choices": [{"index": 0, "delta": {"content": delta}, "finish_reason": null}]
        });
        let bytes =
            serde_json::to_vec(&chunk).map_err(|e| TransformError::MarshalError(e.to_string()))?;
        chunks.push(bytes);
        Ok((chunks.join(&b'\n'), false))
    }

    fn handle_completed(
        &mut self,
        upstream: &Value,
        seq: &Value,
    ) -> Result<(Vec<u8>, bool), TransformError> {
        let finish = if self.saw_tool_calls {
            "tool_calls"
        } else {
            "stop"
        };

        let usage = self.extract_usage(upstream);

        let chunk = json!({
            "id": self.response_id,
            "object": "chat.completion.chunk",
            "created": seq,
            "model": self.model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
            "usage": usage
        });
        let bytes =
            serde_json::to_vec(&chunk).map_err(|e| TransformError::MarshalError(e.to_string()))?;
        Ok((bytes, false))
    }

    fn extract_usage(&self, upstream: &Value) -> Value {
        let u = upstream
            .get("response")
            .and_then(|r| r.get("usage"))
            .and_then(Value::as_object);

        let (pt, ct) = match u {
            Some(u) => {
                let pt = u
                    .get("prompt_tokens")
                    .and_then(Value::as_i64)
                    .or_else(|| u.get("input_tokens").and_then(Value::as_i64))
                    .unwrap_or(0);
                let ct = u
                    .get("completion_tokens")
                    .and_then(Value::as_i64)
                    .or_else(|| u.get("output_tokens").and_then(Value::as_i64))
                    .unwrap_or(0);
                (pt, ct)
            }
            None => (0, 0),
        };

        json!({"prompt_tokens": pt, "completion_tokens": ct, "total_tokens": pt + ct})
    }
}
