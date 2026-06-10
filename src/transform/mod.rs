mod openai;
mod upstream;

use std::collections::HashMap;

use openai::{ChatCompletionChunk, ChunkChoice, ChunkDelta, Usage};
use thiserror::Error;
use upstream::UpstreamEvent;

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

    fn send_role(&mut self, seq: Option<u64>) -> Result<Option<Vec<u8>>, TransformError> {
        if self.role_sent {
            return Ok(None);
        }
        let chunk = ChatCompletionChunk {
            id: self.response_id.clone(),
            object: "chat.completion.chunk",
            created: seq,
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: Some("assistant"),
                    content: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };
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

        let event: UpstreamEvent = serde_json::from_slice(trimmed)
            .map_err(|e| TransformError::InvalidJson(e.to_string()))?;

        match event {
            UpstreamEvent::ResponseCreated { response, .. } => {
                self.response_id = format!("chatcmpl-{}", response.id);
                Ok((Vec::new(), false))
            }
            UpstreamEvent::OutputTextDelta {
                sequence_number,
                delta,
                ..
            } => self.handle_text_delta(sequence_number, &delta),
            UpstreamEvent::ResponseCompleted {
                sequence_number,
                response,
            } => self.handle_completed(sequence_number, response),
        }
    }

    fn handle_text_delta(
        &mut self,
        seq: Option<u64>,
        delta: &str,
    ) -> Result<(Vec<u8>, bool), TransformError> {
        let mut chunks = Vec::new();
        if let Some(role) = self.send_role(seq)? {
            chunks.push(role);
        }

        let chunk = ChatCompletionChunk {
            id: self.response_id.clone(),
            object: "chat.completion.chunk",
            created: seq,
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: Some(delta.to_string()),
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let bytes =
            serde_json::to_vec(&chunk).map_err(|e| TransformError::MarshalError(e.to_string()))?;
        chunks.push(bytes);
        Ok((chunks.join(&b'\n'), false))
    }

    fn handle_completed(
        &mut self,
        seq: Option<u64>,
        response: upstream::CompletedResponse,
    ) -> Result<(Vec<u8>, bool), TransformError> {
        let finish = if self.saw_tool_calls {
            "tool_calls"
        } else {
            "stop"
        };

        let usage = response.usage.as_ref().map(|u| {
            let (pt, ct) = u.to_openai();
            Usage::new(pt, ct)
        });

        let chunk = ChatCompletionChunk {
            id: self.response_id.clone(),
            object: "chat.completion.chunk",
            created: seq,
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta::default(),
                finish_reason: Some(finish),
            }],
            usage,
        };
        let bytes =
            serde_json::to_vec(&chunk).map_err(|e| TransformError::MarshalError(e.to_string()))?;
        Ok((bytes, false))
    }
}
