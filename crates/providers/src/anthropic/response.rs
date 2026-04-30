//! Parse Anthropic Messages responses (streaming SSE or unary JSON) into a
//! [`CanonicalStream`].
//!
//! Mirror image of `frontends::anthropic_messages::encode_stream::encode` and
//! `encode_unary::encode`. Used by the Anthropic provider's canonical path
//! when the inbound frontend is *not* `anthropic_messages` (passthrough is
//! preferred when shapes match).
//!
//! ## Lossy behavior in v0.2
//!
//! Anthropic's `signature_delta` events on thinking blocks carry a base64
//! signature that some downstream agents replay verbatim. The canonical
//! `Reasoning` content block doesn't carry a signature delta channel today,
//! so signature deltas observed during streaming are dropped (with a debug
//! log). This is acceptable because:
//!
//! 1. The passthrough path (`anthropic_messages` inbound) preserves these
//!    bytes byte-for-byte.
//! 2. Cross-protocol re-encoding (e.g. Anthropic → OpenAI Chat) cannot
//!    represent a thinking signature anyway; it would be lost regardless.
//!
//! Plan 04 may revisit this if a use case for cross-protocol signature
//! preservation emerges.

use eventsource_stream::Eventsource;
use futures::stream;
use futures::StreamExt;
use futures_core::Stream;

use agent_shim_core::{
    mapping::anthropic_wire::stop_reason_from_anthropic, CanonicalStream, ContentBlockKind,
    MessageRole, ResponseId, StopReason, StreamError, StreamEvent, ToolCallId, Usage,
};

use super::wire::{
    IncomingContentBlock, IncomingContentBlockDelta, IncomingContentBlockStart, IncomingEvent,
    IncomingMessagesResponse, IncomingUsage,
};

/// Parse the upstream SSE byte stream into a canonical event stream.
pub fn parse_stream<S>(byte_stream: S) -> CanonicalStream
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    let sse_stream = byte_stream.eventsource();

    let mut state = StreamState::default();

    let event_stream = sse_stream.flat_map(move |result| {
        let events: Vec<Result<StreamEvent, StreamError>> = match result {
            Err(e) => {
                tracing::warn!(error = %e, "anthropic SSE stream error");
                vec![Err(StreamError::Upstream(e.to_string()))]
            }
            Ok(event) => {
                tracing::debug!(
                    event_type = %event.event,
                    data_len = event.data.len(),
                    "anthropic SSE event"
                );
                match parse_chunk(&event.data, &mut state) {
                    Ok(evts) => evts.into_iter().map(Ok).collect(),
                    Err(e) => vec![Err(StreamError::Decode(e))],
                }
            }
        };
        stream::iter(events)
    });

    Box::pin(event_stream)
}

/// Parse a non-streaming Anthropic Messages response body into a synthetic
/// stream of canonical events that mirrors what `parse_stream` would have
/// emitted for an equivalent SSE response.
pub fn parse_unary(body: &[u8]) -> CanonicalStream {
    let events = match parse_unary_inner(body) {
        Ok(evts) => evts.into_iter().map(Ok).collect::<Vec<_>>(),
        Err(e) => vec![Err(StreamError::Decode(e))],
    };
    Box::pin(stream::iter(events))
}

// ── streaming ──────────────────────────────────────────────────────────────

#[derive(Default)]
struct StreamState {
    /// Stop reason captured from `message_delta` and held until `message_stop`
    /// to fire in the `MessageStop` canonical event.
    stop_reason: Option<StopReason>,
    stop_sequence: Option<String>,
    /// Accumulated usage from `message_start` (input) and `message_delta`
    /// (output / cache). Emitted once on `message_stop` as the final
    /// `ResponseStop { usage }`.
    accumulated_usage: Usage,
}

fn parse_chunk(data: &str, state: &mut StreamState) -> Result<Vec<StreamEvent>, String> {
    if data.trim().is_empty() {
        return Ok(Vec::new());
    }
    let event: IncomingEvent =
        serde_json::from_str(data).map_err(|e| format!("anthropic SSE json parse: {e}"))?;

    let mut out = Vec::new();
    match event {
        IncomingEvent::MessageStart { message } => {
            out.push(StreamEvent::ResponseStart {
                id: ResponseId(message.id),
                model: message.model,
                created_at_unix: 0,
            });
            out.push(StreamEvent::MessageStart {
                role: MessageRole::Assistant,
            });
            if let Some(usage) = message.usage {
                let mapped = map_usage(&usage);
                merge_usage(&mut state.accumulated_usage, &mapped);
                out.push(StreamEvent::UsageDelta { usage: mapped });
            }
        }
        IncomingEvent::ContentBlockStart {
            index,
            content_block,
        } => match content_block {
            IncomingContentBlockStart::Text { text } => {
                out.push(StreamEvent::ContentBlockStart {
                    index,
                    kind: ContentBlockKind::Text,
                });
                if !text.is_empty() {
                    out.push(StreamEvent::TextDelta { index, text });
                }
            }
            IncomingContentBlockStart::ToolUse { id, name, .. } => {
                out.push(StreamEvent::ContentBlockStart {
                    index,
                    kind: ContentBlockKind::ToolCall,
                });
                out.push(StreamEvent::ToolCallStart {
                    index,
                    id: ToolCallId::from_provider(id),
                    name,
                });
            }
            IncomingContentBlockStart::Thinking { thinking } => {
                out.push(StreamEvent::ContentBlockStart {
                    index,
                    kind: ContentBlockKind::Reasoning,
                });
                if !thinking.is_empty() {
                    out.push(StreamEvent::ReasoningDelta {
                        index,
                        text: thinking,
                    });
                }
            }
            IncomingContentBlockStart::RedactedThinking { .. } => {
                out.push(StreamEvent::ContentBlockStart {
                    index,
                    kind: ContentBlockKind::RedactedReasoning,
                });
            }
        },
        IncomingEvent::ContentBlockDelta { index, delta } => match delta {
            IncomingContentBlockDelta::TextDelta { text } => {
                out.push(StreamEvent::TextDelta { index, text });
            }
            IncomingContentBlockDelta::InputJsonDelta { partial_json } => {
                out.push(StreamEvent::ToolCallArgumentsDelta {
                    index,
                    json_fragment: partial_json,
                });
            }
            IncomingContentBlockDelta::ThinkingDelta { thinking } => {
                out.push(StreamEvent::ReasoningDelta {
                    index,
                    text: thinking,
                });
            }
            IncomingContentBlockDelta::SignatureDelta { .. } => {
                tracing::debug!(
                    "anthropic provider: dropping signature_delta in canonical path \
                     (lossy by design — see response.rs module doc)"
                );
            }
        },
        IncomingEvent::ContentBlockStop { index } => {
            out.push(StreamEvent::ContentBlockStop { index });
        }
        IncomingEvent::MessageDelta { delta, usage } => {
            if let Some(reason) = delta.stop_reason {
                state.stop_reason = Some(stop_reason_from_anthropic(&reason));
            }
            if let Some(seq) = delta.stop_sequence {
                state.stop_sequence = Some(seq);
            }
            if let Some(u) = usage {
                let mapped = map_usage(&u);
                merge_usage(&mut state.accumulated_usage, &mapped);
                out.push(StreamEvent::UsageDelta { usage: mapped });
            }
        }
        IncomingEvent::MessageStop => {
            out.push(StreamEvent::MessageStop {
                stop_reason: state.stop_reason.take().unwrap_or(StopReason::EndTurn),
                stop_sequence: state.stop_sequence.take(),
            });
            let final_usage = std::mem::take(&mut state.accumulated_usage);
            out.push(StreamEvent::ResponseStop {
                usage: Some(final_usage),
            });
        }
        IncomingEvent::Ping => {
            // No canonical equivalent — drop.
        }
        IncomingEvent::Error { error } => {
            out.push(StreamEvent::Error {
                message: format!("upstream {}: {}", error.ty, error.message),
            });
        }
    }

    Ok(out)
}

// ── unary ──────────────────────────────────────────────────────────────────

fn parse_unary_inner(body: &[u8]) -> Result<Vec<StreamEvent>, String> {
    // First check if this is an error envelope.
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        if let Some(err) = v.get("error") {
            let ty = err
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("api_error");
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("upstream error");
            return Ok(vec![StreamEvent::Error {
                message: format!("upstream {}: {}", ty, msg),
            }]);
        }
    }

    let resp: IncomingMessagesResponse =
        serde_json::from_slice(body).map_err(|e| format!("anthropic unary json parse: {e}"))?;

    let mut events = Vec::new();
    let usage = resp.usage.as_ref().map(map_usage).unwrap_or_default();

    events.push(StreamEvent::ResponseStart {
        id: ResponseId(resp.id),
        model: resp.model,
        created_at_unix: 0,
    });
    events.push(StreamEvent::MessageStart {
        role: MessageRole::Assistant,
    });
    if !is_usage_empty(&usage) {
        events.push(StreamEvent::UsageDelta {
            usage: usage.clone(),
        });
    }

    for (idx, block) in resp.content.into_iter().enumerate() {
        let index = idx as u32;
        match block {
            IncomingContentBlock::Text { text } => {
                events.push(StreamEvent::ContentBlockStart {
                    index,
                    kind: ContentBlockKind::Text,
                });
                if !text.is_empty() {
                    events.push(StreamEvent::TextDelta { index, text });
                }
                events.push(StreamEvent::ContentBlockStop { index });
            }
            IncomingContentBlock::ToolUse { id, name, input } => {
                events.push(StreamEvent::ContentBlockStart {
                    index,
                    kind: ContentBlockKind::ToolCall,
                });
                events.push(StreamEvent::ToolCallStart {
                    index,
                    id: ToolCallId::from_provider(id),
                    name,
                });
                let args_str = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                events.push(StreamEvent::ToolCallArgumentsDelta {
                    index,
                    json_fragment: args_str,
                });
                events.push(StreamEvent::ToolCallStop { index });
                events.push(StreamEvent::ContentBlockStop { index });
            }
            IncomingContentBlock::Thinking { thinking, .. } => {
                events.push(StreamEvent::ContentBlockStart {
                    index,
                    kind: ContentBlockKind::Reasoning,
                });
                if !thinking.is_empty() {
                    events.push(StreamEvent::ReasoningDelta {
                        index,
                        text: thinking,
                    });
                }
                events.push(StreamEvent::ContentBlockStop { index });
            }
            IncomingContentBlock::RedactedThinking { .. } => {
                events.push(StreamEvent::ContentBlockStart {
                    index,
                    kind: ContentBlockKind::RedactedReasoning,
                });
                events.push(StreamEvent::ContentBlockStop { index });
            }
        }
    }

    let stop_reason = resp
        .stop_reason
        .as_deref()
        .map(stop_reason_from_anthropic)
        .unwrap_or(StopReason::EndTurn);

    events.push(StreamEvent::MessageStop {
        stop_reason,
        stop_sequence: resp.stop_sequence,
    });
    events.push(StreamEvent::ResponseStop {
        usage: if is_usage_empty(&usage) {
            None
        } else {
            Some(usage)
        },
    });

    Ok(events)
}

// ── helpers ────────────────────────────────────────────────────────────────

fn map_usage(u: &IncomingUsage) -> Usage {
    Usage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_creation_input_tokens: u.cache_creation_input_tokens,
        cache_read_input_tokens: u.cache_read_input_tokens,
        ..Default::default()
    }
}

fn merge_usage(acc: &mut Usage, delta: &Usage) {
    if let Some(v) = delta.input_tokens {
        acc.input_tokens = Some(acc.input_tokens.unwrap_or(0).max(v));
    }
    if let Some(v) = delta.output_tokens {
        acc.output_tokens = Some(acc.output_tokens.unwrap_or(0).max(v));
    }
    if let Some(v) = delta.cache_creation_input_tokens {
        acc.cache_creation_input_tokens = Some(acc.cache_creation_input_tokens.unwrap_or(0).max(v));
    }
    if let Some(v) = delta.cache_read_input_tokens {
        acc.cache_read_input_tokens = Some(acc.cache_read_input_tokens.unwrap_or(0).max(v));
    }
}

fn is_usage_empty(u: &Usage) -> bool {
    u.input_tokens.is_none()
        && u.output_tokens.is_none()
        && u.cache_creation_input_tokens.is_none()
        && u.cache_read_input_tokens.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::stream;

    async fn collect(stream: CanonicalStream) -> Vec<StreamEvent> {
        stream
            .filter_map(|r| async move { r.ok() })
            .collect::<Vec<_>>()
            .await
    }

    fn sse_chunk(event: &str, data: &str) -> Bytes {
        Bytes::from(format!("event: {}\ndata: {}\n\n", event, data))
    }

    fn ok<T>(v: T) -> Result<T, reqwest::Error> {
        Ok(v)
    }

    #[tokio::test]
    async fn parse_stream_text_only() {
        let chunks: Vec<Result<Bytes, reqwest::Error>> = vec![
            ok(sse_chunk(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-test","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            )),
            ok(sse_chunk(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            )),
            ok(sse_chunk(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            )),
            ok(sse_chunk(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#,
            )),
            ok(sse_chunk(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            )),
            ok(sse_chunk(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
            )),
            ok(sse_chunk("message_stop", r#"{"type":"message_stop"}"#)),
        ];
        let s = stream::iter(chunks);
        let out = collect(parse_stream(s)).await;

        // Expect: ResponseStart, MessageStart, UsageDelta(input=10), ContentBlockStart,
        //         TextDelta("Hello"), TextDelta(" world"), ContentBlockStop,
        //         UsageDelta(output=5), MessageStop, ResponseStop
        assert!(matches!(out[0], StreamEvent::ResponseStart { .. }));
        assert!(matches!(
            out[1],
            StreamEvent::MessageStart {
                role: MessageRole::Assistant
            }
        ));
        let texts: Vec<&str> = out
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Hello", " world"]);

        let last = out.last().unwrap();
        match last {
            StreamEvent::ResponseStop { usage } => {
                let u = usage.as_ref().unwrap();
                assert_eq!(u.input_tokens, Some(10));
                assert_eq!(u.output_tokens, Some(5));
            }
            other => panic!("expected ResponseStop, got {:?}", other),
        }

        let stop = out
            .iter()
            .find(|e| matches!(e, StreamEvent::MessageStop { .. }))
            .unwrap();
        match stop {
            StreamEvent::MessageStop { stop_reason, .. } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn parse_stream_tool_use() {
        let chunks: Vec<Result<Bytes, reqwest::Error>> = vec![
            ok(sse_chunk(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-test"}}"#,
            )),
            ok(sse_chunk(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"search","input":{}}}"#,
            )),
            ok(sse_chunk(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}"#,
            )),
            ok(sse_chunk(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"rust\"}"}}"#,
            )),
            ok(sse_chunk(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            )),
            ok(sse_chunk(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            )),
            ok(sse_chunk("message_stop", r#"{"type":"message_stop"}"#)),
        ];
        let out = collect(parse_stream(stream::iter(chunks))).await;

        let tool_start = out
            .iter()
            .find(|e| matches!(e, StreamEvent::ToolCallStart { .. }));
        match tool_start {
            Some(StreamEvent::ToolCallStart { id, name, .. }) => {
                assert_eq!(id.0, "call_1");
                assert_eq!(name, "search");
            }
            other => panic!("expected ToolCallStart, got {:?}", other),
        }

        let frags: Vec<&str> = out
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallArgumentsDelta { json_fragment, .. } => {
                    Some(json_fragment.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(frags.join(""), r#"{"q":"rust"}"#);

        let stop = out
            .iter()
            .find(|e| matches!(e, StreamEvent::MessageStop { .. }))
            .unwrap();
        if let StreamEvent::MessageStop { stop_reason, .. } = stop {
            assert_eq!(*stop_reason, StopReason::ToolUse);
        }
    }

    #[tokio::test]
    async fn parse_stream_thinking_deltas() {
        let chunks: Vec<Result<Bytes, reqwest::Error>> = vec![
            ok(sse_chunk(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-test"}}"#,
            )),
            ok(sse_chunk(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            )),
            ok(sse_chunk(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm..."}}"#,
            )),
            ok(sse_chunk(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-abc"}}"#,
            )),
            ok(sse_chunk(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            )),
            ok(sse_chunk(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            )),
            ok(sse_chunk("message_stop", r#"{"type":"message_stop"}"#)),
        ];
        let out = collect(parse_stream(stream::iter(chunks))).await;

        let thinkings: Vec<&str> = out
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ReasoningDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinkings, vec!["hmm..."]);

        let starts: usize = out
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    StreamEvent::ContentBlockStart {
                        kind: ContentBlockKind::Reasoning,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(starts, 1);
    }

    #[tokio::test]
    async fn parse_stream_error_event_propagates() {
        let chunks: Vec<Result<Bytes, reqwest::Error>> = vec![ok(sse_chunk(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"too busy"}}"#,
        ))];
        let out = collect(parse_stream(stream::iter(chunks))).await;
        match out.first() {
            Some(StreamEvent::Error { message }) => {
                assert!(message.contains("overloaded_error"));
                assert!(message.contains("too busy"));
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn parse_stream_ping_is_dropped() {
        let chunks: Vec<Result<Bytes, reqwest::Error>> = vec![
            ok(sse_chunk("ping", r#"{"type":"ping"}"#)),
            ok(sse_chunk(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-test"}}"#,
            )),
            ok(sse_chunk("message_stop", r#"{"type":"message_stop"}"#)),
        ];
        let out = collect(parse_stream(stream::iter(chunks))).await;
        // Should have ResponseStart, MessageStart, MessageStop, ResponseStop — no Ping.
        assert!(out
            .iter()
            .any(|e| matches!(e, StreamEvent::ResponseStart { .. })));
        assert!(out
            .iter()
            .any(|e| matches!(e, StreamEvent::MessageStop { .. })));
    }

    // ── unary ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn parse_unary_text_response() {
        let body = br#"{
            "id": "msg_unary_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-test",
            "content": [
                {"type":"text","text":"Hello world"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 8, "output_tokens": 3}
        }"#;
        let out = collect(parse_unary(body)).await;
        assert!(matches!(
            out.first(),
            Some(StreamEvent::ResponseStart { .. })
        ));
        let texts: Vec<&str> = out
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Hello world"]);

        let stop = out
            .iter()
            .find(|e| matches!(e, StreamEvent::MessageStop { .. }))
            .unwrap();
        if let StreamEvent::MessageStop { stop_reason, .. } = stop {
            assert_eq!(*stop_reason, StopReason::EndTurn);
        }

        match out.last().unwrap() {
            StreamEvent::ResponseStop { usage } => {
                let u = usage.as_ref().unwrap();
                assert_eq!(u.input_tokens, Some(8));
                assert_eq!(u.output_tokens, Some(3));
            }
            other => panic!("expected ResponseStop, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn parse_unary_tool_use_response() {
        let body = br#"{
            "id": "msg_2",
            "type": "message",
            "role": "assistant",
            "model": "claude-test",
            "content": [
                {"type":"tool_use","id":"call_x","name":"search","input":{"q":"foo"}}
            ],
            "stop_reason": "tool_use"
        }"#;
        let out = collect(parse_unary(body)).await;
        let tool_start = out
            .iter()
            .find(|e| matches!(e, StreamEvent::ToolCallStart { .. }))
            .expect("tool call start");
        if let StreamEvent::ToolCallStart { id, name, .. } = tool_start {
            assert_eq!(id.0, "call_x");
            assert_eq!(name, "search");
        }
        let frag = out
            .iter()
            .find_map(|e| match e {
                StreamEvent::ToolCallArgumentsDelta { json_fragment, .. } => {
                    Some(json_fragment.as_str())
                }
                _ => None,
            })
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(frag).unwrap();
        assert_eq!(v["q"], "foo");

        let stop = out
            .iter()
            .find(|e| matches!(e, StreamEvent::MessageStop { .. }))
            .unwrap();
        if let StreamEvent::MessageStop { stop_reason, .. } = stop {
            assert_eq!(*stop_reason, StopReason::ToolUse);
        }
    }

    #[tokio::test]
    async fn parse_unary_error_envelope_propagates() {
        let body = br#"{
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "slow down"}
        }"#;
        let out = collect(parse_unary(body)).await;
        match out.first() {
            Some(StreamEvent::Error { message }) => {
                assert!(message.contains("rate_limit_error"));
                assert!(message.contains("slow down"));
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }
}
