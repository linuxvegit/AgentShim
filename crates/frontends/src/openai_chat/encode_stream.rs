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
    let state = Arc::new(Mutex::new(EncoderState::new(clock_override)));

    let event_stream = canonical.flat_map(move |item| {
        let state = Arc::clone(&state);
        let mut chunks: Vec<Result<Bytes, crate::FrontendError>> = Vec::new();

        let stream_event = match item {
            Ok(e) => e,
            Err(e) => {
                // Emit a data chunk with error info then [DONE]
                let err_json = serde_json::json!({ "error": { "message": e.to_string() } });
                let s = serde_json::to_string(&err_json).unwrap_or_default();
                chunks.push(Ok(sse::data_only(&s)));
                chunks.push(Ok(Bytes::from("data: [DONE]\n\n")));
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

    if let Some(interval) = keepalive {
        use tokio_stream::wrappers::IntervalStream;
        let ping_stream = IntervalStream::new(tokio::time::interval(interval))
            .map(|_| Ok::<Bytes, crate::FrontendError>(sse::comment("ping")));
        futures_util::stream::select(event_stream, ping_stream).boxed()
    } else {
        event_stream.boxed()
    }
}
