use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_shim_core::stream::StreamEvent;
use bytes::Bytes;
use futures_util::{stream::BoxStream, StreamExt};
use parking_lot::Mutex;

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

pub fn encode(
    canonical: agent_shim_core::stream::CanonicalStream,
    keepalive: Option<Duration>,
    clock_override: Option<u64>,
) -> BoxStream<'static, Result<Bytes, crate::FrontendError>> {
    use std::sync::atomic::{AtomicBool, Ordering};

    let state = Arc::new(Mutex::new(EncoderState::new(clock_override)));
    let done = Arc::new(AtomicBool::new(false));
    let done_for_flat_map = Arc::clone(&done);

    let event_stream = canonical.flat_map(move |item| {
        let state = Arc::clone(&state);
        let done = Arc::clone(&done_for_flat_map);
        let mut chunks: Vec<Result<Bytes, crate::FrontendError>> = Vec::new();

        let stream_event = match item {
            Ok(e) => e,
            Err(e) => {
                // Emit a data chunk with error info then [DONE]
                let err_json = serde_json::json!({ "error": { "message": e.to_string() } });
                let s = serde_json::to_string(&err_json).unwrap_or_default();
                chunks.push(Ok(sse::data_only(&s)));
                chunks.push(Ok(Bytes::from("data: [DONE]\n\n")));
                // Signal termination: stop keepalive pings and trip the
                // terminate_on_sentinel scan downstream so the HTTP body
                // closes (otherwise clients hang waiting for EOF).
                done.store(true, Ordering::SeqCst);
                chunks.push(Ok(Bytes::new()));
                return futures_util::stream::iter(chunks);
            }
        };

        match stream_event {
            StreamEvent::ResponseStart {
                id,
                model,
                created_at_unix,
            } => {
                let mut s = state.lock();
                s.response_id = id.0;
                s.model = model;
                // If no clock_override was set, use the event's timestamp for consistency
                if clock_override.is_none() {
                    s.created = created_at_unix;
                }
                // Emit role chunk
                let delta = DeltaOut {
                    role: Some("assistant"),
                    ..Default::default()
                };
                chunks.push(Ok(make_chunk(&s, delta, None)));
            }

            StreamEvent::MessageStart { .. } => {
                // Handled via ResponseStart
            }

            StreamEvent::ContentBlockStart { .. } => {
                // No direct OpenAI equivalent; tool call start handles it
            }

            StreamEvent::ToolCallStart { index, id, name } => {
                let s = state.lock();
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
                chunks.push(Ok(make_chunk(&s, delta, None)));
            }

            StreamEvent::TextDelta { text, .. } => {
                let s = state.lock();
                let delta = DeltaOut {
                    content: Some(text),
                    ..Default::default()
                };
                chunks.push(Ok(make_chunk(&s, delta, None)));
            }

            StreamEvent::ReasoningDelta { .. } => {
                // OpenAI does not have a canonical reasoning delta field; skip
            }

            StreamEvent::ToolCallArgumentsDelta {
                index,
                json_fragment,
            } => {
                let s = state.lock();
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
                chunks.push(Ok(make_chunk(&s, delta, None)));
            }

            StreamEvent::ToolCallStop { .. } | StreamEvent::ContentBlockStop { .. } => {
                // No per-block stop in OpenAI streaming
            }

            StreamEvent::UsageDelta { .. } => {
                // Accumulation handled at ResponseStop
            }

            StreamEvent::MessageStop { stop_reason, .. } => {
                let s = state.lock();
                let finish = finish_reason_from_canonical(&stop_reason).to_owned();
                let delta = DeltaOut::default();
                chunks.push(Ok(make_chunk(&s, delta, Some(finish))));
            }

            StreamEvent::ResponseStop { usage } => {
                if let Some(u) = usage {
                    let s = state.lock();
                    chunks.push(Ok(make_usage_chunk(&s, &u)));
                }
                chunks.push(Ok(Bytes::from("data: [DONE]\n\n")));
                // Signal termination: stop keepalive pings and trip the
                // terminate_on_sentinel scan downstream. Without this the
                // ping stream is infinite and the HTTP body never closes,
                // so clients (e.g. Codex/Cursor) hang on "waiting for
                // reply" even after [DONE] is sent.
                done.store(true, Ordering::SeqCst);
                chunks.push(Ok(Bytes::new()));
            }

            StreamEvent::Error { message } => {
                let err_json = serde_json::json!({ "error": { "message": message } });
                let s = serde_json::to_string(&err_json).unwrap_or_default();
                chunks.push(Ok(sse::data_only(&s)));
            }

            StreamEvent::RawProviderEvent(_) => {}
        }

        futures_util::stream::iter(chunks)
    });

    // Terminate the output stream after `data: [DONE]\n\n`. The flat_map
    // emits an empty `Bytes` sentinel after [DONE]; `scan` yields all items
    // up to (but not including) the sentinel, then returns None to end.
    //
    // Without this, the keepalive ping stream is infinite and `select` only
    // ends when *both* sides end, so the HTTP body never closes — clients
    // (Codex, Cursor, etc.) sit on "waiting for reply" forever after the
    // model has actually finished.
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
        ids::ResponseId, message::MessageRole, stream::CanonicalStream, StopReason, StreamError,
    };
    use futures::stream;

    fn fake_events() -> Vec<Result<StreamEvent, StreamError>> {
        vec![
            Ok(StreamEvent::ResponseStart {
                id: ResponseId("resp_1".to_string()),
                model: "gpt-4o".to_string(),
                created_at_unix: 1_700_000_000,
            }),
            Ok(StreamEvent::MessageStart {
                role: MessageRole::Assistant,
            }),
            Ok(StreamEvent::TextDelta {
                index: 0,
                text: "hi".to_string(),
            }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                stop_sequence: None,
            }),
            Ok(StreamEvent::ResponseStop { usage: None }),
        ]
    }

    /// Regression: with keepalive enabled, the encoded SSE stream MUST end
    /// after `data: [DONE]\n\n`. Before the terminate_on_sentinel fix the
    /// ping stream was infinite and `select` never closed the body, so
    /// clients hung on "waiting for reply" after the model finished.
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
