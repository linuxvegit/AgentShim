use std::sync::Arc;
use std::time::Duration;

use agent_shim_core::{
    ids::ResponseId,
    stream::{ContentBlockKind, StreamEvent},
};
use bytes::Bytes;
use futures_util::{stream::BoxStream, StreamExt};
use parking_lot::Mutex;

use super::wire::{
    ContentBlockDelta, ContentBlockStartPayload, MessageDeltaPayload, MessageStartPayload,
    OutboundEvent, OutboundUsage,
};
use crate::sse;

/// Mutable state threaded through SSE encoding.
struct EncoderState {
    response_id: String,
    model: String,
    /// Tracks the content block kind for each index so we can emit the right delta type.
    block_kinds: Vec<ContentBlockKind>,
    input_tokens: u32,
    output_tokens: u32,
}

impl EncoderState {
    fn new() -> Self {
        Self {
            response_id: ResponseId::new().0,
            model: String::new(),
            block_kinds: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
        }
    }
}

fn serialize_event(ev: &OutboundEvent) -> Option<Bytes> {
    let name = match ev {
        OutboundEvent::MessageStart { .. } => "message_start",
        OutboundEvent::ContentBlockStart { .. } => "content_block_start",
        OutboundEvent::ContentBlockDelta { .. } => "content_block_delta",
        OutboundEvent::ContentBlockStop { .. } => "content_block_stop",
        OutboundEvent::MessageDelta { .. } => "message_delta",
        OutboundEvent::MessageStop => "message_stop",
        OutboundEvent::Ping => "ping",
        OutboundEvent::Error { .. } => "error",
    };
    let data = serde_json::to_string(ev).ok()?;
    Some(sse::event(name, &data))
}

pub fn encode(
    canonical: agent_shim_core::stream::CanonicalStream,
    keepalive: Option<Duration>,
) -> BoxStream<'static, Result<Bytes, crate::FrontendError>> {
    use std::sync::atomic::{AtomicBool, Ordering};

    let state = Arc::new(Mutex::new(EncoderState::new()));
    let done = Arc::new(AtomicBool::new(false));
    let done_for_flat_map = Arc::clone(&done);

    let event_stream = canonical.flat_map(move |item| {
        let state = Arc::clone(&state);
        let done = Arc::clone(&done_for_flat_map);
        let mut chunks: Vec<Result<Bytes, crate::FrontendError>> = Vec::new();

        let stream_event = match item {
            Ok(e) => e,
            Err(e) => {
                let ev = OutboundEvent::Error {
                    error: super::wire::ErrorPayload {
                        ty: "api_error".into(),
                        message: e.to_string(),
                    },
                };
                if let Some(b) = serialize_event(&ev) {
                    chunks.push(Ok(b));
                }
                return futures_util::stream::iter(chunks);
            }
        };

        match stream_event {
            StreamEvent::ResponseStart { id, model, .. } => {
                let mut s = state.lock();
                s.response_id = id.0.clone();
                s.model = model.clone();
                // Emit message_start with empty usage (updated later)
                let ev = OutboundEvent::MessageStart {
                    message: MessageStartPayload {
                        id: id.0,
                        ty: "message",
                        role: "assistant",
                        model,
                        usage: OutboundUsage::default(),
                    },
                };
                if let Some(b) = serialize_event(&ev) {
                    chunks.push(Ok(b));
                }
                // Emit a ping to signal start
                if let Some(b) = serialize_event(&OutboundEvent::Ping) {
                    chunks.push(Ok(b));
                }
            }

            StreamEvent::ContentBlockStart { index, kind } => {
                let mut s = state.lock();
                let idx = index as usize;
                if idx >= s.block_kinds.len() {
                    s.block_kinds.resize(idx + 1, ContentBlockKind::Text);
                }
                s.block_kinds[idx] = kind;

                if kind != ContentBlockKind::ToolCall {
                    let payload = match kind {
                        ContentBlockKind::Text => ContentBlockStartPayload::Text {
                            text: String::new(),
                        },
                        ContentBlockKind::Reasoning => ContentBlockStartPayload::Thinking {
                            thinking: String::new(),
                        },
                        ContentBlockKind::RedactedReasoning => {
                            ContentBlockStartPayload::RedactedThinking {
                                data: String::new(),
                            }
                        }
                        _ => ContentBlockStartPayload::Text {
                            text: String::new(),
                        },
                    };
                    let ev = OutboundEvent::ContentBlockStart {
                        index,
                        content_block: payload,
                    };
                    if let Some(b) = serialize_event(&ev) {
                        chunks.push(Ok(b));
                    }
                }
            }

            StreamEvent::ToolCallStart { index, id, name } => {
                let ev = OutboundEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlockStartPayload::ToolUse {
                        id: id.0,
                        name,
                        input: String::new(),
                    },
                };
                if let Some(b) = serialize_event(&ev) {
                    chunks.push(Ok(b));
                }
            }

            StreamEvent::TextDelta { index, text } => {
                let ev = OutboundEvent::ContentBlockDelta {
                    index,
                    delta: ContentBlockDelta::TextDelta { text },
                };
                if let Some(b) = serialize_event(&ev) {
                    chunks.push(Ok(b));
                }
            }

            StreamEvent::ReasoningDelta { index, text } => {
                let ev = OutboundEvent::ContentBlockDelta {
                    index,
                    delta: ContentBlockDelta::ThinkingDelta { thinking: text },
                };
                if let Some(b) = serialize_event(&ev) {
                    chunks.push(Ok(b));
                }
            }

            StreamEvent::ToolCallArgumentsDelta {
                index,
                json_fragment,
            } => {
                let ev = OutboundEvent::ContentBlockDelta {
                    index,
                    delta: ContentBlockDelta::InputJsonDelta {
                        partial_json: json_fragment,
                    },
                };
                if let Some(b) = serialize_event(&ev) {
                    chunks.push(Ok(b));
                }
            }

            StreamEvent::ToolCallStop { .. } => {
                // Absorbed — ContentBlockStop handles closing the block
            }

            StreamEvent::ContentBlockStop { index } => {
                let ev = OutboundEvent::ContentBlockStop { index };
                if let Some(b) = serialize_event(&ev) {
                    chunks.push(Ok(b));
                }
            }

            StreamEvent::UsageDelta { usage } => {
                let mut s = state.lock();
                s.input_tokens += usage.input_tokens.unwrap_or(0);
                s.output_tokens += usage.output_tokens.unwrap_or(0);
            }

            StreamEvent::MessageStop {
                stop_reason,
                stop_sequence,
            } => {
                use super::mapping::stop_reason_to_anthropic;
                let s = state.lock();
                let ev = OutboundEvent::MessageDelta {
                    delta: MessageDeltaPayload {
                        stop_reason: stop_reason_to_anthropic(&stop_reason).to_owned(),
                        stop_sequence,
                    },
                    usage: OutboundUsage {
                        input_tokens: s.input_tokens,
                        output_tokens: s.output_tokens,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                    },
                };
                drop(s);
                if let Some(b) = serialize_event(&ev) {
                    chunks.push(Ok(b));
                }
                if let Some(b) = serialize_event(&OutboundEvent::MessageStop) {
                    chunks.push(Ok(b));
                }
                done.store(true, Ordering::SeqCst);
                chunks.push(Ok(Bytes::new()));
            }

            StreamEvent::ResponseStop { usage } => {
                if let Some(u) = usage {
                    let mut s = state.lock();
                    if let Some(it) = u.input_tokens {
                        s.input_tokens = it;
                    }
                    if let Some(ot) = u.output_tokens {
                        s.output_tokens = ot;
                    }
                }
            }

            StreamEvent::Error { message } => {
                let ev = OutboundEvent::Error {
                    error: super::wire::ErrorPayload {
                        ty: "api_error".into(),
                        message,
                    },
                };
                if let Some(b) = serialize_event(&ev) {
                    chunks.push(Ok(b));
                }
            }

            StreamEvent::MessageStart { .. } | StreamEvent::RawProviderEvent(_) => {
                // Not re-emitted; MessageStart is synthetic from ResponseStart
            }
        }

        futures_util::stream::iter(chunks)
    });

    // Terminate the output stream after message_stop. The flat_map emits an
    // empty Bytes sentinel after the message_stop SSE event. scan() yields all
    // items up to (but not including) the sentinel, then returns None to end.
    let terminate_on_sentinel = |stream: BoxStream<
        'static,
        Result<Bytes, crate::FrontendError>,
    >|
     -> BoxStream<'static, Result<Bytes, crate::FrontendError>> {
        stream
            .scan((), |(), item| {
                let is_sentinel = matches!(&item, Ok(b) if b.is_empty());
                if is_sentinel {
                    futures::future::ready(None)
                } else {
                    futures::future::ready(Some(item))
                }
            })
            .boxed()
    };

    // Optionally interleave keepalive pings
    if let Some(interval) = keepalive {
        use tokio_stream::wrappers::IntervalStream;
        let done2 = Arc::clone(&done);
        let ping_stream = IntervalStream::new(tokio::time::interval(interval))
            .take_while(move |_| {
                let is_done = done2.load(std::sync::atomic::Ordering::SeqCst);
                futures::future::ready(!is_done)
            })
            .map(|_| Ok::<Bytes, crate::FrontendError>(sse::comment("ping")));

        let merged = futures_util::stream::select(event_stream.boxed(), ping_stream.boxed());
        terminate_on_sentinel(merged.boxed())
    } else {
        terminate_on_sentinel(event_stream.boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        ids::{ResponseId, ToolCallId},
        message::MessageRole,
        stream::CanonicalStream,
        StopReason, StreamError,
    };
    use futures::stream;

    async fn collect_stream(stream: CanonicalStream) -> String {
        let bytes = encode(stream, None)
            .fold(Vec::new(), |mut out, item| async move {
                out.extend_from_slice(&item.expect("stream item"));
                out
            })
            .await;
        String::from_utf8(bytes).expect("utf8 stream")
    }

    #[tokio::test]
    async fn tool_call_start_emits_single_content_block_start() {
        let events: Vec<Result<StreamEvent, StreamError>> = vec![
            Ok(StreamEvent::ResponseStart {
                id: ResponseId("msg_1".to_string()),
                model: "claude-test".to_string(),
                created_at_unix: 1,
            }),
            Ok(StreamEvent::MessageStart {
                role: MessageRole::Assistant,
            }),
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                kind: ContentBlockKind::ToolCall,
            }),
            Ok(StreamEvent::ToolCallStart {
                index: 0,
                id: ToolCallId::from_provider("call_1"),
                name: "search".to_string(),
            }),
            Ok(StreamEvent::ToolCallArgumentsDelta {
                index: 0,
                json_fragment: "{}".to_string(),
            }),
            Ok(StreamEvent::ToolCallStop { index: 0 }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::ToolUse,
                stop_sequence: None,
            }),
            Ok(StreamEvent::ResponseStop { usage: None }),
        ];

        let body = collect_stream(Box::pin(stream::iter(events))).await;

        assert_eq!(body.matches("event: content_block_start").count(), 1);
        assert!(body.contains("call_1"));
        assert!(body.contains("search"));
    }
}
