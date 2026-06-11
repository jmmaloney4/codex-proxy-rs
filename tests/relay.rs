use std::time::Duration;

use codex_proxy_rs::relay::{
    RelayConfig, RelayError, pass_through_sse_stream, rewrite_sse_stream,
    rewrite_sse_stream_with_callback,
};
use codex_proxy_rs::transform::TransformResult;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

// ---- helpers ---------------------------------------------------------------

/// Run the rewrite relay over a static upstream byte slice, collecting the
/// full downstream output.
async fn run_rewrite(input: &[u8]) -> (String, Result<(), RelayError>) {
    let mut output: Vec<u8> = Vec::new();
    let result = rewrite_sse_stream(input, &mut output, "gpt-5", &RelayConfig::default()).await;
    (String::from_utf8(output).expect("valid utf8"), result)
}

async fn run_pass_through(input: &[u8]) -> (String, Result<(), RelayError>) {
    let mut output: Vec<u8> = Vec::new();
    let result = pass_through_sse_stream(input, &mut output, &RelayConfig::default()).await;
    (String::from_utf8(output).expect("valid utf8"), result)
}

/// Split downstream output into SSE frames (on the blank-line separator).
fn frames(output: &str) -> Vec<&str> {
    output.split("\n\n").filter(|f| !f.is_empty()).collect()
}

/// Parse the JSON payload of a `data: ` frame.
fn frame_json(frame: &str) -> Value {
    let payload = frame.strip_prefix("data: ").expect("data frame");
    serde_json::from_str(payload).expect("valid json frame")
}

const CREATED: &str =
    r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_123"}}"#;
const TEXT_DELTA: &str = r#"{"type":"response.output_text.delta","sequence_number":80,"item_id":"msg_123","output_index":1,"content_index":0,"delta":"Hello"}"#;
const COMPLETED: &str = r#"{"type":"response.completed","sequence_number":99,"response":{"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#;

fn sse(events: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for event in events {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(event.as_bytes());
        out.extend_from_slice(b"\n\n");
    }
    out
}

// ---- rewrite relay: framing + termination ----------------------------------

#[tokio::test]
async fn rewrite_happy_path_ends_with_done_sentinel() {
    let input = sse(&[CREATED, TEXT_DELTA, COMPLETED, "[DONE]"]);
    let (output, result) = run_rewrite(&input).await;
    result.expect("relay succeeds");

    let frames = frames(&output);
    // role chunk + content chunk + finish chunk + usage chunk + [DONE]
    assert_eq!(*frames.last().unwrap(), "data: [DONE]");
    let role = frame_json(frames[0]);
    assert_eq!(role["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(role["id"], "chatcmpl-resp_123");
    let content = frame_json(frames[1]);
    assert_eq!(content["choices"][0]["delta"]["content"], "Hello");
    assert_eq!(content["object"], "chat.completion.chunk");
}

#[tokio::test]
async fn rewrite_appends_done_when_upstream_ends_without_sentinel() {
    // garden#796: upstream dies after a delta with no [DONE]. Downstream must
    // still see the sentinel.
    let input = sse(&[CREATED, TEXT_DELTA]);
    let (output, result) = run_rewrite(&input).await;
    result.expect("relay succeeds");
    assert!(output.ends_with("data: [DONE]\n\n"), "output: {output}");
}

#[tokio::test]
async fn rewrite_emits_done_for_empty_upstream() {
    let (output, result) = run_rewrite(b"").await;
    result.expect("relay succeeds");
    assert_eq!(output, "data: [DONE]\n\n");
}

#[tokio::test]
async fn rewrite_invalid_json_emits_error_event_then_done() {
    // ADR 002 §3: error event + sentinel, and the error is surfaced.
    let input = b"data: {not json\n\n";
    let (output, result) = run_rewrite(input).await;
    assert!(matches!(result, Err(RelayError::Transform(_))));

    let frames = frames(&output);
    assert_eq!(frames.len(), 2);
    let error = frame_json(frames[0]);
    let message = error["error"].as_str().expect("error message");
    assert!(message.starts_with("stream error: "), "message: {message}");
    assert_eq!(frames[1], "data: [DONE]");
}

#[tokio::test]
async fn rewrite_stops_after_upstream_done() {
    // Divergence from Go (documented in the module docs): nothing is relayed
    // after [DONE] — it is always the final frame.
    let input = sse(&[CREATED, "[DONE]", TEXT_DELTA]);
    let (output, result) = run_rewrite(&input).await;
    result.expect("relay succeeds");
    assert_eq!(output, "data: [DONE]\n\n");
}

// ---- rewrite relay: OpenAI-chunk pass-through -------------------------------

#[tokio::test]
async fn rewrite_passes_through_openai_chunks_unchanged() {
    let chunk = r#"{"object":"chat.completion.chunk","id":"chatcmpl-x","choices":[]}"#;
    let input = sse(&[chunk]);
    let (output, result) = run_rewrite(&input).await;
    result.expect("relay succeeds");

    let frames = frames(&output);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0], format!("data: {chunk}"));
    assert_eq!(frames[1], "data: [DONE]");
}

#[tokio::test]
async fn rewrite_ignores_typeless_non_chunk_json() {
    // Go parity: valid JSON without a `type` is an unknown event — swallowed,
    // not an error.
    let input = sse(&[r#"{"object":"something.else","x":1}"#]);
    let (output, result) = run_rewrite(&input).await;
    result.expect("relay succeeds");
    assert_eq!(output, "data: [DONE]\n\n");
}

// ---- rewrite relay: SSE framing edge cases ----------------------------------

#[tokio::test]
async fn rewrite_handles_crlf_comments_and_multiline_data() {
    let mut input = Vec::new();
    // CRLF line endings, an SSE comment, and a multi-line data event whose
    // joined payload is a single JSON document.
    input.extend_from_slice(b": upstream comment\r\n");
    input.extend_from_slice(b"data: {\"type\":\"response.created\",\r\n");
    input.extend_from_slice(b"data: \"response\":{\"id\":\"resp_9\"}}\r\n");
    input.extend_from_slice(b"\r\n");
    input.extend_from_slice(b"data: [DONE]\r\n\r\n");

    let (output, result) = run_rewrite(&input).await;
    result.expect("relay succeeds");
    assert_eq!(output, "data: [DONE]\n\n");
}

#[tokio::test]
async fn rewrite_flushes_trailing_event_without_blank_line() {
    // No terminating blank line after the final event, no trailing newline.
    let input = b"data: [DONE]";
    let (output, result) = run_rewrite(input).await;
    result.expect("relay succeeds");
    assert_eq!(output, "data: [DONE]\n\n");
}

#[tokio::test]
async fn rewrite_has_no_token_size_cap() {
    // ADR 002 §4: Go's bufio.Scanner capped tokens at 10MB. Build a single
    // 11MB delta event and ensure it relays.
    let big = "x".repeat(11 * 1024 * 1024);
    let event =
        format!(r#"{{"type":"response.output_text.delta","sequence_number":1,"delta":"{big}"}}"#);
    let input = sse(&[CREATED, &event, "[DONE]"]);
    let (output, result) = run_rewrite(&input).await;
    result.expect("relay succeeds");
    assert!(output.contains(&big));
    assert!(output.ends_with("data: [DONE]\n\n"));
}

// ---- rewrite relay: callback -------------------------------------------------

#[tokio::test]
async fn rewrite_callback_observes_each_event() {
    let input = sse(&[CREATED, TEXT_DELTA, "[DONE]"]);
    let mut seen: Vec<(Vec<u8>, &'static str)> = Vec::new();
    let mut output: Vec<u8> = Vec::new();
    rewrite_sse_stream_with_callback(
        input.as_slice(),
        &mut output,
        "gpt-5",
        &RelayConfig::default(),
        |raw, result| {
            let kind = match result {
                TransformResult::Emitted(_) => "emitted",
                TransformResult::Swallowed => "swallowed",
                TransformResult::Done => "done",
            };
            seen.push((raw.to_vec(), kind));
        },
    )
    .await
    .expect("relay succeeds");

    let kinds: Vec<&str> = seen.iter().map(|(_, k)| *k).collect();
    assert_eq!(kinds, vec!["swallowed", "emitted", "done"]);
    assert_eq!(seen[0].0, CREATED.as_bytes());
}

// ---- rewrite relay: keepalive -------------------------------------------------

#[tokio::test(start_paused = true)]
async fn rewrite_emits_keepalive_when_upstream_is_silent() {
    // garden#803: silent upstream must not look idle downstream. With the
    // paused clock, the runtime auto-advances to the keepalive deadline as
    // soon as every task is blocked.
    let (mut upstream_tx, upstream_rx) = tokio::io::duplex(64 * 1024);
    let (downstream_tx, mut downstream_rx) = tokio::io::duplex(64 * 1024);

    let relay = tokio::spawn(async move {
        rewrite_sse_stream(
            BufReader::new(upstream_rx),
            downstream_tx,
            "gpt-5",
            &RelayConfig {
                keepalive_interval: Duration::from_secs(15),
            },
        )
        .await
    });

    // Nothing sent upstream: the first downstream bytes must be a keepalive.
    let mut buf = [0u8; 16];
    let n = downstream_rx.read(&mut buf).await.expect("read keepalive");
    assert_eq!(&buf[..n], b": ping\n\n");

    // End the stream and confirm the sentinel still arrives.
    upstream_tx
        .write_all(b"data: [DONE]\n\n")
        .await
        .expect("write done");
    drop(upstream_tx);

    let mut rest = String::new();
    downstream_rx
        .read_to_string(&mut rest)
        .await
        .expect("read rest");
    assert_eq!(rest, "data: [DONE]\n\n");
    relay.await.expect("join").expect("relay succeeds");
}

#[tokio::test(start_paused = true)]
async fn rewrite_does_not_keepalive_while_actively_streaming() {
    // All events are immediately available, so the idle deadline never wins.
    let input = sse(&[CREATED, TEXT_DELTA, COMPLETED, "[DONE]"]);
    let (output, result) = run_rewrite(&input).await;
    result.expect("relay succeeds");
    assert!(!output.contains(": ping"), "output: {output}");
}

// ---- pass-through relay --------------------------------------------------------

#[tokio::test]
async fn pass_through_copies_events_verbatim() {
    let input = sse(&[CREATED, COMPLETED]);
    let (output, result) = run_pass_through(&input).await;
    result.expect("relay succeeds");

    let frames = frames(&output);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0], format!("data: {CREATED}"));
    assert_eq!(frames[1], format!("data: {COMPLETED}"));
}

#[tokio::test]
async fn pass_through_relays_done_but_does_not_inject_it() {
    // Responses-API streams have no [DONE] sentinel: it is relayed when
    // upstream sends it, never injected at EOF.
    let (output, result) = run_pass_through(&sse(&[CREATED])).await;
    result.expect("relay succeeds");
    assert!(!output.contains("[DONE]"), "output: {output}");

    let (output, result) = run_pass_through(&sse(&[CREATED, "[DONE]"])).await;
    result.expect("relay succeeds");
    assert!(output.ends_with("data: [DONE]\n\n"), "output: {output}");
}

#[tokio::test]
async fn pass_through_reconstructs_multiline_events_with_per_line_data_prefix() {
    // A multi-line event must come back out as multiple `data:` lines in ONE
    // event (clients rejoin with \n) — not a single `data: ` prefix with a
    // raw embedded newline, which corrupts SSE framing (Go's behavior).
    let input = b"data: first\ndata: second\n\n";
    let (output, result) = run_pass_through(input).await;
    result.expect("relay succeeds");
    assert_eq!(output, "data: first\ndata: second\n\n");
}

#[tokio::test(start_paused = true)]
async fn zero_keepalive_interval_disables_pings_without_busy_looping() {
    // A zero interval must not turn the select! loop into a ping flood.
    let (mut upstream_tx, upstream_rx) = tokio::io::duplex(64 * 1024);
    let (downstream_tx, mut downstream_rx) = tokio::io::duplex(64 * 1024);

    let relay = tokio::spawn(async move {
        rewrite_sse_stream(
            BufReader::new(upstream_rx),
            downstream_tx,
            "gpt-5",
            &RelayConfig {
                keepalive_interval: Duration::ZERO,
            },
        )
        .await
    });

    upstream_tx
        .write_all(b"data: [DONE]\n\n")
        .await
        .expect("write done");
    drop(upstream_tx);

    let mut output = String::new();
    downstream_rx
        .read_to_string(&mut output)
        .await
        .expect("read output");
    assert_eq!(output, "data: [DONE]\n\n");
    assert!(!output.contains(": ping"));
    relay.await.expect("join").expect("relay succeeds");
}

#[tokio::test(start_paused = true)]
async fn pass_through_emits_keepalive_when_upstream_is_silent() {
    let (upstream_tx, upstream_rx) = tokio::io::duplex(64 * 1024);
    let (downstream_tx, mut downstream_rx) = tokio::io::duplex(64 * 1024);

    let relay = tokio::spawn(async move {
        pass_through_sse_stream(
            BufReader::new(upstream_rx),
            downstream_tx,
            &RelayConfig::default(),
        )
        .await
    });

    let mut buf = [0u8; 16];
    let n = downstream_rx.read(&mut buf).await.expect("read keepalive");
    assert_eq!(&buf[..n], b": ping\n\n");

    drop(upstream_tx);
    let mut rest = String::new();
    downstream_rx
        .read_to_string(&mut rest)
        .await
        .expect("read rest");
    assert_eq!(rest, "");
    relay.await.expect("join").expect("relay succeeds");
}
