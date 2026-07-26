use codex_proxy_rs::buffered::{BufferError, buffer_chat_completion, buffer_responses_response};
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

// ---- buffer_responses_response ------------------------------------------

#[tokio::test]
async fn responses_returns_the_completed_response_object_verbatim() {
    let input = sse(&[
        r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","status":"in_progress"}}"#,
        r#"{"type":"response.output_text.delta","sequence_number":1,"delta":"Hello"}"#,
        r#"{"type":"response.completed","sequence_number":2,"response":{"id":"resp_1","object":"response","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hello"}]}],"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}"#,
        "[DONE]",
    ]);

    let out = buffer_responses_response(input.as_slice())
        .await
        .expect("buffer succeeds");

    // The terminal event's `response` is exactly the non-streaming body, so it
    // must come back untouched — not re-derived from the deltas.
    assert_eq!(out["id"], "resp_1");
    assert_eq!(out["object"], "response");
    assert_eq!(out["status"], "completed");
    assert_eq!(out["output"][0]["content"][0]["text"], "Hello");
    assert_eq!(out["usage"]["total_tokens"], 10);
}

#[tokio::test]
async fn responses_treats_failed_and_incomplete_as_terminal() {
    // The Responses API reports both as a 200 whose body describes the outcome,
    // so they are responses to return, not errors to raise.
    for status in ["failed", "incomplete"] {
        let event = format!(
            r#"{{"type":"response.{status}","response":{{"id":"resp_2","status":"{status}"}}}}"#
        );
        let input = sse(&[event.as_str()]);
        let out = buffer_responses_response(input.as_slice())
            .await
            .unwrap_or_else(|err| panic!("{status} should buffer: {err}"));
        assert_eq!(out["status"], status);
    }
}

#[tokio::test]
async fn responses_skips_non_json_and_untyped_frames() {
    let input = sse(&[
        "[DONE]",
        r#"{"no_type_field":true}"#,
        r#"{"type":"response.completed","response":{"id":"resp_3"}}"#,
    ]);

    let out = buffer_responses_response(input.as_slice())
        .await
        .expect("buffer succeeds");
    assert_eq!(out["id"], "resp_3");
}

#[tokio::test]
async fn responses_without_terminal_event_is_an_error() {
    // A truncated stream must fail loudly rather than hand the caller a
    // plausible-looking partial response.
    let input = sse(&[r#"{"type":"response.created","response":{"id":"resp_4"}}"#]);
    let err = buffer_responses_response(input.as_slice())
        .await
        .expect_err("truncated stream must error");
    assert!(
        matches!(
            err,
            BufferError::MissingTerminalEvent {
                upstream_error: None
            }
        ),
        "unexpected error: {err:?}",
    );
}

#[tokio::test]
async fn responses_error_event_is_retained_for_diagnostics() {
    let input = sse(&[r#"{"type":"error","code":"server_error","message":"upstream exploded"}"#]);
    let err = buffer_responses_response(input.as_slice())
        .await
        .expect_err("error-only stream must error");
    // The handler logs this failure through `Display`, so the retained payload
    // is only useful if it survives into the message an operator actually sees.
    let rendered = err.to_string();
    assert!(
        rendered.contains("upstream exploded"),
        "upstream diagnostic missing from the logged message: {rendered}",
    );

    let BufferError::MissingTerminalEvent {
        upstream_error: Some(payload),
    } = err
    else {
        panic!("expected the upstream error to be carried on the failure");
    };
    assert_eq!(payload["message"], "upstream exploded");
}
