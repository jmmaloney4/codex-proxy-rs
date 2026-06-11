use codex_proxy_rs::transform::{SSETransformer, TransformResult};
use pretty_assertions::assert_eq;
use rstest::rstest;
use serde_json::{Value, json};

/// Helper: flatten the per-frame bytes from an Emitted result into a single byte slice.
/// Panics if the result is not Emitted.
fn emitted_bytes(result: TransformResult) -> Vec<u8> {
    match result {
        TransformResult::Emitted(frames) => frames.join(&b'\n'),
        other => panic!("expected Emitted, got {other:?}"),
    }
}

fn parse_json_lines(bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(bytes)
        .expect("valid utf8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid json line"))
        .collect()
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

// --- response.created with missing id ---

#[rstest]
fn handles_created_without_response_id_gracefully() {
    let input = br#"{"type":"response.created","sequence_number":0,"response":{}}"#;
    let mut transformer = SSETransformer::new("gpt-5");
    transformer.response_id = "chatcmpl-old".to_string();

    let result = transformer.transform(input).expect("transform succeeds");
    assert!(matches!(result, TransformResult::Swallowed));
    // response_id should remain unchanged — not overwritten with empty
    assert_eq!(transformer.response_id, "chatcmpl-old");
}

// --- response.created resets state ---

#[rstest]
fn handles_created_resets_per_response_state() {
    let mut transformer = SSETransformer::new("");
    transformer.response_id = "chatcmpl-old".to_string();
    transformer.role_sent = true;
    transformer.saw_tool_calls = true;
    transformer
        .tool_index_by_item_id
        .insert("fc_old".to_string(), 5);
    transformer.next_tool_index = 5;

    let input = br#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_new"}}"#;
    let result = transformer.transform(input).expect("transform succeeds");
    assert!(matches!(result, TransformResult::Swallowed));
    assert_eq!(transformer.response_id, "chatcmpl-resp_new");
    assert!(!transformer.role_sent);
    assert!(!transformer.saw_tool_calls);
    assert!(transformer.tool_index_by_item_id.is_empty());
    assert_eq!(transformer.next_tool_index, 0);
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

// --- completed with upstream total_tokens ---

#[rstest]
fn completed_prefers_upstream_total_tokens() {
    let mut transformer = SSETransformer::new("");
    transformer.response_id = "chatcmpl-resp_123".to_string();

    let input = br#"{"type":"response.completed","sequence_number":92,"response":{"usage":{"input_tokens":11,"output_tokens":7,"total_tokens":100}}}"#;

    let result = transformer.transform(input).expect("transform succeeds");
    let out = emitted_bytes(result);
    let chunk: Value = serde_json::from_slice(&out).expect("valid json");
    assert_eq!(chunk["usage"]["total_tokens"], json!(100));
}

// --- completed without usage (now emits zeroed usage) ---

#[rstest]
fn handles_completed_without_usage() {
    let mut transformer = SSETransformer::new("");
    transformer.response_id = "chatcmpl-resp_123".to_string();

    let input = br#"{"type":"response.completed","sequence_number":92,"response":{}}"#;

    let result = transformer.transform(input).expect("transform succeeds");

    let out = emitted_bytes(result);
    let chunk: Value = serde_json::from_slice(&out).expect("valid json");
    assert_eq!(chunk["choices"][0]["finish_reason"], json!("stop"));
    // Must emit a zeroed usage object, not omit it
    assert_eq!(
        chunk["usage"],
        json!({
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0
        })
    );
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

// --- tool-call output_item.added with empty id/name is swallowed ---

#[rstest]
fn swallows_output_item_added_with_empty_id_or_name() {
    let mut transformer = SSETransformer::new("");
    transformer.response_id = "chatcmpl-resp_456".to_string();

    let input = br#"{"type":"response.output_item.added","sequence_number":30,"item":{"id":"","call_id":"","type":"function_call","name":""}}"#;

    let result = transformer.transform(input).expect("transform succeeds");
    assert!(matches!(result, TransformResult::Swallowed));
    assert!(!transformer.saw_tool_calls);
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

// --- new tests (21–25) ---

#[rstest]
fn state_reset_across_responses() {
    let mut t = SSETransformer::new("gpt-5");
    let r1 = json!({"type": "response.created", "response": {"id": "abc"}});
    t.transform(&serde_json::to_vec(&r1).unwrap()).unwrap();
    // send text delta
    let td = json!({"type": "response.output_text.delta", "delta": "hello", "sequence_number": 1});
    let result = t.transform(&serde_json::to_vec(&td).unwrap()).unwrap();
    assert!(matches!(result, TransformResult::Emitted(_)));
    assert!(t.role_sent);
    // new response.created resets
    let r2 = json!({"type": "response.created", "response": {"id": "def"}});
    t.transform(&serde_json::to_vec(&r2).unwrap()).unwrap();
    assert!(!t.role_sent, "role_sent should be reset");
    assert!(
        t.response_id.contains("def"),
        "response_id should be updated"
    );
}

#[rstest]
fn multiple_tool_calls_indexed() {
    let mut t = SSETransformer::new("gpt-5");
    t.response_id = "chatcmpl-test".to_string();
    // First tool
    let item1 = json!({
        "type": "response.output_item.added",
        "item": {"id": "fc1", "call_id": "call_1", "name": "tool_a", "type": "function_call"},
    });
    let r1 = t.transform(&serde_json::to_vec(&item1).unwrap()).unwrap();
    let out1 = emitted_bytes(r1);
    let lines1 = parse_json_lines(&out1);
    // lines[0] = role, lines[1] = tool_call chunk
    let tc1 = &lines1[1]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tc1["index"], json!(0));

    // Second tool — role already sent, so only the tool_call chunk
    let item2 = json!({
        "type": "response.output_item.added",
        "item": {"id": "fc2", "call_id": "call_2", "name": "tool_b", "type": "function_call"},
    });
    let r2 = t.transform(&serde_json::to_vec(&item2).unwrap()).unwrap();
    let out2 = emitted_bytes(r2);
    let lines2 = parse_json_lines(&out2);
    // No role frame this time (role already sent), so single frame at index 0
    let tc2 = &lines2[0]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tc2["index"], json!(1));
}

#[rstest]
fn completed_with_tool_calls_finish_reason() {
    let mut t = SSETransformer::new("gpt-5");
    t.response_id = "chatcmpl-test".to_string();
    // Trigger tool call
    let item = json!({
        "type": "response.output_item.added",
        "item": {"id": "fc1", "call_id": "call_1", "name": "tool_a", "type": "function_call"},
    });
    t.transform(&serde_json::to_vec(&item).unwrap()).unwrap();
    // Complete
    let completed = json!({
        "type": "response.completed",
        "response": {"usage": {"input_tokens": 10, "output_tokens": 5}},
    });
    let result = t
        .transform(&serde_json::to_vec(&completed).unwrap())
        .unwrap();
    let bytes = match result {
        TransformResult::Emitted(b) => b,
        _ => panic!("expected Emitted"),
    };
    let chunk: Value = serde_json::from_slice(&bytes[0]).unwrap();
    assert_eq!(chunk["choices"][0]["finish_reason"], json!("tool_calls"));
}

#[rstest]
fn reasoning_summary_extracted() {
    let mut t = SSETransformer::new("gpt-5");
    t.response_id = "chatcmpl-test".to_string();
    let event = json!({
        "type": "response.reasoning.delta",
        "summary": [{ "type": "summary_text", "text": "thinking about it" }],
    });
    let result = t.transform(&serde_json::to_vec(&event).unwrap()).unwrap();
    assert!(matches!(result, TransformResult::Emitted(_)));
}

#[rstest]
fn empty_input_swallowed() {
    let mut t = SSETransformer::new("gpt-5");
    let result = t.transform(b"").unwrap();
    assert!(matches!(result, TransformResult::Swallowed));
    let result2 = t.transform(b"   ").unwrap();
    assert!(matches!(result2, TransformResult::Swallowed));
}
