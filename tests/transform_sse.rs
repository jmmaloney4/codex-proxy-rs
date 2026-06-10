use codex_proxy_rs::transform::{SSETransformer, TransformResult};
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

/// Helper: extract emitted bytes or panic with a context message.
fn emitted_bytes(result: TransformResult) -> Vec<u8> {
    match result {
        TransformResult::Emitted(bytes) => bytes,
        other => panic!("expected Emitted, got {other:?}"),
    }
}

// --- response.created ---

#[rstest]
fn handles_response_created_and_stores_response_id() {
    let input = br#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_123"}}"#;
    let mut transformer = SSETransformer::new("");

    let result = transformer.transform(input).expect("transform succeeds");

    assert!(matches!(result, TransformResult::Swallowed));
    assert_eq!(transformer.response_id, "chatcmpl-resp_123");
}

// --- output_text.delta (first) ---

#[rstest]
fn transforms_first_output_text_delta_into_role_and_content_chunks() {
    let mut transformer = SSETransformer::new("");
    transformer.response_id = "chatcmpl-resp_123".to_string();

    let input = br#"{"type":"response.output_text.delta","sequence_number":80,"item_id":"msg_123","output_index":1,"content_index":0,"delta":"Hello"}"#;

    let result = transformer.transform(input).expect("transform succeeds");

    let out = emitted_bytes(result);
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

// --- output_text.delta (subsequent) ---

#[rstest]
fn handles_subsequent_output_text_delta_without_role() {
    let mut transformer = SSETransformer::new("");
    transformer.response_id = "chatcmpl-resp_123".to_string();
    transformer.role_sent = true;

    let input = br#"{"type":"response.output_text.delta","sequence_number":81,"item_id":"msg_123","output_index":1,"content_index":0,"delta":" world"}"#;

    let result = transformer.transform(input).expect("transform succeeds");

    let out = emitted_bytes(result);
    let chunk: Value = serde_json::from_slice(&out).expect("valid json");
    assert_eq!(chunk["object"], json!("chat.completion.chunk"));
    assert_eq!(chunk["choices"][0]["delta"]["content"], json!(" world"));
    assert!(chunk["choices"][0]["delta"]["role"].is_null());
}

// --- reasoning delta ---

#[rstest]
fn handles_reasoning_delta_event() {
    let mut transformer = SSETransformer::new("");
    transformer.response_id = "chatcmpl-resp_123".to_string();

    let input = br#"{"type":"response.reasoning_summary_text.delta","sequence_number":5,"item_id":"rs_1","summary_index":0,"delta":"Thinking..."}"#;

    let result = transformer.transform(input).expect("transform succeeds");
    let out = emitted_bytes(result);
    let lines = parse_json_lines(&out);
    assert_eq!(lines.len(), 2);

    assert_eq!(lines[0]["choices"][0]["delta"]["role"], json!("assistant"));
    assert_eq!(
        lines[1]["choices"][0]["delta"]["reasoning_content"],
        json!("Thinking...")
    );
}

// --- [DONE] ---

#[rstest]
fn handles_done_marker() {
    let mut transformer = SSETransformer::new("");
    let input = b"[DONE]";

    let result = transformer.transform(input).expect("transform succeeds");

    assert!(matches!(result, TransformResult::Done));
}

// --- ignored events ---

#[rstest]
fn ignores_unknown_events() {
    let mut transformer = SSETransformer::new("");
    let input = br#"{"type":"response.in_progress","sequence_number":1,"response":{}}"#;

    let result = transformer.transform(input).expect("transform succeeds");

    assert!(matches!(result, TransformResult::Swallowed));
}

// --- completed with usage ---

#[rstest]
fn transforms_completed_event_into_final_chunk_with_usage() {
    let mut transformer = SSETransformer::new("");
    transformer.response_id = "chatcmpl-resp_123".to_string();

    let input = br#"{"type":"response.completed","sequence_number":92,"response":{"usage":{"input_tokens":11,"output_tokens":7}}}"#;

    let result = transformer.transform(input).expect("transform succeeds");

    let out = emitted_bytes(result);
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

// --- completed without usage ---

#[rstest]
fn handles_completed_without_usage() {
    let mut transformer = SSETransformer::new("");
    transformer.response_id = "chatcmpl-resp_123".to_string();

    let input = br#"{"type":"response.completed","sequence_number":92,"response":{}}"#;

    let result = transformer.transform(input).expect("transform succeeds");

    let out = emitted_bytes(result);
    let chunk: Value = serde_json::from_slice(&out).expect("valid json");
    assert_eq!(chunk["choices"][0]["finish_reason"], json!("stop"));
    assert!(chunk.get("usage").is_none());
}

// --- tool-call output_item.added ---

#[rstest]
fn handles_output_item_added_for_function_call() {
    let mut transformer = SSETransformer::new("");
    transformer.response_id = "chatcmpl-resp_456".to_string();

    let input = br#"{"type":"response.output_item.added","sequence_number":30,"item":{"id":"fc_1","call_id":"call_abc","type":"function_call","name":"get_weather"}}"#;

    let result = transformer.transform(input).expect("transform succeeds");
    let out = emitted_bytes(result);
    let lines = parse_json_lines(&out);
    assert_eq!(lines.len(), 2);

    assert_eq!(lines[0]["choices"][0]["delta"]["role"], json!("assistant"));

    let tc = &lines[1]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tc["index"], json!(0));
    assert_eq!(tc["id"], json!("call_abc"));
    assert_eq!(tc["type"], json!("function"));
    assert_eq!(tc["function"]["name"], json!("get_weather"));
    assert_eq!(tc["function"]["arguments"], json!(""));

    assert!(transformer.saw_tool_calls);
    assert_eq!(transformer.tool_index_by_item_id["fc_1"], 0);
    assert_eq!(transformer.tool_id_by_item_id["fc_1"], "call_abc");
}

// --- function_call_arguments.delta ---

#[rstest]
fn handles_function_call_args_delta() {
    let mut transformer = SSETransformer::new("");
    transformer.response_id = "chatcmpl-resp_456".to_string();
    transformer.role_sent = true;
    transformer.saw_tool_calls = true;
    transformer
        .tool_index_by_item_id
        .insert("fc_1".to_string(), 0);

    let input = br#"{"type":"response.function_call_arguments.delta","sequence_number":31,"item_id":"fc_1","delta":"{\"city\":"}"#;

    let result = transformer.transform(input).expect("transform succeeds");
    let out = emitted_bytes(result);
    let chunk: Value = serde_json::from_slice(&out).expect("valid json");
    let tc = &chunk["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tc["index"], json!(0));
    assert_eq!(tc["function"]["arguments"], json!("{\"city\":"));
}

// --- ignored explicit events ---

#[rstest]
fn ignores_function_call_arguments_done() {
    let mut transformer = SSETransformer::new("");
    let input = br#"{"type":"response.function_call_arguments.done","sequence_number":32,"item_id":"fc_1"}"#;

    let result = transformer.transform(input).expect("transform succeeds");
    assert!(matches!(result, TransformResult::Swallowed));
}

#[rstest]
fn ignores_output_item_done() {
    let mut transformer = SSETransformer::new("");
    let input = br#"{"type":"response.output_item.done","sequence_number":33,"item":{"id":"fc_1","type":"function_call"}}"#;

    let result = transformer.transform(input).expect("transform succeeds");
    assert!(matches!(result, TransformResult::Swallowed));
}

// --- reasoning edge cases ---

#[rstest]
fn skips_reasoning_events_with_nonzero_output_index() {
    let mut transformer = SSETransformer::new("");
    transformer.response_id = "chatcmpl-resp_123".to_string();

    let input = br#"{"type":"response.reasoning_summary_text.delta","sequence_number":10,"item_id":"rs_2","output_index":1,"summary_index":0,"delta":"More thinking"}"#;

    let result = transformer.transform(input).expect("transform succeeds");
    assert!(matches!(result, TransformResult::Swallowed));
}

#[rstest]
fn skips_reasoning_events_that_are_not_deltas() {
    let mut transformer = SSETransformer::new("");
    transformer.response_id = "chatcmpl-resp_123".to_string();

    let input = br#"{"type":"response.reasoning_summary_text.done","sequence_number":10,"item_id":"rs_1","output_index":0,"summary_index":0,"delta":"Done thinking"}"#;

    let result = transformer.transform(input).expect("transform succeeds");
    assert!(matches!(result, TransformResult::Swallowed));
}

// --- completed with tool_calls finish reason ---

#[rstest]
fn handles_completed_with_tool_calls_finish_reason() {
    let mut transformer = SSETransformer::new("");
    transformer.response_id = "chatcmpl-resp_456".to_string();
    transformer.saw_tool_calls = true;

    let input = br#"{"type":"response.completed","sequence_number":92,"response":{}}"#;

    let result = transformer.transform(input).expect("transform succeeds");
    let out = emitted_bytes(result);
    let chunk: Value = serde_json::from_slice(&out).expect("valid json");
    assert_eq!(chunk["choices"][0]["finish_reason"], json!("tool_calls"));
}
