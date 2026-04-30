//! Parse DeepSeek `/chat/completions` responses into a `CanonicalStream`.
//!
//! DeepSeek-R1 (`deepseek-reasoner`) interleaves `reasoning_content` and
//! `content` deltas in the same SSE stream. This parser is a clone-and-modify
//! of `oai_chat_wire::chat_sse_parser` that:
//!
//! 1. Routes both reasoning and text deltas through the
//!    [`ReasoningInterleaver`] state machine so block boundaries flip cleanly
//!    when upstream switches kinds.
//! 2. Allocates tool-call block indices AFTER the interleaver's text/reasoning
//!    blocks via [`ReasoningInterleaver::next_index`], rather than the cousin
//!    parser's fixed `tc.index + 1` offset (which assumes text always lives at
//!    index 0).
//!
//! The clone-and-modify decision is documented in Plan 02 T4: extending the
//! cousin parser would have forced its OAI-Compat callers to thread a dynamic
//! index allocator through their happy path. The two parsers will likely
//! converge later, but only after Gemini lands in Plan 03 and we can see the
//! shared shape.
//!
//! `parse_unary` continues to delegate to `oai_chat_wire::chat_unary_parser`;
//! T5 will swap in DeepSeek's cache-token usage mapping there.

use std::collections::HashMap;

use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use futures_core::Stream;

use agent_shim_core::{
    CanonicalStream, ContentBlockKind, MessageRole, ResponseId, StopReason, StreamError,
    StreamEvent, ToolCallId, Usage,
};

use crate::oai_chat_wire::interleaved_reasoning::{DeltaKind, ReasoningInterleaver};

/// Parse a streaming SSE response from DeepSeek into a `CanonicalStream`.
pub(crate) fn parse_stream<S>(byte_stream: S) -> CanonicalStream
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let sse_stream = byte_stream.eventsource();
    let mut state = StreamState::default();

    let event_stream = sse_stream.flat_map(move |result| {
        let events: Vec<Result<StreamEvent, StreamError>> = match result {
            Err(e) => {
                tracing::warn!(error = %e, "deepseek SSE stream error");
                vec![Err(StreamError::Upstream(e.to_string()))]
            }
            Ok(event) => {
                tracing::debug!(
                    event_type = %event.event,
                    data_len = event.data.len(),
                    "deepseek SSE event received"
                );
                if event.data == "[DONE]" {
                    let mut evts = Vec::new();
                    // Defensive flush — `finish_reason` should already have
                    // closed everything, but a broken stream might skip it.
                    state.interleaver.flush(&mut evts);
                    drain_open_tool_blocks(&mut state.open_tool_blocks, &mut evts);
                    evts.push(StreamEvent::ResponseStop { usage: None });
                    evts.into_iter().map(Ok).collect()
                } else {
                    match parse_chunk(&event.data, &mut state) {
                        Ok(evts) => evts.into_iter().map(Ok).collect(),
                        Err(e) => vec![Err(StreamError::Decode(e))],
                    }
                }
            }
        };
        futures::stream::iter(events)
    });

    Box::pin(event_stream)
}

/// Parse a non-streaming JSON response from DeepSeek into a `CanonicalStream`.
///
/// T3: delegates to the shared OAI-Chat unary parser.
/// T5: will add a cache-usage mapping step on top of the canonical events.
pub(crate) fn parse_unary(body: &[u8]) -> CanonicalStream {
    crate::oai_chat_wire::chat_unary_parser::parse(body)
}

#[derive(Default)]
struct StreamState {
    emitted_response_start: bool,
    emitted_message_start: bool,
    interleaver: ReasoningInterleaver,
    /// Map from upstream `tool_calls[].index` (often 0, 1, 2...) to the
    /// canonical block index allocated for that tool call. The allocator
    /// advances past the interleaver's blocks: the first new tool block lands
    /// at `interleaver.next_index() + open_tool_blocks.len()` — i.e. AFTER any
    /// text/reasoning blocks the interleaver has owned so far.
    open_tool_blocks: HashMap<u32, u32>,
    response_id: String,
    response_model: String,
}

fn parse_chunk(data: &str, state: &mut StreamState) -> Result<Vec<StreamEvent>, String> {
    let v: serde_json::Value =
        serde_json::from_str(data).map_err(|e| format!("json parse: {e}"))?;

    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("upstream error")
            .to_string();
        return Ok(vec![StreamEvent::Error { message: msg }]);
    }

    let mut events = Vec::new();

    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("unknown");
    let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("unknown");
    let created = v.get("created").and_then(|x| x.as_u64()).unwrap_or(0);

    if !state.emitted_response_start {
        state.emitted_response_start = true;
        state.response_id = id.to_string();
        state.response_model = model.to_string();
        events.push(StreamEvent::ResponseStart {
            id: ResponseId(id.to_string()),
            model: model.to_string(),
            created_at_unix: created,
        });
    }

    let choices = match v.get("choices").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => {
            // Some chunks (e.g. final usage-only chunk) have no choices array.
            if let Some(usage) = parse_usage(&v) {
                events.push(StreamEvent::UsageDelta { usage });
            }
            return Ok(events);
        }
    };

    for choice in choices {
        if let Some(delta) = choice.get("delta") {
            // role → MessageStart (once)
            if delta.get("role").and_then(|r| r.as_str()).is_some() {
                ensure_message_start(state, &mut events);
            }

            // reasoning_content delta → interleaver(Reasoning, ...)
            if let Some(text) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    ensure_message_start(state, &mut events);
                    state
                        .interleaver
                        .push(DeltaKind::Reasoning, text, &mut events);
                }
            }

            // content delta → interleaver(Text, ...)
            if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    ensure_message_start(state, &mut events);
                    state.interleaver.push(DeltaKind::Text, text, &mut events);
                }
            }

            // tool_calls deltas — close the interleaver first (tool calls
            // are a hard block boundary; same posture as the text-block-close
            // branch in `oai_chat_wire::chat_sse_parser::parse_chunk` before
            // tool_calls are processed), then allocate canonical tool block
            // indices AFTER the interleaver's blocks.
            if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                ensure_message_start(state, &mut events);
                state.interleaver.flush(&mut events);

                for tc in tool_calls {
                    let provider_idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;

                    let block_index = match state.open_tool_blocks.get(&provider_idx) {
                        Some(&idx) => idx,
                        None => {
                            // Allocate a new canonical index strictly AFTER
                            // the interleaver's blocks plus any tool blocks
                            // already opened in this stream.
                            let allocated = state.interleaver.next_index()
                                + state.open_tool_blocks.len() as u32;
                            state.open_tool_blocks.insert(provider_idx, allocated);
                            events.push(StreamEvent::ContentBlockStart {
                                index: allocated,
                                kind: ContentBlockKind::ToolCall,
                            });
                            allocated
                        }
                    };

                    if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
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

        // finish_reason — close everything, emit MessageStop.
        if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
            if !reason.is_empty() {
                state.interleaver.flush(&mut events);
                drain_open_tool_blocks(&mut state.open_tool_blocks, &mut events);
                events.push(StreamEvent::MessageStop {
                    stop_reason: StopReason::from_provider_string(reason),
                    stop_sequence: None,
                });
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

fn ensure_message_start(state: &mut StreamState, events: &mut Vec<StreamEvent>) {
    if !state.emitted_message_start {
        state.emitted_message_start = true;
        events.push(StreamEvent::MessageStart {
            role: MessageRole::Assistant,
        });
    }
}

/// Close any tool blocks left open in `open_tool_blocks` and clear the map.
/// Sorts the canonical indices for deterministic event ordering — useful for
/// snapshot tests and cross-platform reproducibility.
fn drain_open_tool_blocks(open_tool_blocks: &mut HashMap<u32, u32>, events: &mut Vec<StreamEvent>) {
    let mut indices: Vec<u32> = open_tool_blocks.values().copied().collect();
    indices.sort_unstable();
    for idx in indices {
        events.push(StreamEvent::ToolCallStop { index: idx });
        events.push(StreamEvent::ContentBlockStop { index: idx });
    }
    open_tool_blocks.clear();
}

fn parse_usage(v: &serde_json::Value) -> Option<Usage> {
    // Mirrors `oai_chat_wire::chat_sse_parser::parse_usage` for now. T5 will
    // route DeepSeek unary parsing through `deepseek::usage::map_usage`, which
    // adds DeepSeek-specific cache-hit/cache-miss field handling. Keeping this
    // in lock-step until then avoids divergent usage shapes between DeepSeek
    // and the OAI-Compat path.
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
    use serde_json::json;

    fn run_chunks(chunks: &[serde_json::Value]) -> Vec<StreamEvent> {
        let mut state = StreamState::default();
        let mut all_events = Vec::new();
        for chunk in chunks {
            let evts = parse_chunk(&chunk.to_string(), &mut state).unwrap();
            all_events.extend(evts);
        }
        all_events
    }

    fn run_chunk(chunk: serde_json::Value) -> Vec<StreamEvent> {
        run_chunks(&[chunk])
    }

    fn base_chunk(delta: serde_json::Value, finish_reason: serde_json::Value) -> serde_json::Value {
        json!({
            "id": "chatcmpl-1",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_u64,
            "model": "deepseek-reasoner",
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }]
        })
    }

    #[test]
    fn parse_chunk_text_only_emits_text_block() {
        let evts = run_chunk(base_chunk(
            json!({ "role": "assistant", "content": "Hello" }),
            json!(null),
        ));

        assert_eq!(
            evts,
            vec![
                StreamEvent::ResponseStart {
                    id: ResponseId("chatcmpl-1".into()),
                    model: "deepseek-reasoner".into(),
                    created_at_unix: 1_700_000_000,
                },
                StreamEvent::MessageStart {
                    role: MessageRole::Assistant,
                },
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Text,
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "Hello".into(),
                },
            ]
        );
    }

    #[test]
    fn parse_chunk_reasoning_only_emits_reasoning_block() {
        let evts = run_chunk(base_chunk(
            json!({ "role": "assistant", "reasoning_content": "Thinking" }),
            json!(null),
        ));

        assert_eq!(
            evts,
            vec![
                StreamEvent::ResponseStart {
                    id: ResponseId("chatcmpl-1".into()),
                    model: "deepseek-reasoner".into(),
                    created_at_unix: 1_700_000_000,
                },
                StreamEvent::MessageStart {
                    role: MessageRole::Assistant,
                },
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Reasoning,
                },
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "Thinking".into(),
                },
            ]
        );
    }

    #[test]
    fn parse_chunk_reasoning_then_text_transitions() {
        let evts = run_chunks(&[
            base_chunk(
                json!({ "role": "assistant", "reasoning_content": "Hmm" }),
                json!(null),
            ),
            base_chunk(json!({ "content": "Answer" }), json!(null)),
        ]);

        // Drop the leading ResponseStart + MessageStart (tested above) and
        // assert the structural transition.
        let tail: Vec<_> = evts
            .iter()
            .filter(|e| {
                !matches!(
                    e,
                    StreamEvent::ResponseStart { .. } | StreamEvent::MessageStart { .. }
                )
            })
            .cloned()
            .collect();

        assert_eq!(
            tail,
            vec![
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Reasoning,
                },
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "Hmm".into(),
                },
                StreamEvent::ContentBlockStop { index: 0 },
                StreamEvent::ContentBlockStart {
                    index: 1,
                    kind: ContentBlockKind::Text,
                },
                StreamEvent::TextDelta {
                    index: 1,
                    text: "Answer".into(),
                },
            ]
        );
    }

    #[test]
    fn parse_chunk_text_with_tool_call_allocates_after_interleaver() {
        let evts = run_chunk(base_chunk(
            json!({
                "role": "assistant",
                "content": "Calling...",
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": {
                        "name": "get_weather",
                        "arguments": "{\"city\":"
                    }
                }]
            }),
            json!(null),
        ));

        let tail: Vec<_> = evts
            .iter()
            .filter(|e| {
                !matches!(
                    e,
                    StreamEvent::ResponseStart { .. } | StreamEvent::MessageStart { .. }
                )
            })
            .cloned()
            .collect();

        assert_eq!(
            tail,
            vec![
                // Text block from interleaver at index 0.
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Text,
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "Calling...".into(),
                },
                // Tool call processing flushes the interleaver, then opens a
                // tool block at index 1 (next_index() == 1, no other tool
                // blocks open yet).
                StreamEvent::ContentBlockStop { index: 0 },
                StreamEvent::ContentBlockStart {
                    index: 1,
                    kind: ContentBlockKind::ToolCall,
                },
                StreamEvent::ToolCallStart {
                    index: 1,
                    id: ToolCallId::from_provider("call_1"),
                    name: "get_weather".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 1,
                    json_fragment: "{\"city\":".into(),
                },
            ]
        );
    }

    #[test]
    fn parse_chunk_finish_reason_flushes_and_emits_message_stop() {
        let evts = run_chunks(&[
            base_chunk(
                json!({ "role": "assistant", "content": "Done." }),
                json!(null),
            ),
            base_chunk(json!({}), json!("stop")),
        ]);

        // Just look at the events from the second chunk onward — the close.
        // The first chunk contributed: ResponseStart, MessageStart,
        // ContentBlockStart{0,Text}, TextDelta{0,"Done."}.
        assert_eq!(
            &evts[evts.len() - 2..],
            &[
                StreamEvent::ContentBlockStop { index: 0 },
                StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                    stop_sequence: None,
                },
            ]
        );
    }

    #[test]
    fn parse_chunk_finish_reason_with_tool_use_uses_tooluse_stop_reason() {
        let evts = run_chunks(&[
            base_chunk(
                json!({
                    "role": "assistant",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": { "name": "f", "arguments": "" }
                    }]
                }),
                json!(null),
            ),
            base_chunk(json!({}), json!("tool_calls")),
        ]);

        // The final three events should close the open tool block and emit
        // MessageStop with StopReason::ToolUse.
        assert_eq!(
            &evts[evts.len() - 3..],
            &[
                StreamEvent::ToolCallStop { index: 0 },
                StreamEvent::ContentBlockStop { index: 0 },
                StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                    stop_sequence: None,
                },
            ]
        );
    }

    #[test]
    fn parse_chunk_handles_error_envelope() {
        let mut state = StreamState::default();
        let chunk = json!({ "error": { "message": "rate limited" } });
        let evts = parse_chunk(&chunk.to_string(), &mut state).unwrap();
        assert_eq!(
            evts,
            vec![StreamEvent::Error {
                message: "rate limited".into(),
            }]
        );
    }

    #[test]
    fn parse_chunk_usage_only_chunk_emits_usage_delta() {
        let mut state = StreamState::default();
        let chunk = json!({
            "id": "chatcmpl-1",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_u64,
            "model": "deepseek-reasoner",
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5
            }
        });
        let evts = parse_chunk(&chunk.to_string(), &mut state).unwrap();

        // First chunk also emits ResponseStart (no MessageStart because there's
        // no choices/delta). Then the usage delta.
        assert_eq!(
            evts,
            vec![
                StreamEvent::ResponseStart {
                    id: ResponseId("chatcmpl-1".into()),
                    model: "deepseek-reasoner".into(),
                    created_at_unix: 1_700_000_000,
                },
                StreamEvent::UsageDelta {
                    usage: Usage {
                        input_tokens: Some(10),
                        output_tokens: Some(5),
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                        ..Default::default()
                    },
                },
            ]
        );
    }
}
