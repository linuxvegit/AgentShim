//! Parse an SSE byte-stream from OpenAI into a CanonicalStream.

use std::collections::{HashSet, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};

use eventsource_stream::Eventsource;
use futures::stream::BoxStream;
use futures_core::Stream;

use agent_shim_core::{
    ContentBlockKind, MessageRole, ResponseId, StopReason, StreamError, StreamEvent, ToolCallId,
    Usage,
};

pub(crate) fn parse<S>(byte_stream: S) -> agent_shim_core::CanonicalStream
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    let source = byte_stream.eventsource();
    Box::pin(ChatSseParser {
        source: Box::pin(source),
        state: ParserState::default(),
        pending: VecDeque::new(),
        finalized: false,
    })
}

/// Hand-rolled `Stream` for the SSE → canonical translation. Owns its
/// state mutably (no `Arc<Mutex>` synchronisation that the previous
/// `flat_map` shape required to share state with the drain tail).
///
/// All fields are `Unpin`, including the boxed source (`Pin<Box<_>>` is
/// itself `Unpin`), so the impl can use `self.get_mut()` instead of any
/// unsafe pin projection.
struct ChatSseParser {
    source: BoxStream<
        'static,
        Result<eventsource_stream::Event, eventsource_stream::EventStreamError<reqwest::Error>>,
    >,
    state: ParserState,
    pending: VecDeque<Result<StreamEvent, StreamError>>,
    /// `true` once `state.finalize()` has been called on source EOF, so the
    /// drain runs exactly once before `Poll::Ready(None)` becomes terminal.
    finalized: bool,
}

impl Stream for ChatSseParser {
    type Item = Result<StreamEvent, StreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            // Drain any buffered output first. `parse_chunk` / `handle_done`
            // can produce 1..N canonical events per SSE input event.
            if let Some(ev) = this.pending.pop_front() {
                return Poll::Ready(Some(ev));
            }

            // Buffer empty — pull the next SSE event (or drive the drain).
            match this.source.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(event))) => {
                    let events = this.state.handle_event(&event);
                    this.pending.extend(events);
                    // Loop back to drain `pending`.
                }
                Poll::Ready(Some(Err(e))) => {
                    let events = this.state.handle_transport_error(e);
                    this.pending.extend(events);
                }
                Poll::Ready(None) => {
                    if !this.finalized {
                        this.finalized = true;
                        // `finalize` synthesises any missing lifecycle events
                        // so downstream encoders always see a well-formed
                        // envelope. Idempotent — returns Vec::new() when the
                        // upstream already drove the envelope to completion
                        // via `[DONE]` or `finish_reason`.
                        //
                        // Mirrors the drain-tail pattern used by
                        // `gemini::response::parse_streaming`.
                        let drain = this.state.finalize();
                        this.pending.extend(drain.into_iter().map(Ok));
                        // Loop back: emit the drain (if any), then on the
                        // next call the source's None drives a terminal None.
                    } else {
                        return Poll::Ready(None);
                    }
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[derive(Default)]
struct ParserState {
    emitted_response_start: bool,
    emitted_message_start: bool,
    emitted_message_stop: bool,
    emitted_response_stop: bool,
    text_block_open: bool,
    open_tool_blocks: HashSet<u32>,
    response_id: String,
    response_model: String,
    /// Tracks whether the upstream has signalled end-of-stream (`[DONE]`
    /// sentinel or a `finish_reason`). Many OpenAI-compatible providers
    /// (notably GitHub Copilot via its CloudFlare-fronted endpoint) close
    /// the underlying connection abruptly after `[DONE]`, which surfaces as
    /// a benign reqwest "error decoding response body" on the body stream.
    /// Once we've already emitted a clean termination we drop those
    /// transport errors instead of propagating them as
    /// `StreamError::Upstream` and noisy WARN logs.
    stream_completed: bool,
    /// [DEBUG-sse1] context for diagnosing why a transport error survives
    /// the `stream_completed` guard. Tracks how much SSE traffic the parser
    /// saw and what the last meaningful state was when the body stream
    /// broke. Kept as lasting diagnostic — when an upstream regresses, this
    /// gives operators enough context to triage without re-instrumenting.
    event_count: u64,
    total_data_bytes: u64,
    last_event_kind: &'static str,
    last_finish_reason: Option<String>,
}

impl ParserState {
    fn handle_transport_error(
        &mut self,
        e: eventsource_stream::EventStreamError<reqwest::Error>,
    ) -> Vec<Result<StreamEvent, StreamError>> {
        if self.stream_completed {
            tracing::debug!(
                error = %e,
                "ignoring transport error after upstream end-of-stream"
            );
            return Vec::new();
        }

        // Walk the error chain so we can tell apart
        // (a) our own `read_timeout` tripping vs (b) the upstream
        // closing mid-stream (hyper `IncompleteMessage`) vs
        // (c) raw IO errors. The top-level Display on the
        // `EventStreamError<reqwest::Error>` is just "Transport
        // error: error decoding response body" and hides which
        // of these actually happened.
        //
        // Quirks:
        //   - `EventStreamError` does NOT implement `source()`
        //     (its `Error` impl is the default empty one), so
        //     we have to hand-match `Transport(inner)` to reach
        //     the wrapped `reqwest::Error`.
        //   - `reqwest::Error::is_timeout()` only fires for
        //     `Kind::Timeout` (total-request timeout), NOT for
        //     `Kind::Body` + `io::Error(TimedOut)` which is how
        //     our `read_timeout` actually surfaces. We detect
        //     that case by walking the reqwest error's source
        //     chain and downcasting to `io::Error`.
        let mut error_chain: Vec<String> = Vec::new();
        let mut reqwest_flags: Option<(bool, bool, bool, bool, bool)> = None;
        if let eventsource_stream::EventStreamError::Transport(ref re) = e {
            let mut src: Option<&dyn std::error::Error> = Some(re);
            let mut io_timed_out = false;
            while let Some(err) = src {
                error_chain.push(err.to_string());
                if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
                    if io_err.kind() == std::io::ErrorKind::TimedOut {
                        io_timed_out = true;
                    }
                }
                src = err.source();
            }
            reqwest_flags = Some((
                re.is_timeout() || io_timed_out,
                re.is_connect(),
                re.is_body(),
                re.is_decode(),
                io_timed_out,
            ));
        } else {
            error_chain.push(e.to_string());
        }
        let (is_timeout, is_connect, is_body, is_decode, io_timed_out) =
            reqwest_flags.unwrap_or((false, false, false, false, false));
        tracing::warn!(
            error = %e,
            error_debug = ?e,
            error_chain = ?error_chain,
            is_timeout,
            is_connect,
            is_body,
            is_decode,
            io_timed_out,
            debug_tag = "DEBUG-sse1",
            event_count = self.event_count,
            total_data_bytes = self.total_data_bytes,
            last_event_kind = self.last_event_kind,
            last_finish_reason = ?self.last_finish_reason,
            emitted_response_start = self.emitted_response_start,
            emitted_message_start = self.emitted_message_start,
            text_block_open = self.text_block_open,
            open_tool_blocks = self.open_tool_blocks.len(),
            response_id = %self.response_id,
            response_model = %self.response_model,
            "SSE stream error"
        );
        vec![Err(StreamError::Upstream(e.to_string()))]
    }

    fn handle_event(
        &mut self,
        event: &eventsource_stream::Event,
    ) -> Vec<Result<StreamEvent, StreamError>> {
        self.event_count += 1;
        self.total_data_bytes += event.data.len() as u64;
        tracing::debug!(
            event_type = %event.event,
            data_len = event.data.len(),
            "SSE event received"
        );
        if event.data == "[DONE]" {
            self.handle_done()
                .into_iter()
                .map(Ok)
                .collect()
        } else {
            match self.parse_chunk(&event.data) {
                Ok(evts) => evts.into_iter().map(Ok).collect(),
                Err(e) => {
                    self.last_event_kind = "decode_error";
                    vec![Err(StreamError::Decode(e))]
                }
            }
        }
    }

    /// Handle the `[DONE]` sentinel. Closes any still-open blocks. Synthesises
    /// `MessageStart` and `MessageStop` if the upstream never sent them, so the
    /// canonical lifecycle envelope is always well-formed even when an
    /// upstream goes straight from "request acknowledged" to `[DONE]` without
    /// a `finish_reason` chunk (PR-B2: rule 2 violation observed in
    /// `done_before_any_delta_lifecycle_is_clean`).
    fn handle_done(&mut self) -> Vec<StreamEvent> {
        self.last_event_kind = "done";
        let mut evts = Vec::new();
        if !self.emitted_response_start {
            // No upstream chunk to copy id/model from — leave them as the
            // empty-string sentinels and let downstream encoders surface
            // them as `unknown`. We still need a ResponseStart to keep the
            // canonical envelope well-formed.
            self.emitted_response_start = true;
            evts.push(StreamEvent::ResponseStart {
                id: ResponseId(self.response_id.clone()),
                model: self.response_model.clone(),
                created_at_unix: 0,
            });
        }
        if !self.emitted_message_start {
            self.emitted_message_start = true;
            evts.push(StreamEvent::MessageStart {
                role: MessageRole::Assistant,
            });
        }
        // Close any open text block
        if self.text_block_open {
            evts.push(StreamEvent::ContentBlockStop { index: 0 });
            self.text_block_open = false;
        }
        // Close any open tool blocks (ToolCallStop precedes ContentBlockStop
        // per canonical lifecycle rule 4).
        for idx in self.open_tool_blocks.drain() {
            evts.push(StreamEvent::ToolCallStop { index: idx });
            evts.push(StreamEvent::ContentBlockStop { index: idx });
        }
        if !self.emitted_message_stop {
            self.emitted_message_stop = true;
            evts.push(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                stop_sequence: None,
            });
        }
        if !self.emitted_response_stop {
            self.emitted_response_stop = true;
            evts.push(StreamEvent::ResponseStop { usage: None });
        }
        self.stream_completed = true;
        evts
    }

    /// Called once after the upstream stream ends — for any reason, including
    /// abrupt disconnect without `[DONE]` or `finish_reason`. Synthesises any
    /// missing lifecycle events so downstream encoders always see a
    /// well-formed envelope (PR-B2: rule 1d violation observed in
    /// `content_then_silent_drop_lifecycle_is_clean`). Idempotent — if
    /// `[DONE]` or `finish_reason` already drove the envelope to completion,
    /// returns an empty vector.
    fn finalize(&mut self) -> Vec<StreamEvent> {
        if self.emitted_response_stop {
            return Vec::new();
        }

        let mut evts = Vec::new();
        if !self.emitted_response_start {
            // Stream broke before any successful chunk — nothing to close.
            // Emit nothing; downstream sees just whatever transport error
            // was already propagated, and the absence of a ResponseStart
            // tells encoders there is no envelope to finalize.
            return evts;
        }
        if !self.emitted_message_start {
            self.emitted_message_start = true;
            evts.push(StreamEvent::MessageStart {
                role: MessageRole::Assistant,
            });
        }
        if self.text_block_open {
            self.text_block_open = false;
            evts.push(StreamEvent::ContentBlockStop { index: 0 });
        }
        for idx in self.open_tool_blocks.drain() {
            evts.push(StreamEvent::ToolCallStop { index: idx });
            evts.push(StreamEvent::ContentBlockStop { index: idx });
        }
        if !self.emitted_message_stop {
            self.emitted_message_stop = true;
            evts.push(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                stop_sequence: None,
            });
        }
        self.emitted_response_stop = true;
        evts.push(StreamEvent::ResponseStop { usage: None });
        evts
    }

    fn parse_chunk(&mut self, data: &str) -> Result<Vec<StreamEvent>, String> {
        let v: serde_json::Value =
            serde_json::from_str(data).map_err(|e| format!("json parse: {e}"))?;

        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("upstream error")
                .to_string();
            self.last_event_kind = "upstream_error";
            return Ok(vec![StreamEvent::Error { message: msg }]);
        }

        self.last_event_kind = "delta";

        let mut events = Vec::new();

        let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("unknown");
        let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("unknown");
        let created = v.get("created").and_then(|x| x.as_u64()).unwrap_or(0);

        if !self.emitted_response_start {
            self.emitted_response_start = true;
            self.response_id = id.to_string();
            self.response_model = model.to_string();
            events.push(StreamEvent::ResponseStart {
                id: ResponseId(id.to_string()),
                model: model.to_string(),
                created_at_unix: created,
            });
        }

        let choices = match v.get("choices").and_then(|c| c.as_array()) {
            Some(c) => c,
            None => {
                if let Some(usage) = parse_usage(&v) {
                    events.push(StreamEvent::UsageDelta { usage });
                }
                return Ok(events);
            }
        };

        for choice in choices {
            // role → MessageStart (once)
            if let Some(delta) = choice.get("delta") {
                if let Some(_role) = delta.get("role").and_then(|r| r.as_str()) {
                    if !self.emitted_message_start {
                        self.emitted_message_start = true;
                        events.push(StreamEvent::MessageStart {
                            role: MessageRole::Assistant,
                        });
                    }
                }

                // content text delta
                if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
                    if !text.is_empty() {
                        if !self.emitted_message_start {
                            self.emitted_message_start = true;
                            events.push(StreamEvent::MessageStart {
                                role: MessageRole::Assistant,
                            });
                        }
                        if !self.text_block_open {
                            self.text_block_open = true;
                            events.push(StreamEvent::ContentBlockStart {
                                index: 0,
                                kind: ContentBlockKind::Text,
                            });
                        }
                        events.push(StreamEvent::TextDelta {
                            index: 0,
                            text: text.to_string(),
                        });
                    }
                }

                // tool_calls deltas
                if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    if !self.emitted_message_start {
                        self.emitted_message_start = true;
                        events.push(StreamEvent::MessageStart {
                            role: MessageRole::Assistant,
                        });
                    }
                    // Close text block if open before tool calls start
                    if self.text_block_open {
                        self.text_block_open = false;
                        events.push(StreamEvent::ContentBlockStop { index: 0 });
                    }

                    for tc in tool_calls {
                        let tc_index =
                            tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                        // Offset tool indices by 1 since text uses index 0
                        let block_index = tc_index + 1;

                        if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                            let name = tc
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            if !self.open_tool_blocks.contains(&block_index) {
                                self.open_tool_blocks.insert(block_index);
                                events.push(StreamEvent::ContentBlockStart {
                                    index: block_index,
                                    kind: ContentBlockKind::ToolCall,
                                });
                            }
                            events.push(StreamEvent::ToolCallStart {
                                index: block_index,
                                id: ToolCallId::from_provider(id),
                                name,
                            });
                        }

                        if let Some(args) = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|a| a.as_str())
                        {
                            if !args.is_empty() {
                                events.push(StreamEvent::ToolCallArgumentsDelta {
                                    index: block_index,
                                    json_fragment: args.to_string(),
                                });
                            }
                        }
                    }
                }
            }

            // finish_reason — close blocks and emit MessageStop
            if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                if !reason.is_empty() {
                    self.last_finish_reason = Some(reason.to_string());
                    self.last_event_kind = "finish";
                    if self.text_block_open {
                        self.text_block_open = false;
                        events.push(StreamEvent::ContentBlockStop { index: 0 });
                    }
                    for idx in self.open_tool_blocks.drain() {
                        events.push(StreamEvent::ToolCallStop { index: idx });
                        events.push(StreamEvent::ContentBlockStop { index: idx });
                    }
                    if !self.emitted_message_stop {
                        self.emitted_message_stop = true;
                        events.push(StreamEvent::MessageStop {
                            stop_reason: StopReason::from_provider_string(reason),
                            stop_sequence: None,
                        });
                    }
                    // Some providers (e.g. Copilot) close the connection right
                    // after the final choice without sending an explicit `[DONE]`
                    // sentinel. Mark the stream complete here so the caller drops
                    // the trailing transport error instead of WARNing on it.
                    self.stream_completed = true;
                }
            }
        }

        if let Some(usage) = v
            .get("usage")
            .filter(|u| !u.is_null())
            .and_then(|_| parse_usage(&v))
        {
            events.push(StreamEvent::UsageDelta { usage });
        }

        Ok(events)
    }
}

fn parse_usage(v: &serde_json::Value) -> Option<Usage> {
    let u = v.get("usage")?;
    Some(Usage {
        input_tokens: u
            .get("prompt_tokens")
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        output_tokens: u
            .get("completion_tokens")
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        cache_creation_input_tokens: u
            .get("cache_creation_input_tokens")
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        cache_read_input_tokens: u
            .get("cache_read_input_tokens")
            .or_else(|| {
                u.get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
            })
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serve a chunked HTTP/1.1 response that streams a single OpenAI-style
    /// chunk with `finish_reason=stop`, then `data: [DONE]`, and then drops the
    /// connection without sending the terminating `0\r\n\r\n` chunk. `reqwest`
    /// surfaces this as a body-decode transport error — exactly what we see
    /// from the GitHub Copilot upstream in production.
    #[tokio::test]
    async fn transport_error_after_done_is_dropped() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            // Drain the request headers up to the blank line so the client
            // doesn't see a connection reset mid-send.
            let mut buf = [0u8; 4096];
            let mut total = 0usize;
            loop {
                let n = socket.read(&mut buf[total..]).await.unwrap();
                if n == 0 {
                    break;
                }
                total += n;
                if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if total == buf.len() {
                    break;
                }
            }

            let header = b"HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Transfer-Encoding: chunked\r\n\
                \r\n";
            socket.write_all(header).await.unwrap();

            // Chunk 1: a final OpenAI-style delta with finish_reason=stop.
            let chunk1 = b"data: {\"id\":\"chatcmpl-1\",\"model\":\"copilot-test\",\"created\":1700000000,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n";
            let chunk1_frame = format!("{:x}\r\n", chunk1.len());
            socket.write_all(chunk1_frame.as_bytes()).await.unwrap();
            socket.write_all(chunk1).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();

            // Chunk 2: the [DONE] sentinel.
            let chunk2 = b"data: [DONE]\n\n";
            let chunk2_frame = format!("{:x}\r\n", chunk2.len());
            socket.write_all(chunk2_frame.as_bytes()).await.unwrap();
            socket.write_all(chunk2).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
            socket.flush().await.unwrap();

            // Deliberately drop the connection BEFORE the `0\r\n\r\n` chunk
            // terminator. reqwest will surface this as a transport error on
            // the body stream — same shape as the warning we are silencing.
            drop(socket);
        });

        let url = format!("http://{}/", addr);
        let response = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .expect("request should succeed");

        let stream = response.bytes_stream();
        let canonical = parse(stream);

        let collected: Vec<Result<StreamEvent, StreamError>> = canonical.collect().await;

        // No upstream error should have been propagated downstream.
        let errs: Vec<_> = collected.iter().filter_map(|r| r.as_ref().err()).collect();
        assert!(
            errs.is_empty(),
            "expected no StreamError after [DONE], got: {:?}",
            errs
        );

        // The clean termination still produced a ResponseStop.
        let response_stops: usize = collected
            .iter()
            .filter(|r| matches!(r, Ok(StreamEvent::ResponseStop { .. })))
            .count();
        assert_eq!(
            response_stops, 1,
            "expected exactly one ResponseStop event, got {response_stops}"
        );
    }
}
