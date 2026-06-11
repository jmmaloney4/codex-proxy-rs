//! Async SSE relay: reads an upstream SSE stream, transforms it through
//! [`SSETransformer`], and writes a downstream SSE stream with keepalive and
//! guaranteed termination.
//!
//! This is the Phase 2 layer from ADR 002 — the production fix for
//! `jmmaloney4/garden` #796 (silent stream truncation) and #803 (idle-gap
//! timeouts). It ports the *framing and event-loop* behavior of Go
//! `RewriteSSEStreamWithCallback` and `PassThroughSSEStream`
//! (`internal/server/transform.go:1225-1394`) while fixing, by design, the
//! defects ADR 002 documents:
//!
//! - **Guaranteed `data: [DONE]\n\n`** as the final frame of every rewrite
//!   relay, on success *and* error. On error a structured
//!   `data: {"error":"stream error: <msg>"}` frame precedes the sentinel.
//! - **Keepalive comments** (`: ping\n\n`) when nothing has been written
//!   downstream for [`RelayConfig::keepalive_interval`] (default 15s),
//!   implemented as an idle deadline on last-write per the ADR 002 amendment —
//!   it fires even when the upstream is completely silent.
//! - **No token-size cap.** Lines are read with a growable buffer; Go's 10MB
//!   `bufio.Scanner` limit does not exist here.
//!
//! ## Intentional divergences from Go
//!
//! - The Go rewrite loop keeps reading after an upstream `[DONE]` and will
//!   happily emit frames *after* the sentinel. ADR 002 requires `[DONE]` to be
//!   the last frame, so this relay stops at `Done`.
//! - Go's relay caller (`server.go:645-655`) returns without a sentinel on
//!   error — the garden#796 bug. This relay always terminates the stream.
//! - The pass-through relay gains keepalive but does **not** inject `[DONE]`
//!   or error frames: it serves Responses-API-shaped streams, which have no
//!   `[DONE]` sentinel, so injecting one would corrupt the protocol.

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{Instant, sleep_until};

use crate::transform::{SSETransformer, TransformError, TransformResult};

/// Relay-layer configuration.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// Emit a `: ping\n\n` SSE comment when nothing has been written
    /// downstream for this long. ADR 002 default: 15 seconds.
    pub keepalive_interval: std::time::Duration,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            keepalive_interval: std::time::Duration::from_secs(15),
        }
    }
}

/// Errors surfaced by the relay loops. For the rewrite relay, the downstream
/// stream has already been terminated with an error frame and `[DONE]` by the
/// time `UpstreamRead`/`Transform` is returned (best effort); `DownstreamWrite`
/// means the client is gone and nothing more can be written.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("upstream read error: {0}")]
    UpstreamRead(#[source] std::io::Error),
    #[error("downstream write error: {0}")]
    DownstreamWrite(#[source] std::io::Error),
    #[error("transform error: {0}")]
    Transform(#[from] TransformError),
}

/// Incremental SSE event reader. Accumulates `data:` lines until a blank line
/// (or EOF) closes the event, then yields the lines joined with `\n` — the
/// same framing as the Go relay's `flushEvent`. Comment lines (`:`) and other
/// fields (`event:`, `id:`) are ignored, matching Go.
///
/// `next_event` is cancel-safe: a partially read line persists in `self.line`
/// across a dropped call (`read_until` appends into the caller's buffer), so
/// the keepalive `select!` branch in the relay loops cannot lose data.
struct SseEventReader<R> {
    reader: R,
    line: Vec<u8>,
    data_lines: Vec<Vec<u8>>,
    eof: bool,
}

impl<R: AsyncBufRead + Unpin> SseEventReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            line: Vec::new(),
            data_lines: Vec::new(),
            eof: false,
        }
    }

    /// Yield the next complete SSE event's joined data payload, or `None` at
    /// end of stream. A trailing event without a terminating blank line is
    /// flushed at EOF, matching Go.
    async fn next_event(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            if self.eof {
                return Ok(self.take_pending_event());
            }
            let n = self.reader.read_until(b'\n', &mut self.line).await?;
            if n == 0 || !self.line.ends_with(b"\n") {
                // EOF — possibly with a final unterminated line (which a
                // cancelled call may also have left behind when n == 0).
                self.eof = true;
                if !self.line.is_empty() {
                    let line = self.take_line();
                    self.process_line(&line);
                }
                continue;
            }
            let mut line = self.take_line();
            line.pop();
            if self.process_line(&line)
                && let Some(event) = self.take_pending_event()
            {
                return Ok(Some(event));
            }
        }
    }

    fn take_line(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.line)
    }

    /// Process one line (after the caller pops the `\n`); returns `true` when
    /// the line is an event boundary (blank line). Strips one trailing `\r`,
    /// like Go's `bufio.ScanLines`.
    fn process_line(&mut self, line: &[u8]) -> bool {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.trim_ascii().is_empty() {
            return true;
        }
        // Comments before fields, matching the Go loop's order.
        if line.starts_with(b":") {
            return false;
        }
        if let Some(payload) = line.strip_prefix(b"data:") {
            // SSE spec allows one optional space after the colon.
            let payload = payload.strip_prefix(b" ").unwrap_or(payload);
            self.data_lines.push(payload.to_vec());
        }
        false
    }

    fn take_pending_event(&mut self) -> Option<Vec<u8>> {
        if self.data_lines.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.data_lines).join(&b'\n'))
    }
}

/// Write one `data: <payload>\n\n` frame and flush.
async fn write_data_frame<W: AsyncWrite + Unpin>(
    downstream: &mut W,
    payload: &[u8],
) -> std::io::Result<()> {
    downstream.write_all(b"data: ").await?;
    downstream.write_all(payload).await?;
    downstream.write_all(b"\n\n").await?;
    downstream.flush().await
}

/// Best-effort error frame + `[DONE]` sentinel. Failures are ignored — this
/// runs on paths where the stream is already failing.
async fn terminate_with_error<W: AsyncWrite + Unpin>(downstream: &mut W, message: &str) {
    let payload = json!({ "error": format!("stream error: {message}") });
    if let Ok(bytes) = serde_json::to_vec(&payload) {
        let _ = write_data_frame(downstream, &bytes).await;
    }
    let _ = write_data_frame(downstream, b"[DONE]").await;
}

fn is_openai_chunk(value: &Value) -> bool {
    value.get("object").and_then(Value::as_str) == Some("chat.completion.chunk")
}

/// Rewrite an upstream Codex/Responses SSE stream into OpenAI chat-completion
/// chunks. See the module docs for termination and keepalive guarantees.
pub async fn rewrite_sse_stream<R, W>(
    upstream: R,
    downstream: W,
    model: &str,
    config: &RelayConfig,
) -> Result<(), RelayError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    rewrite_sse_stream_with_callback(upstream, downstream, model, config, |_, _| {}).await
}

/// [`rewrite_sse_stream`] with a per-event observer hook, the port of Go's
/// `onEvent` debug callback. Called with the raw event payload and the
/// transform result before frames are written. Not called on transform errors
/// — those are fully visible in the returned [`RelayError`].
pub async fn rewrite_sse_stream_with_callback<R, W, F>(
    upstream: R,
    mut downstream: W,
    model: &str,
    config: &RelayConfig,
    mut on_event: F,
) -> Result<(), RelayError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
    F: FnMut(&[u8], &TransformResult),
{
    let mut reader = SseEventReader::new(upstream);
    let mut transformer = SSETransformer::new(model);
    let mut last_write = Instant::now();

    loop {
        let event = tokio::select! {
            result = reader.next_event() => match result {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(e) => {
                    terminate_with_error(&mut downstream, &e.to_string()).await;
                    return Err(RelayError::UpstreamRead(e));
                }
            },
            _ = sleep_until(last_write + config.keepalive_interval) => {
                downstream
                    .write_all(b": ping\n\n")
                    .await
                    .map_err(RelayError::DownstreamWrite)?;
                downstream.flush().await.map_err(RelayError::DownstreamWrite)?;
                last_write = Instant::now();
                continue;
            }
        };

        match transformer.transform(&event) {
            Ok(result) => {
                on_event(&event, &result);
                match result {
                    TransformResult::Done => {
                        // ADR 002: [DONE] is the final frame; stop reading.
                        // (Go keeps relaying post-[DONE] events — see module
                        // docs.)
                        write_data_frame(&mut downstream, b"[DONE]")
                            .await
                            .map_err(RelayError::DownstreamWrite)?;
                        return Ok(());
                    }
                    TransformResult::Emitted(frames) => {
                        for frame in frames {
                            write_data_frame(&mut downstream, &frame)
                                .await
                                .map_err(RelayError::DownstreamWrite)?;
                        }
                        last_write = Instant::now();
                    }
                    TransformResult::Swallowed => {
                        if pass_through_openai_chunk(&mut downstream, &event)
                            .await
                            .map_err(RelayError::DownstreamWrite)?
                        {
                            last_write = Instant::now();
                        }
                    }
                }
            }
            Err(err) => {
                // Go parity: the Go transformer reads `type` out of a loose
                // map, so valid JSON without a string `type` is an *unknown
                // event* (swallowed, then probed for OpenAI-chunk
                // pass-through), not an error. Only invalid JSON — or a typed
                // payload failure on a recognized event — aborts the stream.
                match serde_json::from_slice::<Value>(&event) {
                    Ok(value) if value.get("type").and_then(Value::as_str).is_none() => {
                        if is_openai_chunk(&value) {
                            write_data_frame(&mut downstream, event.trim_ascii())
                                .await
                                .map_err(RelayError::DownstreamWrite)?;
                            last_write = Instant::now();
                        }
                    }
                    _ => {
                        terminate_with_error(&mut downstream, &err.to_string()).await;
                        return Err(err.into());
                    }
                }
            }
        }
    }

    // Upstream ended without an explicit [DONE]: emit the sentinel anyway —
    // the garden#796 fix.
    write_data_frame(&mut downstream, b"[DONE]")
        .await
        .map_err(RelayError::DownstreamWrite)
}

/// Pass-through pass for swallowed events: Go probes the raw payload and
/// relays OpenAI chunks unchanged. Returns whether a frame was written.
async fn pass_through_openai_chunk<W: AsyncWrite + Unpin>(
    downstream: &mut W,
    event: &[u8],
) -> std::io::Result<bool> {
    let trimmed = event.trim_ascii();
    if let Ok(value) = serde_json::from_slice::<Value>(trimmed)
        && is_openai_chunk(&value)
    {
        write_data_frame(downstream, trimmed).await?;
        return Ok(true);
    }
    Ok(false)
}

/// Copy upstream SSE events downstream without transformation, port of Go
/// `PassThroughSSEStream` plus keepalive. Serves Responses-API streams, which
/// have no `[DONE]` sentinel — so unlike the rewrite relay, nothing is
/// injected at EOF or on error.
pub async fn pass_through_sse_stream<R, W>(
    upstream: R,
    mut downstream: W,
    config: &RelayConfig,
) -> Result<(), RelayError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = SseEventReader::new(upstream);
    let mut last_write = Instant::now();

    loop {
        let event = tokio::select! {
            result = reader.next_event() => match result {
                Ok(Some(event)) => event,
                Ok(None) => return Ok(()),
                Err(e) => return Err(RelayError::UpstreamRead(e)),
            },
            _ = sleep_until(last_write + config.keepalive_interval) => {
                downstream
                    .write_all(b": ping\n\n")
                    .await
                    .map_err(RelayError::DownstreamWrite)?;
                downstream.flush().await.map_err(RelayError::DownstreamWrite)?;
                last_write = Instant::now();
                continue;
            }
        };

        let trimmed = event.trim_ascii();
        let payload: &[u8] = if trimmed == b"[DONE]" {
            b"[DONE]"
        } else {
            &event
        };
        // Go skips empty payloads (`len(raw) > 0`).
        if payload.is_empty() {
            continue;
        }
        write_data_frame(&mut downstream, payload)
            .await
            .map_err(RelayError::DownstreamWrite)?;
        last_write = Instant::now();
    }
}
