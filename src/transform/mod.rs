mod openai;
mod upstream;

use std::collections::HashMap;

use openai::{ChatCompletionChunk, ChunkChoice, ChunkDelta, FunctionDelta, ToolCallDelta, Usage};
use thiserror::Error;
use upstream::EventEnvelope;

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
        let chunk = self.make_chunk(
            seq,
            ChunkDelta {
                role: Some("assistant"),
                ..Default::default()
            },
            None,
            None,
        )?;
        self.role_sent = true;
        Ok(Some(chunk))
    }

    fn make_chunk(
        &self,
        seq: Option<u64>,
        delta: ChunkDelta,
        finish_reason: Option<&'static str>,
        usage: Option<Usage>,
    ) -> Result<Vec<u8>, TransformError> {
        let chunk = ChatCompletionChunk {
            id: self.response_id.clone(),
            object: "chat.completion.chunk",
            created: seq,
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta,
                finish_reason,
            }],
            usage,
        };
        serde_json::to_vec(&chunk).map_err(|e| TransformError::MarshalError(e.to_string()))
    }

    pub fn transform(&mut self, data: &[u8]) -> Result<(Vec<u8>, bool), TransformError> {
        let trimmed = data.trim_ascii();
        if trimmed.is_empty() {
            return Ok((Vec::new(), false));
        }
        if trimmed == b"[DONE]" {
            return Ok((Vec::new(), true));
        }

        let envelope: EventEnvelope = serde_json::from_slice(trimmed)
            .map_err(|e| TransformError::InvalidJson(e.to_string()))?;

        let seq = envelope.sequence_number;

        // -- Reasoning events (prefix match) --
        if envelope.event_type.starts_with("response.reasoning") {
            return self.handle_reasoning(&envelope, seq);
        }

        match envelope.event_type.as_str() {
            "response.created" => self.handle_created(&envelope),
            "response.output_text.delta" => self.handle_text_delta(&envelope, seq),
            "response.completed" => self.handle_completed(&envelope, seq),
            "response.output_item.added" => self.handle_output_item_added(&envelope, seq),
            "response.function_call_arguments.delta" => {
                self.handle_function_call_args_delta(&envelope, seq)
            }
            // Explicitly ignored: no emission, not done
            "response.function_call_arguments.done"
            | "response.output_item.done"
            | "response.output_text.done" => Ok((Vec::new(), false)),
            // Unknown events: no-op
            _ => Ok((Vec::new(), false)),
        }
    }

    fn handle_created(
        &mut self,
        envelope: &EventEnvelope,
    ) -> Result<(Vec<u8>, bool), TransformError> {
        let payload: upstream::CreatedPayload = serde_json::from_value(envelope.extra.clone())
            .map_err(|e| TransformError::InvalidJson(e.to_string()))?;
        self.response_id = format!("chatcmpl-{}", payload.response.id);
        Ok((Vec::new(), false))
    }

    fn handle_text_delta(
        &mut self,
        envelope: &EventEnvelope,
        seq: Option<u64>,
    ) -> Result<(Vec<u8>, bool), TransformError> {
        let payload: upstream::OutputTextDeltaPayload =
            serde_json::from_value(envelope.extra.clone())
                .map_err(|e| TransformError::InvalidJson(e.to_string()))?;

        let mut chunks = Vec::new();
        if let Some(role) = self.send_role(seq)? {
            chunks.push(role);
        }

        let bytes = self.make_chunk(
            seq,
            ChunkDelta {
                content: Some(payload.delta),
                ..Default::default()
            },
            None,
            None,
        )?;
        chunks.push(bytes);
        Ok((chunks.join(&b'\n'), false))
    }

    fn handle_completed(
        &mut self,
        envelope: &EventEnvelope,
        seq: Option<u64>,
    ) -> Result<(Vec<u8>, bool), TransformError> {
        let payload: upstream::CompletedPayload = serde_json::from_value(envelope.extra.clone())
            .map_err(|e| TransformError::InvalidJson(e.to_string()))?;

        let finish = if self.saw_tool_calls {
            "tool_calls"
        } else {
            "stop"
        };

        let usage = payload.response.usage.as_ref().map(|u| {
            let (pt, ct) = u.to_openai();
            Usage::new(pt, ct)
        });

        let bytes = self.make_chunk(seq, ChunkDelta::default(), Some(finish), usage)?;
        Ok((bytes, false))
    }

    fn handle_reasoning(
        &mut self,
        envelope: &EventEnvelope,
        seq: Option<u64>,
    ) -> Result<(Vec<u8>, bool), TransformError> {
        // Only process the first reasoning item (output_index: 0)
        if envelope.output_index().unwrap_or(0) > 0 {
            return Ok((Vec::new(), false));
        }
        // Only process .delta events
        if !envelope.event_type.contains(".delta") {
            return Ok((Vec::new(), false));
        }
        let reasoning_text = match envelope.extract_reasoning_content() {
            Some(t) => t,
            None => return Ok((Vec::new(), false)),
        };

        let mut chunks = Vec::new();
        if let Some(role) = self.send_role(seq)? {
            chunks.push(role);
        }

        let bytes = self.make_chunk(
            seq,
            ChunkDelta {
                reasoning_content: Some(reasoning_text),
                ..Default::default()
            },
            None,
            None,
        )?;
        chunks.push(bytes);
        Ok((chunks.join(&b'\n'), false))
    }

    fn handle_output_item_added(
        &mut self,
        envelope: &EventEnvelope,
        seq: Option<u64>,
    ) -> Result<(Vec<u8>, bool), TransformError> {
        let payload: upstream::OutputItemAddedPayload =
            serde_json::from_value(envelope.extra.clone())
                .map_err(|e| TransformError::InvalidJson(e.to_string()))?;

        let item = match payload.item {
            Some(i) => i,
            None => return Ok((Vec::new(), false)),
        };

        if item.item_type != "function_call" {
            return Ok((Vec::new(), false));
        }

        let fc_id = item.id;
        let call_id = if item.call_id.is_empty() {
            format!("call_{}", fc_id)
        } else {
            item.call_id
        };
        let name = item.name;

        // Assign tool index
        let idx = *self
            .tool_index_by_item_id
            .entry(fc_id.clone())
            .or_insert_with(|| {
                let i = self.next_tool_index;
                self.next_tool_index += 1;
                i
            });

        self.tool_id_by_item_id
            .insert(fc_id.clone(), call_id.clone());
        self.tool_name_by_item_id.insert(fc_id, name.clone());
        self.saw_tool_calls = true;

        let mut chunks = Vec::new();
        if let Some(role) = self.send_role(seq)? {
            chunks.push(role);
        }

        let bytes = self.make_chunk(
            seq,
            ChunkDelta {
                tool_calls: Some(vec![ToolCallDelta {
                    index: idx,
                    id: Some(call_id),
                    call_type: Some("function"),
                    function: Some(FunctionDelta {
                        name: Some(name),
                        arguments: Some(String::new()),
                    }),
                }]),
                ..Default::default()
            },
            None,
            None,
        )?;
        chunks.push(bytes);
        Ok((chunks.join(&b'\n'), false))
    }

    fn handle_function_call_args_delta(
        &mut self,
        envelope: &EventEnvelope,
        seq: Option<u64>,
    ) -> Result<(Vec<u8>, bool), TransformError> {
        let payload: upstream::FunctionCallArgsDeltaPayload =
            serde_json::from_value(envelope.extra.clone())
                .map_err(|e| TransformError::InvalidJson(e.to_string()))?;

        let idx = match self.tool_index_by_item_id.get(&payload.item_id) {
            Some(&i) => i,
            None => return Ok((Vec::new(), false)),
        };

        let mut chunks = Vec::new();
        if let Some(role) = self.send_role(seq)? {
            chunks.push(role);
        }

        let bytes = self.make_chunk(
            seq,
            ChunkDelta {
                tool_calls: Some(vec![ToolCallDelta {
                    index: idx,
                    id: None,
                    call_type: None,
                    function: Some(FunctionDelta {
                        name: None,
                        arguments: Some(payload.delta),
                    }),
                }]),
                ..Default::default()
            },
            None,
            None,
        )?;
        chunks.push(bytes);
        Ok((chunks.join(&b'\n'), false))
    }
}
