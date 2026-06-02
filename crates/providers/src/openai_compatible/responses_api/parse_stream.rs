use std::collections::HashMap;

/// Parse an SSE stream from the OpenAI Responses API into a CanonicalStream.
use eventsource_stream::Eventsource;
use futures::StreamExt;
use futures_core::Stream;

use agent_shim_core::{
    ContentBlockKind, MessageRole, ResponseId, StopReason, StreamError, StreamEvent, ToolCallId,
    Usage,
};

pub fn parse<S>(byte_stream: S) -> agent_shim_core::CanonicalStream
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    let sse_stream = byte_stream.eventsource();
    let state = std::sync::Arc::new(std::sync::Mutex::new(ParserState::default()));
    let state_for_items = std::sync::Arc::clone(&state);

    let event_stream = sse_stream.flat_map(move |result| {
        let events: Vec<Result<StreamEvent, StreamError>> = {
            let mut state = state_for_items.lock().expect("state mutex poisoned");
            match result {
                Err(e) => {
                    if state.stream_completed {
                        tracing::debug!(
                            error = %e,
                            "ignoring openai-responses transport error after upstream end-of-stream"
                        );
                        Vec::new()
                    } else {
                        tracing::warn!(error = %e, "openai-responses SSE stream error");
                        vec![Err(StreamError::Upstream(e.to_string()))]
                    }
                }
                Ok(sse_event) => {
                    let event_type = sse_event.event.as_str();
                    let data = &sse_event.data;

                    if event_type == "response.completed" || event_type == "response.failed" {
                        state.stream_completed = true;
                    }

                    let parsed: serde_json::Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(e) => {
                            return futures::stream::iter(vec![Err(StreamError::Decode(
                                format!("json parse for {event_type}: {e}"),
                            ))]);
                        }
                    };

                    parse_event(event_type, &parsed, &mut state)
                }
            }
        };
        futures::stream::iter(events)
    });

    // After the upstream ends — clean `response.completed`, abrupt
    // disconnect, or transport error — `finalize` closes the envelope so
    // downstream encoders / H7 / streaming usage logger always see
    // MessageStop + ResponseStop. Idempotent: if `response.completed`
    // already drove completion, returns nothing.
    //
    // F2.1: ParserState tracks open blocks on `output_item.added`/`.done`,
    // so finalize also closes any orphaned blocks (tool blocks get
    // ToolCallStop before ContentBlockStop per lifecycle rule 4) before
    // emitting MessageStop + ResponseStop.
    let drain = futures::stream::once(async move {
        let mut state = state.lock().expect("state mutex poisoned");
        state.finalize().into_iter().map(Ok).collect::<Vec<_>>()
    })
    .flat_map(futures::stream::iter);

    Box::pin(event_stream.chain(drain))
}

#[derive(Default)]
struct ParserState {
    emitted_response_start: bool,
    emitted_message_start: bool,
    /// Set when a `MessageStop` event has been emitted — either by
    /// `response.completed` / `response.failed` or by `finalize`.
    emitted_message_stop: bool,
    /// Set when a `ResponseStop` event has been emitted. Lets `finalize`
    /// be idempotent.
    emitted_response_stop: bool,
    /// Set once the upstream signals end-of-stream
    /// (`response.completed` / `response.failed`). Lets the outer
    /// `flat_map` drop the trailing transport error that some upstreams
    /// emit when they close the connection right after the final SSE event.
    stream_completed: bool,
    /// Map of `output_index` → `ContentBlockKind` for blocks the upstream
    /// has opened via `response.output_item.added` but not yet closed via
    /// `response.output_item.done`. F2.1: used by `finalize` to close
    /// orphaned blocks on silent-drop mid-block so the canonical lifecycle
    /// stays well-formed (rule 1d). Inserted on .added, removed on .done.
    open_blocks: HashMap<u32, ContentBlockKind>,
}

impl ParserState {
    /// Called once after the upstream stream ends — for any reason,
    /// including abrupt disconnect without `response.completed`.
    /// Synthesises missing `MessageStop` / `ResponseStop` so downstream
    /// encoders always see a closed envelope. Idempotent.
    fn finalize(&mut self) -> Vec<StreamEvent> {
        if self.emitted_response_stop {
            return Vec::new();
        }
        let mut evts = Vec::new();
        if !self.emitted_response_start {
            // Stream broke before any successful chunk — nothing to close.
            return evts;
        }
        if !self.emitted_message_start {
            self.emitted_message_start = true;
            evts.push(StreamEvent::MessageStart {
                role: MessageRole::Assistant,
            });
        }
        // F2.1: close any blocks the upstream opened but never closed.
        // Walk indices in sorted order so the synthesised events are
        // deterministic for snapshot tests and cross-platform repro. Tool
        // blocks emit ToolCallStop before ContentBlockStop per lifecycle
        // rule 4; message blocks just emit ContentBlockStop.
        let mut orphaned: Vec<(u32, ContentBlockKind)> =
            self.open_blocks.drain().collect();
        orphaned.sort_unstable_by_key(|(idx, _)| *idx);
        for (idx, kind) in orphaned {
            if kind == ContentBlockKind::ToolCall {
                evts.push(StreamEvent::ToolCallStop { index: idx });
            }
            evts.push(StreamEvent::ContentBlockStop { index: idx });
        }
        if !self.emitted_message_stop {
            self.emitted_message_stop = true;
            evts.push(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                stop_sequence: None,
            });
        }
        self.emitted_response_stop = true;
        evts.push(StreamEvent::ResponseStop { usage: None });
        evts
    }
}

fn parse_event(
    event_type: &str,
    data: &serde_json::Value,
    state: &mut ParserState,
) -> Vec<Result<StreamEvent, StreamError>> {
    let mut events = Vec::new();

    match event_type {
        "response.created" | "response.in_progress" if !state.emitted_response_start => {
            state.emitted_response_start = true;
            let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
            let model = data
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let created = data.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0);
            events.push(Ok(StreamEvent::ResponseStart {
                id: ResponseId(id.to_string()),
                model: model.to_string(),
                created_at_unix: created,
            }));
        }
        "response.created" | "response.in_progress" => {}

        "response.output_item.added" => {
            let item = data.get("item").unwrap_or(data);
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let output_index = data
                .get("output_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;

            if !state.emitted_message_start {
                state.emitted_message_start = true;
                events.push(Ok(StreamEvent::MessageStart {
                    role: MessageRole::Assistant,
                }));
            }

            match item_type {
                "message" => {
                    events.push(Ok(StreamEvent::ContentBlockStart {
                        index: output_index,
                        kind: ContentBlockKind::Text,
                    }));
                    state
                        .open_blocks
                        .insert(output_index, ContentBlockKind::Text);
                }
                "function_call" => {
                    let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    events.push(Ok(StreamEvent::ContentBlockStart {
                        index: output_index,
                        kind: ContentBlockKind::ToolCall,
                    }));
                    events.push(Ok(StreamEvent::ToolCallStart {
                        index: output_index,
                        id: ToolCallId::from_provider(call_id),
                        name: name.to_string(),
                    }));
                    state
                        .open_blocks
                        .insert(output_index, ContentBlockKind::ToolCall);
                }
                _ => {}
            }
        }

        "response.output_text.delta" => {
            let delta = data.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            let output_index = data
                .get("output_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            if !delta.is_empty() {
                events.push(Ok(StreamEvent::TextDelta {
                    index: output_index,
                    text: delta.to_string(),
                }));
            }
        }

        "response.function_call_arguments.delta" => {
            let delta = data.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            let output_index = data
                .get("output_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            if !delta.is_empty() {
                events.push(Ok(StreamEvent::ToolCallArgumentsDelta {
                    index: output_index,
                    json_fragment: delta.to_string(),
                }));
            }
        }

        "response.output_text.done" | "response.content_part.done" => {
            // We handle block closing at output_item.done
        }

        "response.function_call_arguments.done" => {
            // We handle block closing at output_item.done
        }

        "response.output_item.done" => {
            let item = data.get("item").unwrap_or(data);
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let output_index = data
                .get("output_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;

            match item_type {
                "function_call" => {
                    events.push(Ok(StreamEvent::ToolCallStop {
                        index: output_index,
                    }));
                    events.push(Ok(StreamEvent::ContentBlockStop {
                        index: output_index,
                    }));
                    state.open_blocks.remove(&output_index);
                }
                "message" => {
                    events.push(Ok(StreamEvent::ContentBlockStop {
                        index: output_index,
                    }));
                    state.open_blocks.remove(&output_index);
                }
                _ => {}
            }
        }

        "response.completed" => {
            // Extract stop reason from status
            let status = data
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("completed");
            let stop_reason = match status {
                "incomplete" => StopReason::ContentFilter,
                _ => StopReason::EndTurn,
            };
            events.push(Ok(StreamEvent::MessageStop {
                stop_reason,
                stop_sequence: None,
            }));
            state.emitted_message_stop = true;

            // Extract usage
            let usage = data.get("usage").map(|u| Usage {
                input_tokens: u
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32),
                output_tokens: u
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32),
                ..Default::default()
            });
            events.push(Ok(StreamEvent::ResponseStop { usage }));
            state.emitted_response_stop = true;
        }

        "response.failed" => {
            let error_msg = data
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("upstream response failed");
            events.push(Ok(StreamEvent::Error {
                message: error_msg.to_string(),
            }));
        }

        "error" => {
            let msg = data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            events.push(Ok(StreamEvent::Error {
                message: msg.to_string(),
            }));
        }

        // Ignore events we don't need to translate
        _ => {}
    }

    events
}

#[cfg(test)]
mod tests {
    //! PR-B3 lifecycle coverage. Each test feeds a hand-crafted SSE byte
    //! stream into `parse()` and asserts the resulting canonical events
    //! satisfy `assert_canonical_lifecycle`. The validator is the single
    //! source of truth for the contract — see `CONTEXT.md`
    //! "Canonical lifecycle".

    use super::*;
    use bytes::Bytes;
    use futures::StreamExt;

    fn byte_stream(
        body: &'static str,
    ) -> impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static {
        // `Result<Bytes, reqwest::Error>` — `Ok` only. `reqwest::Error` has
        // no public constructor for synthetic errors, so this fixture stream
        // can only model happy-path bytes (which is all the lifecycle tests
        // here need).
        futures::stream::iter(vec![Ok(Bytes::from_static(body.as_bytes()))])
    }

    async fn drain(
        stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    ) -> Vec<StreamEvent> {
        let mut canonical = parse(stream);
        let mut events = Vec::new();
        while let Some(item) = canonical.next().await {
            match item {
                Ok(e) => events.push(e),
                Err(e) => panic!("parser yielded error on happy-path fixture: {e:?}"),
            }
        }
        agent_shim_protocol_tests::lifecycle::assert_canonical_lifecycle(&events);
        events
    }

    /// Minimal text-only flow: created → output_item.added (message) →
    /// output_text.delta → output_item.done → completed.
    #[tokio::test]
    async fn text_only_lifecycle_is_clean() {
        const SSE: &str = concat!(
            "event: response.created\n",
            "data: {\"id\":\"resp_1\",\"model\":\"gpt-4o\",\"created_at\":1700000000,\"status\":\"in_progress\"}\n\n",
            "event: response.output_item.added\n",
            "data: {\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_0\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"item_id\":\"msg_0\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hello\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_0\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\",\"annotations\":[]}]}}\n\n",
            "event: response.completed\n",
            "data: {\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}\n\n",
        );
        let events = drain(byte_stream(SSE)).await;
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
    }

    /// Function-call flow: created → output_item.added (function_call) →
    /// function_call_arguments.delta → output_item.done → completed. Verifies
    /// the ToolCall block opens, accumulates args, and closes with
    /// ToolCallStop → ContentBlockStop (rule 4).
    #[tokio::test]
    async fn single_function_call_lifecycle_is_clean() {
        const SSE: &str = concat!(
            "event: response.created\n",
            "data: {\"id\":\"resp_1\",\"model\":\"gpt-4o\",\"created_at\":1700000000,\"status\":\"in_progress\"}\n\n",
            "event: response.output_item.added\n",
            "data: {\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_0\",\"call_id\":\"call_1\",\"name\":\"get_weather\",\"arguments\":\"\",\"status\":\"in_progress\"}}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"item_id\":\"fc_0\",\"output_index\":0,\"delta\":\"{\\\"city\\\":\\\"SF\\\"}\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_0\",\"call_id\":\"call_1\",\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"SF\\\"}\",\"status\":\"completed\"}}\n\n",
            "event: response.completed\n",
            "data: {\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":8,\"output_tokens\":4}}\n\n",
        );
        let events = drain(byte_stream(SSE)).await;
        let args: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallArgumentsDelta { json_fragment, .. } => {
                    Some(json_fragment.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(args, "{\"city\":\"SF\"}");
    }

    /// Lifecycle coverage: upstream sends `response.created` and then drops
    /// the connection silently — no `response.output_item.added`, no
    /// `response.completed`. finalize must synthesise MessageStart →
    /// MessageStop → ResponseStop so downstream encoders / H7 hook / usage
    /// logger see a closed envelope.
    ///
    /// Counterpart to `silent_drop_mid_block_finalizes_envelope` below,
    /// which exercises the F2.1 open-block tracking. This test covers the
    /// no-blocks-opened path; that one covers the orphaned-block path.
    #[tokio::test]
    async fn silent_drop_after_response_created_finalizes_envelope() {
        const SSE: &str = concat!(
            "event: response.created\n",
            "data: {\"id\":\"resp_1\",\"model\":\"gpt-4o\",\"created_at\":1700000000,\"status\":\"in_progress\"}\n\n",
        );
        let events = drain(byte_stream(SSE)).await;

        // ResponseStart was emitted by the parser …
        assert!(
            matches!(events.first(), Some(StreamEvent::ResponseStart { .. })),
            "expected ResponseStart, got {:?}",
            events.first()
        );
        // … and finalize synthesised the rest.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::MessageStop { .. })),
            "expected synthesised MessageStop on silent-drop stream"
        );
        assert!(
            matches!(events.last(), Some(StreamEvent::ResponseStop { .. })),
            "stream must end with ResponseStop, got {:?}",
            events.last()
        );

        // The drain helper already calls assert_canonical_lifecycle(&events),
        // so reaching this point proves the envelope is well-formed.
    }

    /// F2.1: upstream sends `response.created` + `response.output_item.added`
    /// for a message + a `response.output_text.delta` and then drops the
    /// connection. The text block is open with no `output_item.done`,
    /// no `response.completed`. finalize must close the open block (rule 1d)
    /// in addition to synthesising MessageStop + ResponseStop.
    #[tokio::test]
    async fn silent_drop_mid_block_finalizes_envelope() {
        const SSE: &str = concat!(
            "event: response.created\n",
            "data: {\"id\":\"resp_1\",\"model\":\"gpt-4o\",\"created_at\":1700000000,\"status\":\"in_progress\"}\n\n",
            "event: response.output_item.added\n",
            "data: {\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_0\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"item_id\":\"msg_0\",\"output_index\":0,\"content_index\":0,\"delta\":\"partial\"}\n\n",
        );
        let events = drain(byte_stream(SSE)).await;

        // The text delta survives — proves the parser didn't bail early.
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "partial");

        // finalize closed the orphaned block.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::ContentBlockStop { index: 0 })),
            "expected finalize to emit ContentBlockStop for orphaned block 0"
        );
        assert!(
            matches!(events.last(), Some(StreamEvent::ResponseStop { .. })),
            "stream must end with ResponseStop, got {:?}",
            events.last()
        );

        // The drain helper's assert_canonical_lifecycle would have panicked
        // on a leaked open block before F2.1; reaching here proves the
        // envelope is now well-formed.
    }
}
