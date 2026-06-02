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

    let mut emitted_response_start = false;
    let mut emitted_message_start = false;
    // Set once we see `response.completed` / `response.failed`. After that,
    // a transport-level error from the upstream closing the connection is
    // expected and we drop it instead of forwarding a `StreamError::Upstream`.
    let mut stream_completed = false;

    let event_stream = sse_stream.flat_map(move |result| {
        let events: Vec<Result<StreamEvent, StreamError>> = match result {
            Err(e) => {
                if stream_completed {
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
                    stream_completed = true;
                }

                let parsed: serde_json::Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(e) => {
                        return futures::stream::iter(vec![Err(StreamError::Decode(format!(
                            "json parse for {event_type}: {e}"
                        )))]);
                    }
                };

                parse_event(
                    event_type,
                    &parsed,
                    &mut emitted_response_start,
                    &mut emitted_message_start,
                )
            }
        };
        futures::stream::iter(events)
    });

    Box::pin(event_stream)
}

fn parse_event(
    event_type: &str,
    data: &serde_json::Value,
    emitted_response_start: &mut bool,
    emitted_message_start: &mut bool,
) -> Vec<Result<StreamEvent, StreamError>> {
    let mut events = Vec::new();

    match event_type {
        "response.created" | "response.in_progress" if !*emitted_response_start => {
            *emitted_response_start = true;
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

            if !*emitted_message_start {
                *emitted_message_start = true;
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
                }
                "message" => {
                    events.push(Ok(StreamEvent::ContentBlockStop {
                        index: output_index,
                    }));
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
}
