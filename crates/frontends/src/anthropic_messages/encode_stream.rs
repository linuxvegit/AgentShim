use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use agent_shim_core::{
    ids::ResponseId,
    stream::{CanonicalStream, ContentBlockKind, StreamEvent},
};
use bytes::Bytes;
use futures_util::{stream::BoxStream, Stream, StreamExt};

use super::wire::{
    ContentBlockDelta, ContentBlockStartPayload, MessageDeltaPayload, MessageStartPayload,
    OutboundEvent, OutboundUsage,
};
use crate::sse;

/// Mutable state owned by the encoder stream. No `Arc<Mutex<_>>` — the
/// hand-rolled `AnthropicEncoderStream` owns this directly, so every
/// `&mut self` borrow proves there is no concurrent access.
struct EncoderState {
    response_id: String,
    model: String,
    /// Tracks the content block kind for each index so we can emit the right delta type.
    block_kinds: Vec<ContentBlockKind>,
    input_tokens: u32,
    output_tokens: u32,
    cache_creation_input_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
    /// Deferred stop reason — we wait for ResponseStop to emit message_delta/message_stop
    /// so that final usage from the provider is included.
    pending_stop: Option<(agent_shim_core::usage::StopReason, Option<String>)>,
}

impl EncoderState {
    fn new() -> Self {
        Self {
            response_id: ResponseId::new().0,
            model: String::new(),
            block_kinds: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            pending_stop: None,
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

/// Hand-rolled `Stream` that turns a `CanonicalStream` into Anthropic
/// Messages SSE bytes.
///
/// Replaces the prior `flat_map` + `Arc<Mutex<state>>` + `Arc<AtomicBool>
/// done` + `Bytes::new()` sentinel + outer `scan()` terminator shape with a
/// single owning struct that:
///
/// - holds `EncoderState` by value (zero synchronisation),
/// - drains buffered output before pulling the source again,
/// - terminates itself the moment `MessageStop` is emitted (no
///   downstream `scan` needed; the wrapper handles `done` directly).
///
/// All fields are `Unpin` (`Pin<Box<_>>` is itself `Unpin`,
/// `EncoderState`/`VecDeque`/`bool` are `Unpin`), so `self.get_mut()` is
/// safe without pin projection.
struct AnthropicEncoderStream {
    source: CanonicalStream,
    state: EncoderState,
    pending: VecDeque<Result<Bytes, crate::FrontendError>>,
    /// `true` once `MessageStop` has been emitted; on the next poll the
    /// stream returns `Poll::Ready(None)` to terminate cleanly.
    done: bool,
}

impl AnthropicEncoderStream {
    fn new(source: CanonicalStream) -> Self {
        Self {
            source,
            state: EncoderState::new(),
            pending: VecDeque::new(),
            done: false,
        }
    }

    fn push(&mut self, ev: &OutboundEvent) {
        if let Some(b) = serialize_event(ev) {
            self.pending.push_back(Ok(b));
        }
    }

    /// Translate one canonical event into 0..N outbound SSE events,
    /// buffering them in `self.pending`. Returns `true` once the encoder
    /// has emitted `message_stop` and the stream should terminate.
    fn handle(&mut self, item: Result<StreamEvent, agent_shim_core::StreamError>) -> bool {
        let stream_event = match item {
            Ok(e) => e,
            Err(e) => {
                self.push(&OutboundEvent::Error {
                    error: super::wire::ErrorPayload {
                        ty: "api_error".into(),
                        message: e.to_string(),
                    },
                });
                return false;
            }
        };

        match stream_event {
            StreamEvent::ResponseStart { id, model, .. } => {
                self.state.response_id = id.0.clone();
                self.state.model = model.clone();
                // Emit message_start with empty usage (updated later)
                self.push(&OutboundEvent::MessageStart {
                    message: MessageStartPayload {
                        id: id.0,
                        ty: "message",
                        role: "assistant",
                        model,
                        usage: OutboundUsage::default(),
                    },
                });
                // Emit a ping to signal start
                self.push(&OutboundEvent::Ping);
            }

            StreamEvent::ContentBlockStart { index, kind } => {
                let idx = index as usize;
                if idx >= self.state.block_kinds.len() {
                    self.state
                        .block_kinds
                        .resize(idx + 1, ContentBlockKind::Text);
                }
                self.state.block_kinds[idx] = kind;

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
                    self.push(&OutboundEvent::ContentBlockStart {
                        index,
                        content_block: payload,
                    });
                }
            }

            StreamEvent::ToolCallStart { index, id, name } => {
                self.push(&OutboundEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlockStartPayload::ToolUse {
                        id: id.0,
                        name,
                        input: String::new(),
                    },
                });
            }

            StreamEvent::TextDelta { index, text } => {
                self.push(&OutboundEvent::ContentBlockDelta {
                    index,
                    delta: ContentBlockDelta::TextDelta { text },
                });
            }

            StreamEvent::ReasoningDelta { index, text } => {
                self.push(&OutboundEvent::ContentBlockDelta {
                    index,
                    delta: ContentBlockDelta::ThinkingDelta { thinking: text },
                });
            }

            StreamEvent::ToolCallArgumentsDelta {
                index,
                json_fragment,
            } => {
                self.push(&OutboundEvent::ContentBlockDelta {
                    index,
                    delta: ContentBlockDelta::InputJsonDelta {
                        partial_json: json_fragment,
                    },
                });
            }

            StreamEvent::ToolCallStop { .. } => {
                // Absorbed — ContentBlockStop handles closing the block
            }

            StreamEvent::ContentBlockStop { index } => {
                self.push(&OutboundEvent::ContentBlockStop { index });
            }

            StreamEvent::UsageDelta { usage } => {
                self.state.input_tokens += usage.input_tokens.unwrap_or(0);
                self.state.output_tokens += usage.output_tokens.unwrap_or(0);
                if let Some(v) = usage.cache_creation_input_tokens {
                    *self.state.cache_creation_input_tokens.get_or_insert(0) += v;
                }
                if let Some(v) = usage.cache_read_input_tokens {
                    *self.state.cache_read_input_tokens.get_or_insert(0) += v;
                }
            }

            StreamEvent::MessageStop {
                stop_reason,
                stop_sequence,
            } => {
                // Defer emission to ResponseStop so that any trailing UsageDelta
                // events (common with OpenAI-compatible backends) are captured.
                self.state.pending_stop = Some((stop_reason, stop_sequence));
            }

            StreamEvent::ResponseStop { usage } => {
                use super::mapping::stop_reason_to_anthropic;
                // Apply any final usage override from the provider.
                if let Some(u) = usage {
                    if let Some(it) = u.input_tokens {
                        self.state.input_tokens = it;
                    }
                    if let Some(ot) = u.output_tokens {
                        self.state.output_tokens = ot;
                    }
                    if let Some(v) = u.cache_creation_input_tokens {
                        self.state.cache_creation_input_tokens = Some(v);
                    }
                    if let Some(v) = u.cache_read_input_tokens {
                        self.state.cache_read_input_tokens = Some(v);
                    }
                }
                // Now emit the deferred message_delta + message_stop.
                if let Some((stop_reason, stop_sequence)) = self.state.pending_stop.take() {
                    self.push(&OutboundEvent::MessageDelta {
                        delta: MessageDeltaPayload {
                            stop_reason: stop_reason_to_anthropic(&stop_reason).to_owned(),
                            stop_sequence,
                        },
                        usage: OutboundUsage {
                            input_tokens: self.state.input_tokens,
                            output_tokens: self.state.output_tokens,
                            cache_creation_input_tokens: self.state.cache_creation_input_tokens,
                            cache_read_input_tokens: self.state.cache_read_input_tokens,
                        },
                    });
                    self.push(&OutboundEvent::MessageStop);
                    return true;
                }
            }

            StreamEvent::Error { message } => {
                self.push(&OutboundEvent::Error {
                    error: super::wire::ErrorPayload {
                        ty: "api_error".into(),
                        message,
                    },
                });
            }

            StreamEvent::MessageStart { .. } | StreamEvent::RawProviderEvent(_) => {
                // Not re-emitted; MessageStart is synthetic from ResponseStart
            }
        }

        false
    }
}

impl Stream for AnthropicEncoderStream {
    type Item = Result<Bytes, crate::FrontendError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            // Drain any buffered output first. `handle` can produce 0..N
            // SSE events per canonical input event.
            if let Some(ev) = this.pending.pop_front() {
                return Poll::Ready(Some(ev));
            }

            // No buffered output. If we already emitted message_stop on a
            // prior poll, this is the terminal call.
            if this.done {
                return Poll::Ready(None);
            }

            // Pull the next canonical event (or terminate on source EOF).
            match this.source.as_mut().poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    if this.handle(item) {
                        this.done = true;
                    }
                    // Loop back to drain `pending`.
                }
                Poll::Ready(None) => {
                    // Source ended without ResponseStop. Treat as terminal —
                    // the upstream lifecycle validator is responsible for
                    // flagging malformed canonical streams; this encoder
                    // surfaces whatever bytes it already emitted.
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Wraps an inner SSE byte stream with periodic keepalive comments.
///
/// Replaces the prior `Arc<AtomicBool> done` flag + `select` pattern with a
/// stream that observes the inner stream's own `Poll::Ready(None)` to stop
/// pinging. Side benefit: when the canonical stream terminates early (e.g.
/// upstream cancel before `ResponseStop`), the keepalive task now stops
/// promptly instead of waiting for the next interval tick.
pub fn encode(
    canonical: CanonicalStream,
    keepalive: Option<Duration>,
) -> BoxStream<'static, Result<Bytes, crate::FrontendError>> {
    let encoder = AnthropicEncoderStream::new(canonical);
    match keepalive {
        Some(period) => sse::KeepaliveStream::new(encoder, period).boxed(),
        None => encoder.boxed(),
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
        // PR-B3: assert the canonical input itself satisfies the lifecycle
        // contract — see `CONTEXT.md` "Canonical lifecycle". Buffer the
        // stream into a Vec first, validate, then re-wrap into a stream for
        // the encoder so the assertion happens before encoding.
        let buffered: Vec<Result<StreamEvent, StreamError>> = {
            use futures_util::StreamExt as _;
            stream.collect().await
        };
        let owned: Vec<StreamEvent> = buffered
            .iter()
            .map(|r| r.as_ref().expect("test fixture must be Ok").clone())
            .collect();
        agent_shim_protocol_tests::lifecycle::assert_canonical_lifecycle(&owned);
        let replay: CanonicalStream = Box::pin(stream::iter(buffered));
        let bytes = encode(replay, None)
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
