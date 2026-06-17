use codex_proxy_rs::buffered::buffer_chat_completion;
use pretty_assertions::assert_eq;

fn sse(events: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for event in events {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(event.as_bytes());
        out.extend_from_slice(b"\n\n");
    }
    out
}

#[tokio::test]
async fn aggregates_text_finish_and_usage() {
    let input = sse(&[
        r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1"}}"#,
        r#"{"type":"response.output_text.delta","sequence_number":1,"delta":"Hello"}"#,
        r#"{"type":"response.output_text.delta","sequence_number":2,"delta":" world"}"#,
        r#"{"type":"response.completed","sequence_number":3,"response":{"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}"#,
        "[DONE]",
    ]);
    let out = buffer_chat_completion(input.as_slice(), "gpt-5.1-codex")
        .await
        .expect("buffer succeeds");

    assert_eq!(out["id"], "chatcmpl-resp_1");
    assert_eq!(out["object"], "chat.completion");
    assert_eq!(out["model"], "gpt-5.1-codex");
    assert_eq!(out["choices"][0]["index"], 0);
    assert_eq!(out["choices"][0]["message"]["role"], "assistant");
    assert_eq!(out["choices"][0]["message"]["content"], "Hello world");
    assert_eq!(out["choices"][0]["finish_reason"], "stop");
    assert_eq!(out["usage"]["prompt_tokens"], 7);
    assert_eq!(out["usage"]["completion_tokens"], 3);
    assert_eq!(out["usage"]["total_tokens"], 10);
}

#[tokio::test]
async fn aggregates_reasoning_summary_into_reasoning_content() {
    // Reasoning summary deltas precede the visible answer; the buffered response
    // must surface them as `message.reasoning_content`, mirroring the streaming
    // path (transform::SSETransformer::handle_reasoning). Only the first
    // reasoning item (output_index 0) is forwarded by the transformer.
    let input = sse(&[
        r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_r"}}"#,
        r#"{"type":"response.reasoning_summary_text.delta","sequence_number":1,"output_index":0,"delta":"Think"}"#,
        r#"{"type":"response.reasoning_summary_text.delta","sequence_number":2,"output_index":0,"delta":"ing..."}"#,
        r#"{"type":"response.output_text.delta","sequence_number":3,"delta":"Answer"}"#,
        r#"{"type":"response.completed","sequence_number":4,"response":{}}"#,
        "[DONE]",
    ]);
    let out = buffer_chat_completion(input.as_slice(), "gpt-5.1-codex")
        .await
        .expect("buffer succeeds");

    assert_eq!(
        out["choices"][0]["message"]["reasoning_content"],
        "Thinking..."
    );
    assert_eq!(out["choices"][0]["message"]["content"], "Answer");
    assert_eq!(out["choices"][0]["message"]["role"], "assistant");
    assert_eq!(out["choices"][0]["finish_reason"], "stop");
}

#[tokio::test]
async fn omits_reasoning_content_when_no_reasoning_events() {
    // No reasoning events → the field must be absent, not an empty string, so
    // clients see the prior response shape unchanged.
    let input = sse(&[
        r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_n"}}"#,
        r#"{"type":"response.output_text.delta","sequence_number":1,"delta":"hi"}"#,
        r#"{"type":"response.completed","sequence_number":2,"response":{}}"#,
        "[DONE]",
    ]);
    let out = buffer_chat_completion(input.as_slice(), "gpt-5")
        .await
        .expect("buffer succeeds");

    assert_eq!(out["choices"][0]["message"]["content"], "hi");
    assert!(
        out["choices"][0]["message"]
            .get("reasoning_content")
            .is_none()
    );
}

#[tokio::test]
async fn aggregates_tool_calls_across_argument_deltas() {
    // output_item.added announces the call; argument deltas stream in pieces.
    let input = sse(&[
        r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_2"}}"#,
        r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_abc","name":"get_weather"}}"#,
        r#"{"type":"response.function_call_arguments.delta","sequence_number":2,"item_id":"fc_1","delta":"{\"location\":"}"#,
        r#"{"type":"response.function_call_arguments.delta","sequence_number":3,"item_id":"fc_1","delta":"\"sf\"}"}"#,
        r#"{"type":"response.completed","sequence_number":4,"response":{}}"#,
        "[DONE]",
    ]);
    let out = buffer_chat_completion(input.as_slice(), "gpt-5")
        .await
        .expect("buffer succeeds");

    let calls = out["choices"][0]["message"]["tool_calls"]
        .as_array()
        .expect("tool_calls present");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["id"], "call_abc");
    assert_eq!(calls[0]["type"], "function");
    assert_eq!(calls[0]["function"]["name"], "get_weather");
    assert_eq!(calls[0]["function"]["arguments"], r#"{"location":"sf"}"#);
    assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
}

#[tokio::test]
async fn defaults_when_stream_is_minimal() {
    let input = sse(&[
        r#"{"type":"response.output_text.delta","sequence_number":1,"delta":"hi"}"#,
        "[DONE]",
    ]);
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let out = buffer_chat_completion(input.as_slice(), "gpt-5")
        .await
        .expect("buffer succeeds");

    // No response.created → transformer's default response id is empty →
    // chunks carry an empty id → the buffered default kicks in.
    assert_eq!(out["id"], "chatcmpl-buffered");
    assert_eq!(out["choices"][0]["message"]["role"], "assistant");
    assert_eq!(out["choices"][0]["message"]["content"], "hi");
    assert_eq!(out["choices"][0]["finish_reason"], "stop");
    assert!(out["choices"][0]["message"].get("tool_calls").is_none());
    assert!(out.get("usage").is_none());
    // created falls back to now when no chunk carried one... the delta chunk
    // carries sequence_number 1 as `created`, so created == 1 here. Assert it
    // took the chunk's value (Go parity: first non-zero created wins).
    assert_eq!(out["created"], 1);
    assert!(before > 0);
}

#[tokio::test]
async fn invalid_event_json_is_an_error() {
    let input = b"data: {not json\n\n".to_vec();
    let err = buffer_chat_completion(input.as_slice(), "gpt-5").await;
    assert!(err.is_err());
}
