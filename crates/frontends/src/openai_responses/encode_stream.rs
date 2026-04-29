use std::sync::Arc;
use std::time::Duration;

use agent_shim_core::stream::{ContentBlockKind, StreamEvent};
use bytes::Bytes;
use futures_util::{stream::BoxStream, StreamExt};
use parking_lot::Mutex;

use super::mapping::status_from_stop_reason;
use super::wire::{
    ContentPartAdded, ContentPartDone, FunctionCallArgsDelta, FunctionCallArgsDone,
    OutputContent, OutputItem, OutputItemAdded, OutputItemDone, ResponseObject,
    TextDeltaPayload, TextDonePayload, UsageOut,
};
use crate::sse;

struct EncoderState {
    response_id: String,
    model: String,
    created_at: u64,
    output_index: u32,
    /// Accumulated text per output_index
    text_buf: std::collections::HashMap<u32, String>,
    /// Accumulated tool call arguments per output_index
    tool_args_buf: std::collections::HashMap<u32, String>,
    /// Item ID per output_index
    item_ids: std::collections::HashMap<u32, String>,
    /// Tool call metadata per output_index (call_id, name)
    tool_meta: std::collections::HashMap<u32, (String, String)>,
    input_tokens: u32,
    output_tokens: u32,
}

impl EncoderState {
    fn new() -> Self {
        Self {
            response_id: String::new(),
            model: String::new(),
            created_at: 0,
            output_index: 0,
            text_buf: std::collections::HashMap::new(),
            tool_args_buf: std::collections::HashMap::new(),
            item_ids: std::collections::HashMap::new(),
            tool_meta: std::collections::HashMap::new(),
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    fn next_output_index(&mut self) -> u32 {
        let idx = self.output_index;
        self.output_index += 1;
        idx
    }
}

fn emit(event_name: &str, data: &impl serde::Serialize) -> Option<Bytes> {
    let json = serde_json::to_string(data).ok()?;
    Some(sse::event(event_name, &json))
}

pub fn encode(
    canonical: agent_shim_core::stream::CanonicalStream,
    keepalive: Option<Duration>,
    clock_override: Option<u64>,
) -> BoxStream<'static, Result<Bytes, crate::FrontendError>> {
    use std::sync::atomic::{AtomicBool, Ordering};

    let state = Arc::new(Mutex::new(EncoderState::new()));
    if let Some(ts) = clock_override {
        state.lock().created_at = ts;
    }
    let done = Arc::new(AtomicBool::new(false));
    let done_for_flat_map = Arc::clone(&done);

    let event_stream = canonical.flat_map(move |item| {
        let state = Arc::clone(&state);
        let done = Arc::clone(&done_for_flat_map);
        let mut chunks: Vec<Result<Bytes, crate::FrontendError>> = Vec::new();

        let stream_event = match item {
            Ok(e) => e,
            Err(e) => {
                let err = serde_json::json!({
                    "type": "error",
                    "code": "server_error",
                    "message": e.to_string()
                });
                if let Some(b) = emit("error", &err) {
                    chunks.push(Ok(b));
                }
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
                s.response_id = format!("resp_{}", id.0);
                s.model = model.clone();
                if clock_override.is_none() {
                    s.created_at = created_at_unix;
                }
                let resp = ResponseObject {
                    id: s.response_id.clone(),
                    object: "response",
                    status: "in_progress",
                    model,
                    created_at: s.created_at,
                    output: vec![],
                    usage: None,
                };
                if let Some(b) = emit("response.created", &resp) {
                    chunks.push(Ok(b));
                }
            }

            StreamEvent::ContentBlockStart { index: _, kind } => {
                if kind == ContentBlockKind::Text {
                    let mut s = state.lock();
                    let oi = s.next_output_index();
                    let item_id = format!("msg_{oi}");
                    s.item_ids.insert(oi, item_id.clone());
                    s.text_buf.insert(oi, String::new());

                    let item = OutputItem::Message {
                        id: item_id.clone(),
                        role: "assistant",
                        status: "in_progress",
                        content: vec![],
                    };
                    if let Some(b) = emit(
                        "response.output_item.added",
                        &OutputItemAdded {
                            output_index: oi,
                            item,
                        },
                    ) {
                        chunks.push(Ok(b));
                    }
                    if let Some(b) = emit(
                        "response.content_part.added",
                        &ContentPartAdded {
                            item_id,
                            output_index: oi,
                            content_index: 0,
                            part: OutputContent::OutputText {
                                text: String::new(),
                                annotations: vec![],
                            },
                        },
                    ) {
                        chunks.push(Ok(b));
                    }
                }
            }

            StreamEvent::TextDelta { index: _, text } => {
                let s = state.lock();
                // Find the current text output_index (last message item)
                let oi = s.output_index.saturating_sub(1);
                let item_id = s
                    .item_ids
                    .get(&oi)
                    .cloned()
                    .unwrap_or_default();
                drop(s);

                if let Some(b) = emit(
                    "response.output_text.delta",
                    &TextDeltaPayload {
                        item_id,
                        output_index: oi,
                        content_index: 0,
                        delta: text.clone(),
                    },
                ) {
                    chunks.push(Ok(b));
                }
                state.lock().text_buf.entry(oi).or_default().push_str(&text);
            }

            StreamEvent::ToolCallStart { index: _, id, name } => {
                let mut s = state.lock();
                let oi = s.next_output_index();
                let item_id = format!("fc_{oi}");
                s.item_ids.insert(oi, item_id.clone());
                s.tool_args_buf.insert(oi, String::new());
                s.tool_meta.insert(oi, (id.0.clone(), name.clone()));

                let item = OutputItem::FunctionCall {
                    id: item_id,
                    call_id: id.0,
                    name,
                    arguments: String::new(),
                    status: "in_progress",
                };
                if let Some(b) = emit(
                    "response.output_item.added",
                    &OutputItemAdded {
                        output_index: oi,
                        item,
                    },
                ) {
                    chunks.push(Ok(b));
                }
            }

            StreamEvent::ToolCallArgumentsDelta {
                index: _,
                json_fragment,
            } => {
                let s = state.lock();
                let oi = s.output_index.saturating_sub(1);
                let item_id = s.item_ids.get(&oi).cloned().unwrap_or_default();
                drop(s);

                if let Some(b) = emit(
                    "response.function_call_arguments.delta",
                    &FunctionCallArgsDelta {
                        item_id,
                        output_index: oi,
                        delta: json_fragment.clone(),
                    },
                ) {
                    chunks.push(Ok(b));
                }
                state
                    .lock()
                    .tool_args_buf
                    .entry(oi)
                    .or_default()
                    .push_str(&json_fragment);
            }

            StreamEvent::ContentBlockStop { index: _ } => {
                let mut s = state.lock();
                let oi = s.output_index.saturating_sub(1);
                let item_id = s.item_ids.get(&oi).cloned().unwrap_or_default();

                if let Some(text) = s.text_buf.remove(&oi) {
                    if let Some(b) = emit(
                        "response.output_text.done",
                        &TextDonePayload {
                            item_id: item_id.clone(),
                            output_index: oi,
                            content_index: 0,
                            text: text.clone(),
                        },
                    ) {
                        chunks.push(Ok(b));
                    }
                    if let Some(b) = emit(
                        "response.content_part.done",
                        &ContentPartDone {
                            item_id: item_id.clone(),
                            output_index: oi,
                            content_index: 0,
                            part: OutputContent::OutputText {
                                text: text.clone(),
                                annotations: vec![],
                            },
                        },
                    ) {
                        chunks.push(Ok(b));
                    }
                    let done_item = OutputItem::Message {
                        id: item_id,
                        role: "assistant",
                        status: "completed",
                        content: vec![OutputContent::OutputText {
                            text,
                            annotations: vec![],
                        }],
                    };
                    if let Some(b) = emit(
                        "response.output_item.done",
                        &OutputItemDone {
                            output_index: oi,
                            item: done_item,
                        },
                    ) {
                        chunks.push(Ok(b));
                    }
                }
            }

            StreamEvent::ToolCallStop { index: _ } => {
                let mut s = state.lock();
                let oi = s.output_index.saturating_sub(1);
                let item_id = s.item_ids.get(&oi).cloned().unwrap_or_default();
                let args = s.tool_args_buf.remove(&oi).unwrap_or_default();
                let (call_id, name) = s
                    .tool_meta
                    .remove(&oi)
                    .unwrap_or_default();

                if let Some(b) = emit(
                    "response.function_call_arguments.done",
                    &FunctionCallArgsDone {
                        item_id: item_id.clone(),
                        output_index: oi,
                        arguments: args.clone(),
                    },
                ) {
                    chunks.push(Ok(b));
                }
                let done_item = OutputItem::FunctionCall {
                    id: item_id,
                    call_id,
                    name,
                    arguments: args,
                    status: "completed",
                };
                if let Some(b) = emit(
                    "response.output_item.done",
                    &OutputItemDone {
                        output_index: oi,
                        item: done_item,
                    },
                ) {
                    chunks.push(Ok(b));
                }
            }

            StreamEvent::UsageDelta { usage } => {
                let mut s = state.lock();
                s.input_tokens += usage.input_tokens.unwrap_or(0);
                s.output_tokens += usage.output_tokens.unwrap_or(0);
            }

            StreamEvent::MessageStop { stop_reason, .. } => {
                let s = state.lock();
                let status = status_from_stop_reason(&stop_reason);
                let resp = ResponseObject {
                    id: s.response_id.clone(),
                    object: "response",
                    status,
                    model: s.model.clone(),
                    created_at: s.created_at,
                    output: vec![],
                    usage: Some(UsageOut {
                        input_tokens: s.input_tokens,
                        output_tokens: s.output_tokens,
                        total_tokens: s.input_tokens + s.output_tokens,
                    }),
                };
                drop(s);
                if let Some(b) = emit("response.completed", &resp) {
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
                if !done.load(Ordering::SeqCst) {
                    let s = state.lock();
                    let resp = ResponseObject {
                        id: s.response_id.clone(),
                        object: "response",
                        status: "completed",
                        model: s.model.clone(),
                        created_at: s.created_at,
                        output: vec![],
                        usage: Some(UsageOut {
                            input_tokens: s.input_tokens,
                            output_tokens: s.output_tokens,
                            total_tokens: s.input_tokens + s.output_tokens,
                        }),
                    };
                    drop(s);
                    if let Some(b) = emit("response.completed", &resp) {
                        chunks.push(Ok(b));
                    }
                    done.store(true, Ordering::SeqCst);
                    chunks.push(Ok(Bytes::new()));
                }
            }

            StreamEvent::Error { message } => {
                let err = serde_json::json!({
                    "type": "error",
                    "code": "server_error",
                    "message": message
                });
                if let Some(b) = emit("error", &err) {
                    chunks.push(Ok(b));
                }
            }

            StreamEvent::MessageStart { .. }
            | StreamEvent::ReasoningDelta { .. }
            | StreamEvent::RawProviderEvent(_) => {}
        }

        futures_util::stream::iter(chunks)
    });

    // Terminate the stream after response.completed. The flat_map emits an
    // empty Bytes sentinel; scan() yields all items up to the sentinel, then
    // returns None to end.
    let terminate_on_sentinel =
        |stream: BoxStream<'static, Result<Bytes, crate::FrontendError>>| -> BoxStream<'static, Result<Bytes, crate::FrontendError>> {
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
