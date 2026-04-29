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

    let event_stream = sse_stream.flat_map(move |result| {
        let events: Vec<Result<StreamEvent, StreamError>> = match result {
            Err(e) => vec![Err(StreamError::Upstream(e.to_string()))],
            Ok(sse_event) => {
                let event_type = sse_event.event.as_str();
                let data = &sse_event.data;

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
