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
//! 3. Routes `usage` payloads through [`super::usage::map_usage`] so DeepSeek's
//!    `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` fields land in
//!    the canonical `cache_read_input_tokens` and `input_tokens` slots.
//!
//! The clone-and-modify decision is documented in Plan 02 T4: extending the
//! cousin parser would have forced its OAI-Compat callers to thread a dynamic
//! index allocator through their happy path. The two parsers will likely
//! converge later, but only after Gemini lands in Plan 03 and we can see the
//! shared shape.
//!
//! `parse_unary` is similarly a clone-and-modify of
//! `oai_chat_wire::chat_unary_parser::parse` that swaps in [`map_usage`] for
//! the final `ResponseStop` event's usage.

use std::collections::HashMap;

use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures::stream;
use futures::StreamExt;
use futures_core::Stream;

use agent_shim_core::{
    CanonicalStream, ContentBlockKind, MessageRole, ResponseId, StopReason, StreamError,
    StreamEvent, ToolCallId,
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
                if state.completed {
                    tracing::debug!(
                        error = %e,
                        "ignoring deepseek transport error after upstream end-of-stream"
                    );
                    Vec::new()
                } else {
                    tracing::warn!(error = %e, "deepseek SSE stream error");
                    vec![Err(StreamError::Upstream(e.to_string()))]
                }
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
                    state.completed = true;
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
/// Cloned from `oai_chat_wire::chat_unary_parser::parse` and modified to
/// route the final `usage` block through [`super::usage::map_usage`] so
/// DeepSeek's prompt-cache hit/miss fields land in the canonical shape.
pub(crate) fn parse_unary(body: &[u8]) -> CanonicalStream {
    let events = match parse_unary_inner(body) {
        Ok(evts) => evts.into_iter().map(Ok).collect::<Vec<_>>(),
        Err(e) => vec![Err(StreamError::Decode(e))],
    };
    Box::pin(stream::iter(events))
}

fn parse_unary_inner(body: &[u8]) -> Result<Vec<StreamEvent>, String> {
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("json parse: {e}"))?;

    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("upstream error")
            .to_string();
        return Ok(vec![StreamEvent::Error { message: msg }]);
    }

    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown")
        .to_string();
    let model = v
        .get("model")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown")
        .to_string();
    let created = v.get("created").and_then(|x| x.as_u64()).unwrap_or(0);

    let mut events = Vec::new();

    events.push(StreamEvent::ResponseStart {
        id: ResponseId(id),
        model,
        created_at_unix: created,
    });
    events.push(StreamEvent::MessageStart {
        role: MessageRole::Assistant,
    });

    let choices = v
        .get("choices")
        .and_then(|c| c.as_array())
        .ok_or_else(|| "missing choices".to_string())?;

    let mut stop_reason = StopReason::EndTurn;
    let stop_sequence: Option<String> = None;
    let mut block_index: u32 = 0;

    for choice in choices {
        if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
            stop_reason = StopReason::from_provider_string(reason);
        }

        let message = match choice.get("message") {
            Some(m) => m,
            None => continue,
        };

        // Text content
        if let Some(text) = message.get("content").and_then(|c| c.as_str()) {
            if !text.is_empty() {
                events.push(StreamEvent::ContentBlockStart {
                    index: block_index,
                    kind: ContentBlockKind::Text,
                });
                events.push(StreamEvent::TextDelta {
                    index: block_index,
                    text: text.to_string(),
                });
                events.push(StreamEvent::ContentBlockStop { index: block_index });
                block_index += 1;
            }
        }

        // Tool calls
        if let Some(tool_calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tool_calls {
                let id = tc
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("call_unknown")
                    .to_string();
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let args_str = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                    .unwrap_or("{}")
                    .to_string();

                events.push(StreamEvent::ContentBlockStart {
                    index: block_index,
                    kind: ContentBlockKind::ToolCall,
                });
                events.push(StreamEvent::ToolCallStart {
                    index: block_index,
                    id: ToolCallId::from_provider(&id),
                    name: name.clone(),
                });
                events.push(StreamEvent::ToolCallArgumentsDelta {
                    index: block_index,
                    json_fragment: args_str.clone(),
                });
                events.push(StreamEvent::ToolCallStop { index: block_index });
                events.push(StreamEvent::ContentBlockStop { index: block_index });
                block_index += 1;
            }
        }
    }

    // Usage — DeepSeek-specific cache-hit/miss mapping.
    let usage = v.get("usage").map(super::usage::map_usage);

    events.push(StreamEvent::MessageStop {
        stop_reason,
        stop_sequence,
    });
    events.push(StreamEvent::ResponseStop { usage });

    Ok(events)
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
    /// Set once the upstream signals end-of-stream (`[DONE]` sentinel or a
    /// `finish_reason`). Lets the outer `flat_map` drop the trailing transport
    /// error that some upstreams emit when they close the connection right
    /// after the final SSE event.
    completed: bool,
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
            if let Some(usage_value) = v.get("usage").filter(|u| !u.is_null()) {
                events.push(StreamEvent::UsageDelta {
                    usage: super::usage::map_usage(usage_value),
                });
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
                // Mark completion so a trailing transport-level error from the
                // upstream closing the connection is treated as benign.
                state.completed = true;
            }
        }
    }

    if let Some(usage_value) = v.get("usage").filter(|u| !u.is_null()) {
        events.push(StreamEvent::UsageDelta {
            usage: super::usage::map_usage(usage_value),
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::Usage;
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
        let usage_json = json!({
            "prompt_tokens": 10,
            "completion_tokens": 5
        });
        let chunk = json!({
            "id": "chatcmpl-1",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_u64,
            "model": "deepseek-reasoner",
            "usage": usage_json,
        });
        let evts = parse_chunk(&chunk.to_string(), &mut state).unwrap();

        // First chunk also emits ResponseStart (no MessageStart because there's
        // no choices/delta). Then the usage delta. Without cache fields, the
        // canonical `input_tokens` falls back to `prompt_tokens`.
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
                        reasoning_tokens: None,
                        estimated: false,
                        provider_raw: Some(usage_json),
                    },
                },
            ]
        );
    }

    #[test]
    fn parse_chunk_usage_with_cache_fields_routes_through_map_usage() {
        // Final usage-only chunk carrying DeepSeek's prompt_cache fields.
        // The cache hit lands in cache_read_input_tokens, the cache miss
        // overrides prompt_tokens for input_tokens.
        let mut state = StreamState::default();
        let chunk = json!({
            "id": "chatcmpl-1",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_u64,
            "model": "deepseek-reasoner",
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "prompt_cache_hit_tokens": 80,
                "prompt_cache_miss_tokens": 20
            }
        });
        let evts = parse_chunk(&chunk.to_string(), &mut state).unwrap();

        let usage_event = evts
            .iter()
            .find_map(|e| match e {
                StreamEvent::UsageDelta { usage } => Some(usage.clone()),
                _ => None,
            })
            .expect("usage delta should be present");

        assert_eq!(usage_event.input_tokens, Some(20));
        assert_eq!(usage_event.output_tokens, Some(50));
        assert_eq!(usage_event.cache_read_input_tokens, Some(80));
        assert_eq!(usage_event.cache_creation_input_tokens, None);
        assert!(usage_event.provider_raw.is_some());
    }

    #[test]
    fn parse_chunk_inline_usage_alongside_choices_emits_usage_delta() {
        // DeepSeek streaming sometimes carries `usage` alongside the final
        // chunk's choices/finish_reason rather than as a separate event. The
        // parser should still route it through map_usage.
        let mut state = StreamState::default();
        let chunk = json!({
            "id": "chatcmpl-1",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_u64,
            "model": "deepseek-reasoner",
            "choices": [{
                "index": 0,
                "delta": { "role": "assistant", "content": "hi" },
                "finish_reason": "stop",
            }],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 1,
                "prompt_cache_hit_tokens": 4,
                "prompt_cache_miss_tokens": 1
            }
        });
        let evts = parse_chunk(&chunk.to_string(), &mut state).unwrap();

        let usage_event = evts
            .iter()
            .find_map(|e| match e {
                StreamEvent::UsageDelta { usage } => Some(usage.clone()),
                _ => None,
            })
            .expect("usage delta should be present");

        assert_eq!(usage_event.input_tokens, Some(1));
        assert_eq!(usage_event.cache_read_input_tokens, Some(4));
    }

    #[test]
    fn parse_unary_emits_canonical_event_sequence_with_cache_usage() {
        // Full unary response: id/model/choices + DeepSeek cache usage.
        // Verifies parse_unary routes the usage through map_usage and emits
        // the standard ResponseStart -> MessageStart -> ... -> ResponseStop
        // shape.
        let body = serde_json::to_vec(&json!({
            "id": "chatcmpl-unary-1",
            "object": "chat.completion",
            "created": 1_700_000_000_u64,
            "model": "deepseek-chat",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Hi there" },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 8,
                "prompt_cache_hit_tokens": 90,
                "prompt_cache_miss_tokens": 10
            }
        }))
        .unwrap();

        let events = futures::executor::block_on(async {
            use futures::StreamExt;
            parse_unary(&body)
                .collect::<Vec<Result<StreamEvent, StreamError>>>()
                .await
        });

        let events: Vec<StreamEvent> = events.into_iter().map(Result::unwrap).collect();

        // Sanity check the structural shape.
        assert!(matches!(events[0], StreamEvent::ResponseStart { .. }));
        assert!(matches!(events[1], StreamEvent::MessageStart { .. }));

        // The ResponseStop's usage carries DeepSeek cache fields.
        let stop_usage = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::ResponseStop { usage } => usage.clone(),
                _ => None,
            })
            .expect("ResponseStop should carry usage");

        assert_eq!(stop_usage.input_tokens, Some(10));
        assert_eq!(stop_usage.output_tokens, Some(8));
        assert_eq!(stop_usage.cache_read_input_tokens, Some(90));
        assert_eq!(stop_usage.cache_creation_input_tokens, None);
        assert!(stop_usage.provider_raw.is_some());
    }

    #[test]
    fn parse_unary_without_cache_falls_back_to_prompt_tokens() {
        // Older DeepSeek payloads without cache fields: input_tokens falls
        // back to prompt_tokens via map_usage.
        let body = serde_json::to_vec(&json!({
            "id": "chatcmpl-unary-2",
            "object": "chat.completion",
            "created": 1_700_000_000_u64,
            "model": "deepseek-chat",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "ok" },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 7,
                "completion_tokens": 1
            }
        }))
        .unwrap();

        let events = futures::executor::block_on(async {
            use futures::StreamExt;
            parse_unary(&body)
                .collect::<Vec<Result<StreamEvent, StreamError>>>()
                .await
        });
        let events: Vec<StreamEvent> = events.into_iter().map(Result::unwrap).collect();

        let stop_usage = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::ResponseStop { usage } => usage.clone(),
                _ => None,
            })
            .expect("ResponseStop should carry usage");

        assert_eq!(stop_usage.input_tokens, Some(7));
        assert_eq!(stop_usage.cache_read_input_tokens, None);
    }
}
