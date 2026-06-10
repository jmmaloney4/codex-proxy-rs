use codex_proxy_rs::transform::SSETransformer;
use pretty_assertions::assert_eq;
use rstest::rstest;
use serde_json::{Value, json};

fn parse_json_lines(bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(bytes)
        .expect("valid utf8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid json line"))
        .collect()
}

#[rstest]
fn transforms_first_output_text_delta_into_role_and_content_chunks() {
    let mut transformer = SSETransformer::new("");
    transformer.response_id = "chatcmpl-resp_123".to_string();

    let input = br#"{"type":"response.output_text.delta","sequence_number":80,"item_id":"msg_123","output_index":1,"content_index":0,"delta":"Hello"}"#;

    let (out, done) = transformer.transform(input).expect("transform succeeds");

    assert!(!done);
    let chunks = parse_json_lines(&out);
    assert_eq!(chunks.len(), 2);

    assert_eq!(
        chunks[0]["choices"][0]["delta"],
        json!({"role": "assistant"})
    );
    assert_eq!(
        chunks[1]["choices"][0]["delta"],
        json!({"content": "Hello"})
    );
}

#[rstest]
fn transforms_completed_event_into_final_chunk_with_usage() {
    let mut transformer = SSETransformer::new("");
    transformer.response_id = "chatcmpl-resp_123".to_string();

    let input = br#"{"type":"response.completed","sequence_number":92,"response":{"usage":{"input_tokens":11,"output_tokens":7}}}"#;

    let (out, done) = transformer.transform(input).expect("transform succeeds");

    assert!(!done);
    let chunk: Value = serde_json::from_slice(&out).expect("valid json");

    assert_eq!(chunk["object"], json!("chat.completion.chunk"));
    assert_eq!(chunk["choices"][0]["finish_reason"], json!("stop"));
    assert_eq!(
        chunk["usage"],
        json!({
            "prompt_tokens": 11,
            "completion_tokens": 7,
            "total_tokens": 18
        })
    );
}
