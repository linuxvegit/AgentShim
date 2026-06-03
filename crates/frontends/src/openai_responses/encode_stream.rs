use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use agent_shim_core::stream::{CanonicalStream, ContentBlockKind, StreamEvent};
use bytes::Bytes;
use futures_util::{stream::BoxStream, Stream, StreamExt};

use super::mapping::status_from_stop_reason;
use super::wire::{
    ContentPartAdded, ContentPartDone, FunctionCallArgsDelta, FunctionCallArgsDone, OutputContent,
    OutputItem, OutputItemAdded, OutputItemDone, ReasoningDeltaPayload, ReasoningDonePayload,
    ResponseObject, TextDeltaPayload, TextDonePayload, UsageOut,
};
use crate::sse;

struct EncoderState {
    response_id: String,
    model: String,
    created_at: u64,
    output_index: u32,
    /// Accumulated text per output_index
    text_buf: std::collections::HashMap<u32, String>,
    /// Accumulated reasoning text per output_index
    reasoning_buf: std::collections::HashMap<u32, String>,
    /// Accumulated tool call arguments per output_index
    tool_args_buf: std::collections::HashMap<u32, String>,
    /// Item ID per output_index
    item_ids: std::collections::HashMap<u32, String>,
    /// Tool call metadata per output_index (call_id, name)
    tool_meta: std::collections::HashMap<u32, (String, String)>,
    /// Mapping from canonical content block indexes to Responses output indexes.
    canonical_to_output: std::collections::HashMap<u32, u32>,
    final_status: Option<&'static str>,
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
            reasoning_buf: std::collections::HashMap::new(),
            tool_args_buf: std::collections::HashMap::new(),
            item_ids: std::collections::HashMap::new(),
            tool_meta: std::collections::HashMap::new(),
            canonical_to_output: std::collections::HashMap::new(),
            final_status: None,
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

/// Hand-rolled `Stream` for the canonical -> OpenAI Responses SSE
/// translation.
///
/// Replaces the prior `flat_map` + `Arc<Mutex<EncoderState>>` +
/// `Arc<AtomicBool> done` + `Bytes::new()` sentinel + outer `scan()`
/// terminator shape with a single owning struct (same template as the
/// `openai_chat` and `anthropic_messages` encoders).
///
/// The state is heavier than the other two encoders (six HashMaps for
/// per-output_index accumulators) but the *coordination* shape is
/// identical, and the rewrite eliminates the lock cost on every text
/// delta — for a long response that was the single hottest lock site.
struct OaiResponsesEncoderStream {
    source: CanonicalStream,
    state: EncoderState,
    clock_override: Option<u64>,
    pending: VecDeque<Result<Bytes, crate::FrontendError>>,
    /// `true` once `response.completed` has been emitted; the next poll
    /// returns `Poll::Ready(None)`.
    done: bool,
}

impl OaiResponsesEncoderStream {
    fn new(source: CanonicalStream, clock_override: Option<u64>) -> Self {
        let mut state = EncoderState::new();
        if let Some(ts) = clock_override {
            state.created_at = ts;
        }
        Self {
            source,
            state,
            clock_override,
            pending: VecDeque::new(),
            done: false,
        }
    }

    fn push_event(&mut self, event_name: &str, data: &impl serde::Serialize) {
        if let Some(b) = emit(event_name, data) {
            self.pending.push_back(Ok(b));
        }
    }

    /// Process one canonical event into 0..N outbound chunks. Returns
    /// `true` once `response.completed` has been emitted and the stream
    /// should terminate.
    fn handle(&mut self, item: Result<StreamEvent, agent_shim_core::StreamError>) -> bool {
        let stream_event = match item {
            Ok(e) => e,
            Err(e) => {
                let err = serde_json::json!({
                    "type": "error",
                    "code": "server_error",
                    "message": e.to_string()
                });
                self.push_event("error", &err);
                return false;
            }
        };

        match stream_event {
            StreamEvent::ResponseStart {
                id,
                model,
                created_at_unix,
            } => {
                self.state.response_id = format!("resp_{}", id.0);
                self.state.model = model.clone();
                if self.clock_override.is_none() {
                    self.state.created_at = created_at_unix;
                }
                let resp = ResponseObject {
                    id: self.state.response_id.clone(),
                    object: "response",
                    status: "in_progress",
                    model,
                    created_at: self.state.created_at,
                    output: vec![],
                    usage: None,
                };
                self.push_event("response.created", &resp);
            }

            StreamEvent::ContentBlockStart { index, kind } => {
                if kind == ContentBlockKind::Text {
                    let oi = self.state.next_output_index();
                    let item_id = format!("msg_{oi}");
                    self.state.canonical_to_output.insert(index, oi);
                    self.state.item_ids.insert(oi, item_id.clone());
                    self.state.text_buf.insert(oi, String::new());

                    let item = OutputItem::Message {
                        id: item_id.clone(),
                        role: "assistant",
                        status: "in_progress",
                        content: vec![],
                    };
                    self.push_event(
                        "response.output_item.added",
                        &OutputItemAdded {
                            output_index: oi,
                            item,
                        },
                    );
                    self.push_event(
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
                    );
                } else if kind == ContentBlockKind::Reasoning {
                    let oi = self.state.next_output_index();
                    let item_id = format!("rs_{oi}");
                    self.state.canonical_to_output.insert(index, oi);
                    self.state.item_ids.insert(oi, item_id.clone());
                    self.state.reasoning_buf.insert(oi, String::new());

                    let item = OutputItem::Reasoning {
                        id: item_id,
                        status: "in_progress",
                        content: vec![],
                    };
                    self.push_event(
                        "response.output_item.added",
                        &OutputItemAdded {
                            output_index: oi,
                            item,
                        },
                    );
                }
            }

            StreamEvent::TextDelta { index, text } => {
                let Some(oi) = self.state.canonical_to_output.get(&index).copied() else {
                    self.pending
                        .push_back(Err(crate::FrontendError::Encode(format!(
                            "text delta for unknown content block index: {index}"
                        ))));
                    return false;
                };
                let item_id = self.state.item_ids.get(&oi).cloned().unwrap_or_default();

                self.push_event(
                    "response.output_text.delta",
                    &TextDeltaPayload {
                        item_id,
                        output_index: oi,
                        content_index: 0,
                        delta: text.clone(),
                    },
                );
                self.state.text_buf.entry(oi).or_default().push_str(&text);
            }

            StreamEvent::ReasoningDelta { index, text } => {
                let Some(oi) = self.state.canonical_to_output.get(&index).copied() else {
                    self.pending
                        .push_back(Err(crate::FrontendError::Encode(format!(
                            "reasoning delta for unknown content block index: {index}"
                        ))));
                    return false;
                };
                let item_id = self.state.item_ids.get(&oi).cloned().unwrap_or_default();

                self.push_event(
                    "response.reasoning.delta",
                    &ReasoningDeltaPayload {
                        item_id,
                        output_index: oi,
                        delta: text.clone(),
                    },
                );
                self.state
                    .reasoning_buf
                    .entry(oi)
                    .or_default()
                    .push_str(&text);
            }

            StreamEvent::ToolCallStart { index, id, name } => {
                let oi = self.state.next_output_index();
                let item_id = format!("fc_{oi}");
                self.state.canonical_to_output.insert(index, oi);
                self.state.item_ids.insert(oi, item_id.clone());
                self.state.tool_args_buf.insert(oi, String::new());
                self.state
                    .tool_meta
                    .insert(oi, (id.0.clone(), name.clone()));

                let item = OutputItem::FunctionCall {
                    id: item_id,
                    call_id: id.0,
                    name,
                    arguments: String::new(),
                    status: "in_progress",
                };
                self.push_event(
                    "response.output_item.added",
                    &OutputItemAdded {
                        output_index: oi,
                        item,
                    },
                );
            }

            StreamEvent::ToolCallArgumentsDelta {
                index,
                json_fragment,
            } => {
                let Some(oi) = self.state.canonical_to_output.get(&index).copied() else {
                    self.pending
                        .push_back(Err(crate::FrontendError::Encode(format!(
                            "tool call argument delta for unknown content block index: {index}"
                        ))));
                    return false;
                };
                let item_id = self.state.item_ids.get(&oi).cloned().unwrap_or_default();

                self.push_event(
                    "response.function_call_arguments.delta",
                    &FunctionCallArgsDelta {
                        item_id,
                        output_index: oi,
                        delta: json_fragment.clone(),
                    },
                );
                self.state
                    .tool_args_buf
                    .entry(oi)
                    .or_default()
                    .push_str(&json_fragment);
            }

            StreamEvent::ContentBlockStop { index } => {
                let Some(oi) = self.state.canonical_to_output.remove(&index) else {
                    return false;
                };
                let item_id = self.state.item_ids.get(&oi).cloned().unwrap_or_default();

                if let Some(text) = self.state.text_buf.remove(&oi) {
                    self.push_event(
                        "response.output_text.done",
                        &TextDonePayload {
                            item_id: item_id.clone(),
                            output_index: oi,
                            content_index: 0,
                            text: text.clone(),
                        },
                    );
                    self.push_event(
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
                    );
                    let done_item = OutputItem::Message {
                        id: item_id,
                        role: "assistant",
                        status: "completed",
                        content: vec![OutputContent::OutputText {
                            text,
                            annotations: vec![],
                        }],
                    };
                    self.push_event(
                        "response.output_item.done",
                        &OutputItemDone {
                            output_index: oi,
                            item: done_item,
                        },
                    );
                } else if let Some(reasoning_text) = self.state.reasoning_buf.remove(&oi) {
                    self.push_event(
                        "response.reasoning.done",
                        &ReasoningDonePayload {
                            item_id: item_id.clone(),
                            output_index: oi,
                            text: reasoning_text.clone(),
                        },
                    );
                    let done_item = OutputItem::Reasoning {
                        id: item_id,
                        status: "completed",
                        content: vec![OutputContent::Reasoning {
                            text: reasoning_text,
                            summary: None,
                        }],
                    };
                    self.push_event(
                        "response.output_item.done",
                        &OutputItemDone {
                            output_index: oi,
                            item: done_item,
                        },
                    );
                }
            }

            StreamEvent::ToolCallStop { index } => {
                let Some(oi) = self.state.canonical_to_output.get(&index).copied() else {
                    self.pending
                        .push_back(Err(crate::FrontendError::Encode(format!(
                            "tool call stop for unknown content block index: {index}"
                        ))));
                    return false;
                };
                let item_id = self.state.item_ids.get(&oi).cloned().unwrap_or_default();
                let args = self.state.tool_args_buf.remove(&oi).unwrap_or_default();
                let (call_id, name) = self.state.tool_meta.remove(&oi).unwrap_or_default();

                self.push_event(
                    "response.function_call_arguments.done",
                    &FunctionCallArgsDone {
                        item_id: item_id.clone(),
                        output_index: oi,
                        arguments: args.clone(),
                    },
                );
                let done_item = OutputItem::FunctionCall {
                    id: item_id,
                    call_id,
                    name,
                    arguments: args,
                    status: "completed",
                };
                self.push_event(
                    "response.output_item.done",
                    &OutputItemDone {
                        output_index: oi,
                        item: done_item,
                    },
                );
            }

            StreamEvent::UsageDelta { usage } => {
                self.state.input_tokens += usage.input_tokens.unwrap_or(0);
                self.state.output_tokens += usage.output_tokens.unwrap_or(0);
            }

            StreamEvent::MessageStop { stop_reason, .. } => {
                let status = status_from_stop_reason(&stop_reason);
                self.state.final_status = Some(status);
            }

            StreamEvent::ResponseStop { usage } => {
                if let Some(u) = usage {
                    if let Some(it) = u.input_tokens {
                        self.state.input_tokens = it;
                    }
                    if let Some(ot) = u.output_tokens {
                        self.state.output_tokens = ot;
                    }
                }
                let status = self.state.final_status.unwrap_or("completed");
                let resp = ResponseObject {
                    id: self.state.response_id.clone(),
                    object: "response",
                    status,
                    model: self.state.model.clone(),
                    created_at: self.state.created_at,
                    output: vec![],
                    usage: Some(UsageOut {
                        input_tokens: self.state.input_tokens,
                        output_tokens: self.state.output_tokens,
                        total_tokens: self.state.input_tokens + self.state.output_tokens,
                    }),
                };
                self.push_event("response.completed", &resp);
                return true;
            }

            StreamEvent::Error { message } => {
                let err = serde_json::json!({
                    "type": "error",
                    "code": "server_error",
                    "message": message
                });
                self.push_event("error", &err);
            }

            StreamEvent::MessageStart { .. } | StreamEvent::RawProviderEvent(_) => {}
        }

        false
    }
}

impl Stream for OaiResponsesEncoderStream {
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
    let encoder = OaiResponsesEncoderStream::new(canonical, clock_override);
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
        let bytes = encode(replay, None, Some(1))
            .fold(Vec::new(), |mut out, item| async move {
                out.extend_from_slice(&item.expect("stream item"));
                out
            })
            .await;
        String::from_utf8(bytes).expect("utf8 stream")
    }

    #[tokio::test]
    async fn deltas_follow_their_canonical_indexes() {
        let events: Vec<Result<StreamEvent, StreamError>> = vec![
            Ok(StreamEvent::ResponseStart {
                id: ResponseId("resp_1".to_string()),
                model: "gpt-test".to_string(),
                created_at_unix: 1,
            }),
            Ok(StreamEvent::MessageStart {
                role: MessageRole::Assistant,
            }),
            Ok(StreamEvent::ContentBlockStart {
                index: 3,
                kind: ContentBlockKind::Text,
            }),
            Ok(StreamEvent::TextDelta {
                index: 3,
                text: "A".to_string(),
            }),
            Ok(StreamEvent::ContentBlockStart {
                index: 8,
                kind: ContentBlockKind::ToolCall,
            }),
            Ok(StreamEvent::ToolCallStart {
                index: 8,
                id: ToolCallId::from_provider("call_1"),
                name: "search".to_string(),
            }),
            Ok(StreamEvent::ToolCallArgumentsDelta {
                index: 8,
                json_fragment: "{}".to_string(),
            }),
            Ok(StreamEvent::TextDelta {
                index: 3,
                text: "B".to_string(),
            }),
            Ok(StreamEvent::ToolCallStop { index: 8 }),
            Ok(StreamEvent::ContentBlockStop { index: 8 }),
            Ok(StreamEvent::ContentBlockStop { index: 3 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::ToolUse,
                stop_sequence: None,
            }),
            Ok(StreamEvent::ResponseStop {
                usage: Some(agent_shim_core::Usage {
                    input_tokens: Some(7),
                    output_tokens: Some(9),
                    ..Default::default()
                }),
            }),
        ];

        let body = collect_stream(Box::pin(stream::iter(events))).await;

        assert!(body.contains(r#""output_index":0,"content_index":0,"delta":"A""#));
        assert!(body.contains(r#""output_index":0,"content_index":0,"delta":"B""#));
        assert!(body.contains(r#""output_index":1,"delta":"{}""#));
        assert!(body.contains(r#""usage":{"input_tokens":7,"output_tokens":9,"total_tokens":16}"#));
    }

    #[tokio::test]
    async fn reasoning_delta_emits_responses_events() {
        let events: Vec<Result<StreamEvent, StreamError>> = vec![
            Ok(StreamEvent::ResponseStart {
                id: ResponseId("resp_r".to_string()),
                model: "gpt-test".to_string(),
                created_at_unix: 1,
            }),
            Ok(StreamEvent::MessageStart {
                role: MessageRole::Assistant,
            }),
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                kind: ContentBlockKind::Reasoning,
            }),
            Ok(StreamEvent::ReasoningDelta {
                index: 0,
                text: "first ".to_string(),
            }),
            Ok(StreamEvent::ReasoningDelta {
                index: 0,
                text: "part".to_string(),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                stop_sequence: None,
            }),
            Ok(StreamEvent::ResponseStop { usage: None }),
        ];

        let body = collect_stream(Box::pin(stream::iter(events))).await;

        // Locate each event in order and confirm they appear sequentially.
        let added_idx = body
            .find("event: response.output_item.added")
            .expect("output_item.added present");
        let delta1_idx = body[added_idx..]
            .find("event: response.reasoning.delta")
            .map(|p| p + added_idx)
            .expect("first reasoning.delta present");
        let delta2_idx = body[delta1_idx + 1..]
            .find("event: response.reasoning.delta")
            .map(|p| p + delta1_idx + 1)
            .expect("second reasoning.delta present");
        let done_idx = body[delta2_idx..]
            .find("event: response.reasoning.done")
            .map(|p| p + delta2_idx)
            .expect("reasoning.done present");
        let item_done_idx = body[done_idx..]
            .find("event: response.output_item.done")
            .map(|p| p + done_idx)
            .expect("output_item.done present");

        // Reasoning item is added with rs_ prefix and in_progress status.
        assert!(body[added_idx..delta1_idx].contains(r#""type":"reasoning""#));
        assert!(body[added_idx..delta1_idx].contains(r#""id":"rs_0""#));
        assert!(body[added_idx..delta1_idx].contains(r#""status":"in_progress""#));

        // Each delta carries the right payload.
        assert!(body[delta1_idx..delta2_idx].contains(r#""delta":"first ""#));
        assert!(body[delta1_idx..delta2_idx].contains(r#""item_id":"rs_0""#));
        assert!(body[delta2_idx..done_idx].contains(r#""delta":"part""#));

        // reasoning.done carries the accumulated text.
        assert!(body[done_idx..item_done_idx].contains(r#""text":"first part""#));
        assert!(body[done_idx..item_done_idx].contains(r#""item_id":"rs_0""#));

        // output_item.done finalizes with status "completed" and content array.
        let tail = &body[item_done_idx..];
        assert!(tail.contains(r#""type":"reasoning""#));
        assert!(tail.contains(r#""status":"completed""#));
        assert!(tail.contains(r#""text":"first part""#));
    }

    #[tokio::test]
    async fn reasoning_then_text_use_separate_output_indexes() {
        let events: Vec<Result<StreamEvent, StreamError>> = vec![
            Ok(StreamEvent::ResponseStart {
                id: ResponseId("resp_rt".to_string()),
                model: "gpt-test".to_string(),
                created_at_unix: 1,
            }),
            Ok(StreamEvent::MessageStart {
                role: MessageRole::Assistant,
            }),
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                kind: ContentBlockKind::Reasoning,
            }),
            Ok(StreamEvent::ReasoningDelta {
                index: 0,
                text: "thinking".to_string(),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::ContentBlockStart {
                index: 1,
                kind: ContentBlockKind::Text,
            }),
            Ok(StreamEvent::TextDelta {
                index: 1,
                text: "answer".to_string(),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 1 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                stop_sequence: None,
            }),
            Ok(StreamEvent::ResponseStop { usage: None }),
        ];

        let body = collect_stream(Box::pin(stream::iter(events))).await;

        // Reasoning lands on output_index 0 with rs_ prefix.
        assert!(body.contains(r#""output_index":0,"item":{"type":"reasoning","id":"rs_0""#));
        assert!(body.contains(r#""item_id":"rs_0","output_index":0,"delta":"thinking""#));
        assert!(body.contains(r#""item_id":"rs_0","output_index":0,"text":"thinking""#));

        // Text lands on output_index 1 with msg_ prefix and round-trips correctly.
        assert!(body.contains(r#""output_index":1,"item":{"type":"message","id":"msg_1""#));
        assert!(body
            .contains(r#""item_id":"msg_1","output_index":1,"content_index":0,"delta":"answer""#));
        assert!(body
            .contains(r#""item_id":"msg_1","output_index":1,"content_index":0,"text":"answer""#));

        // Reasoning events come before text events in the stream.
        let reasoning_added = body
            .find(r#""output_index":0,"item":{"type":"reasoning""#)
            .expect("reasoning added present");
        let text_added = body
            .find(r#""output_index":1,"item":{"type":"message""#)
            .expect("text added present");
        assert!(reasoning_added < text_added);
    }

    #[tokio::test]
    async fn tool_only_response_completed_has_empty_output_array() {
        // Plan 01 T6 — gap fill: confirm the encoder always emits an empty
        // `output: []` on `response.completed`, even when the response is
        // tool-only (no text). Function-call items are surfaced via
        // `response.output_item.added` / `output_item.done`, NOT replayed
        // inside `response.completed`. This test pins that contract.

        // Arrange: a stream with only a function call (no text content).
        let events: Vec<Result<StreamEvent, StreamError>> = vec![
            Ok(StreamEvent::ResponseStart {
                id: ResponseId("resp_tool_only".to_string()),
                model: "gpt-test".to_string(),
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
                id: ToolCallId::from_provider("call_x"),
                name: "lookup".to_string(),
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

        // Act
        let body = collect_stream(Box::pin(stream::iter(events))).await;

        // Assert: response.completed payload carries `"output":[]` literally.
        // Clamp the slice to the end of the actual SSE frame (delimited by a
        // blank line) so a stray `"output":[]` in any future post-completed
        // event cannot satisfy the assertion.
        let completed_pos = body
            .find("event: response.completed")
            .expect("response.completed event present");
        let completed_block = &body[completed_pos..];
        let frame_end = completed_block
            .find("\n\n")
            .unwrap_or(completed_block.len());
        let completed_frame = &completed_block[..frame_end];
        assert!(
            completed_frame.contains(r#""output":[]"#),
            "response.completed must have empty output array; frame was:\n{completed_frame}"
        );
        assert!(
            completed_frame.contains(r#""status":"completed""#),
            "response.completed must report completed status; frame was:\n{completed_frame}"
        );
    }
}
