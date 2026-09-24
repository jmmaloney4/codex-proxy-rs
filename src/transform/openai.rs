use serde::Serialize;

/// Typed OpenAI chat-completion chunk emitted by the transformer.
#[derive(Debug, Serialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: &'static str,
    pub created: Option<u64>,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Serialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: ChunkDelta,
    pub finish_reason: Option<&'static str>,
}

#[derive(Debug, Default, Serialize)]
pub struct ChunkDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Serialize)]
pub struct ToolCallDelta {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub call_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Serialize)]
pub struct FunctionDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Usage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    /// Present only when upstream reported a cache split (see
    /// `UpstreamUsage::to_openai`). Absent means "unknown", not "zero".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// Present only when upstream reported a reasoning-token count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

/// OpenAI `usage.prompt_tokens_details`. The count is not optional: a details
/// object without its count cannot be constructed, so it is never emitted empty.
#[derive(Debug, Serialize)]
pub struct PromptTokensDetails {
    pub cached_tokens: i64,
}

/// OpenAI `usage.completion_tokens_details`.
#[derive(Debug, Serialize)]
pub struct CompletionTokensDetails {
    pub reasoning_tokens: i64,
}

impl Usage {
    pub fn new(prompt_tokens: i64, completion_tokens: i64) -> Self {
        Self::with_total(prompt_tokens, completion_tokens, None)
    }

    /// Create a Usage with an explicit total_tokens from upstream,
    /// falling back to computed `prompt_tokens + completion_tokens`.
    /// No token details; see `UpstreamUsage::to_openai` for those.
    pub fn with_total(prompt_tokens: i64, completion_tokens: i64, total: Option<i64>) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens: total.unwrap_or(prompt_tokens + completion_tokens),
            prompt_tokens_details: None,
            completion_tokens_details: None,
        }
    }
}
