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
    let done_for_stream = Arc::clone(&done);

    let event_stream = canonical.flat_map(move |item| {
        let state = Arc::clone(&state);
        let done = Arc::clone(&done_for_stream);
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

                let payload = match kind {
                    ContentBlockKind::Text => ContentBlockStartPayload::Text {
                        text: String::new(),
                    },
                    ContentBlockKind::ToolCall => {
                        // We don't know id/name until ToolCallStart; emit a placeholder
                        ContentBlockStartPayload::ToolUse {
                            id: String::new(),
                            name: String::new(),
                            input: String::new(),
                        }
                    }
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

            StreamEvent::ToolCallStart { index, id, name } => {
                // Emit a content_block_start for the tool_use block with real id/name
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

    // Optionally interleave keepalive pings — stop once message_stop is emitted
    if let Some(interval) = keepalive {
        use tokio_stream::wrappers::IntervalStream;
        let done2 = done;
        let ping_stream = IntervalStream::new(tokio::time::interval(interval))
            .take_while(move |_| {
                let is_done = done2.load(std::sync::atomic::Ordering::SeqCst);
                futures::future::ready(!is_done)
            })
            .map(|_| Ok::<Bytes, crate::FrontendError>(sse::comment("ping")));

        let merged = futures_util::stream::select(event_stream, ping_stream);
        merged.boxed()
    } else {
        event_stream.boxed()
    }
}
