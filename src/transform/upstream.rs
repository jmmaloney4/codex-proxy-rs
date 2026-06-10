use serde::Deserialize;

/// Typed upstream event payloads for the handled event families.
///
/// Uses `#[serde(tag = "type")]` so the event discriminator drives deserialization.
/// Each variant only models the fields the transformer actually uses.
///
/// Fields marked `#[expect(dead_code)]` are stable schema members needed for
/// deserialization but not yet consumed by the transformer. They will be used
/// when additional Go test slices are ported.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[expect(dead_code, reason = "schema fields used by future event handlers")]
pub enum UpstreamEvent {
    #[serde(rename = "response.created")]
    ResponseCreated {
        sequence_number: Option<u64>,
        response: CreatedResponse,
    },

    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        sequence_number: Option<u64>,
        #[serde(default)]
        item_id: Option<String>,
        #[serde(default)]
        output_index: Option<u64>,
        #[serde(default)]
        content_index: Option<u64>,
        #[serde(default)]
        delta: String,
    },

    #[serde(rename = "response.completed")]
    ResponseCompleted {
        sequence_number: Option<u64>,
        response: CompletedResponse,
    },
}

#[derive(Debug, Deserialize)]
pub struct CreatedResponse {
    pub id: String,
}

#[derive(Debug, Deserialize)]
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
