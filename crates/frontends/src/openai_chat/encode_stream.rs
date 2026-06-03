use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_shim_core::stream::{CanonicalStream, StreamEvent};
use bytes::Bytes;
use futures_util::{stream::BoxStream, Stream, StreamExt};

use super::mapping::finish_reason_from_canonical;
use super::wire::{
    ChoiceOut, ChunkOut, DeltaOut, ToolCallDeltaOut, ToolCallFunctionDeltaOut, UsageOut,
};
use crate::sse;

struct EncoderState {
    response_id: String,
    model: String,
    created: u64,
}

impl EncoderState {
    fn new(clock_override: Option<u64>) -> Self {
        let created = clock_override.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        });
        Self {
            response_id: String::new(),
            model: String::new(),
            created,
        }
    }
}

fn make_chunk(state: &EncoderState, delta: DeltaOut, finish_reason: Option<String>) -> Bytes {
    let chunk = ChunkOut {
        id: state.response_id.clone(),
        object: "chat.completion.chunk",
        created: state.created,
        model: state.model.clone(),
        choices: vec![ChoiceOut {
            index: 0,
            delta,
            finish_reason,
        }],
        usage: None,
    };
    let json = serde_json::to_string(&chunk).unwrap_or_default();
    sse::data_only(&json)
}

fn make_usage_chunk(state: &EncoderState, usage: &agent_shim_core::usage::Usage) -> Bytes {
    let chunk = ChunkOut {
        id: state.response_id.clone(),
        object: "chat.completion.chunk",
        created: state.created,
        model: state.model.clone(),
        choices: vec![],
        usage: Some(UsageOut {
            prompt_tokens: usage.input_tokens.unwrap_or(0),
            completion_tokens: usage.output_tokens.unwrap_or(0),
            total_tokens: usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0),
        }),
    };
    let json = serde_json::to_string(&chunk).unwrap_or_default();
    sse::data_only(&json)
}

/// Hand-rolled `Stream` for the canonical -> OpenAI Chat SSE translation.
///
/// Replaces the prior `flat_map` + `Arc<Mutex<EncoderState>>` +
/// `Arc<AtomicBool> done` + `Bytes::new()` sentinel + outer `scan()`
/// terminator shape with a single owning struct (same template as
/// `chat_sse_parser::ChatSseParser` and
/// `anthropic_messages::encode_stream::AnthropicEncoderStream`).
struct OaiChatEncoderStream {
    source: CanonicalStream,
    state: EncoderState,
    clock_override: Option<u64>,
    pending: VecDeque<Result<Bytes, crate::FrontendError>>,
    /// `true` once `[DONE]` has been emitted; the next poll returns
    /// `Poll::Ready(None)`.
    done: bool,
}

impl OaiChatEncoderStream {
    fn new(source: CanonicalStream, clock_override: Option<u64>) -> Self {
        Self {
            source,
            state: EncoderState::new(clock_override),
            clock_override,
            pending: VecDeque::new(),
            done: false,
        }
    }

    fn push_bytes(&mut self, b: Bytes) {
        self.pending.push_back(Ok(b));
    }

    /// Process one canonical event into 0..N outbound chunks. Returns `true`
    /// once `[DONE]` has been emitted and the stream should terminate.
    fn handle(&mut self, item: Result<StreamEvent, agent_shim_core::StreamError>) -> bool {
        let stream_event = match item {
            Ok(e) => e,
            Err(e) => {
                // Emit a data chunk with error info then [DONE], then terminate.
                let err_json = serde_json::json!({ "error": { "message": e.to_string() } });
                let s = serde_json::to_string(&err_json).unwrap_or_default();
                self.push_bytes(sse::data_only(&s));
                self.push_bytes(Bytes::from("data: [DONE]\n\n"));
                return true;
            }
        };

        match stream_event {
            StreamEvent::ResponseStart {
                id,
                model,
                created_at_unix,
            } => {
                self.state.response_id = id.0;
                self.state.model = model;
                // If no clock_override was set, use the event's timestamp for consistency
                if self.clock_override.is_none() {
                    self.state.created = created_at_unix;
                }
                // Emit role chunk
                let delta = DeltaOut {
                    role: Some("assistant"),
                    ..Default::default()
                };
                let b = make_chunk(&self.state, delta, None);
                self.push_bytes(b);
            }

            StreamEvent::MessageStart { .. } => {
                // Handled via ResponseStart
            }

            StreamEvent::ContentBlockStart { .. } => {
                // No direct OpenAI equivalent; tool call start handles it
            }

            StreamEvent::ToolCallStart { index, id, name } => {
                let delta = DeltaOut {
                    tool_calls: vec![ToolCallDeltaOut {
                        index,
                        id: Some(id.0),
                        ty: Some("function"),
                        function: ToolCallFunctionDeltaOut {
                            name: Some(name),
                            arguments: Some(String::new()),
                        },
                    }],
                    ..Default::default()
                };
                let b = make_chunk(&self.state, delta, None);
                self.push_bytes(b);
            }

            StreamEvent::TextDelta { text, .. } => {
                let delta = DeltaOut {
                    content: Some(text),
                    ..Default::default()
                };
                let b = make_chunk(&self.state, delta, None);
                self.push_bytes(b);
            }

            StreamEvent::ReasoningDelta { .. } => {
                // OpenAI does not have a canonical reasoning delta field; skip
            }

            StreamEvent::ToolCallArgumentsDelta {
                index,
                json_fragment,
            } => {
                let delta = DeltaOut {
                    tool_calls: vec![ToolCallDeltaOut {
                        index,
                        id: None,
                        ty: None,
                        function: ToolCallFunctionDeltaOut {
                            name: None,
                            arguments: Some(json_fragment),
                        },
                    }],
                    ..Default::default()
                };
                let b = make_chunk(&self.state, delta, None);
                self.push_bytes(b);
            }

            StreamEvent::ToolCallStop { .. } | StreamEvent::ContentBlockStop { .. } => {
                // No per-block stop in OpenAI streaming
            }

            StreamEvent::UsageDelta { .. } => {
                // Accumulation handled at ResponseStop
            }

            StreamEvent::MessageStop { stop_reason, .. } => {
                let finish = finish_reason_from_canonical(&stop_reason).to_owned();
                let delta = DeltaOut::default();
                let b = make_chunk(&self.state, delta, Some(finish));
                self.push_bytes(b);
            }

            StreamEvent::ResponseStop { usage } => {
                if let Some(u) = usage {
                    let b = make_usage_chunk(&self.state, &u);
                    self.push_bytes(b);
                }
                self.push_bytes(Bytes::from("data: [DONE]\n\n"));
                return true;
            }

            StreamEvent::Error { message } => {
                let err_json = serde_json::json!({ "error": { "message": message } });
                let s = serde_json::to_string(&err_json).unwrap_or_default();
                self.push_bytes(sse::data_only(&s));
            }

            StreamEvent::RawProviderEvent(_) => {}
        }

        false
    }
}

impl Stream for OaiChatEncoderStream {
    type Item = Result<Bytes, crate::FrontendError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(ev) = this.pending.pop_front() {
                return Poll::Ready(Some(ev));
            }
            if this.done {
                return Poll::Ready(None);
            }
            match this.source.as_mut().poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    if this.handle(item) {
                        this.done = true;
                    }
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

pub fn encode(
    canonical: CanonicalStream,
    keepalive: Option<Duration>,
    clock_override: Option<u64>,
) -> BoxStream<'static, Result<Bytes, crate::FrontendError>> {
    let encoder = OaiChatEncoderStream::new(canonical, clock_override);
    match keepalive {
        Some(period) => sse::KeepaliveStream::new(encoder, period).boxed(),
        None => encoder.boxed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        ids::ResponseId, message::MessageRole, stream::CanonicalStream, StopReason, StreamError,
    };
    use futures::stream;

    fn fake_events() -> Vec<Result<StreamEvent, StreamError>> {
        let events = vec![
            Ok(StreamEvent::ResponseStart {
                id: ResponseId("resp_1".to_string()),
                model: "gpt-4o".to_string(),
                created_at_unix: 1_700_000_000,
            }),
            Ok(StreamEvent::MessageStart {
                role: MessageRole::Assistant,
            }),
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                kind: agent_shim_core::ContentBlockKind::Text,
            }),
            Ok(StreamEvent::TextDelta {
                index: 0,
                text: "hi".to_string(),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                stop_sequence: None,
            }),
            Ok(StreamEvent::ResponseStop { usage: None }),
        ];
        // PR-B3: assert the fixture itself satisfies the canonical lifecycle
        // contract — see `CONTEXT.md` "Canonical lifecycle". An encoder test
        // that feeds an illegal fixture would be testing a scenario the
        // encoder is allowed to handle however it wants; this anchors all
        // encoder behaviour against well-formed input.
        let owned: Vec<StreamEvent> = events.iter().map(|r| r.as_ref().unwrap().clone()).collect();
        agent_shim_protocol_tests::lifecycle::assert_canonical_lifecycle(&owned);
        events
    }

    /// Regression: with keepalive enabled, the encoded SSE stream MUST end
    /// after `data: [DONE]\n\n`. Before the self-terminating encoder + new
    /// KeepaliveStream landed, the ping stream was infinite and `select`
    /// never closed the body, so clients hung on "waiting for reply" after
    /// the model finished.
    #[tokio::test]
    async fn stream_terminates_after_done_with_keepalive() {
        let canonical: CanonicalStream = Box::pin(stream::iter(fake_events()));
        // Pick a keepalive longer than the test deadline so a ping cannot
        // mask a missing terminator. The test would hang without the fix.
        let encoded = encode(canonical, Some(Duration::from_secs(60)), Some(1));

        let body: Vec<u8> = tokio::time::timeout(Duration::from_secs(2), async {
            encoded
                .fold(Vec::new(), |mut out, item| async move {
                    out.extend_from_slice(&item.expect("stream item"));
                    out
                })
                .await
        })
        .await
        .expect("encoded stream must terminate after [DONE]");

        let text = std::str::from_utf8(&body).expect("utf8 body");
        assert!(text.contains("data: [DONE]"), "missing [DONE]: {text}");
        assert!(
            text.trim_end().ends_with("data: [DONE]"),
            "stream should end immediately after [DONE], got tail: {:?}",
            &text[text.len().saturating_sub(80)..]
        );
    }

    #[tokio::test]
    async fn stream_terminates_after_done_without_keepalive() {
        let canonical: CanonicalStream = Box::pin(stream::iter(fake_events()));
        let encoded = encode(canonical, None, Some(1));

        let body: Vec<u8> = tokio::time::timeout(Duration::from_secs(2), async {
            encoded
                .fold(Vec::new(), |mut out, item| async move {
                    out.extend_from_slice(&item.expect("stream item"));
                    out
                })
                .await
        })
        .await
        .expect("encoded stream must terminate after [DONE]");

        let text = std::str::from_utf8(&body).expect("utf8 body");
        assert!(text.contains("data: [DONE]"), "missing [DONE]: {text}");
    }
}
