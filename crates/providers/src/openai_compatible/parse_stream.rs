/// Parse an SSE byte-stream from OpenAI into a CanonicalStream.

use eventsource_stream::Eventsource;
use futures::StreamExt;
use futures_core::Stream;

use agent_shim_core::{
    ContentBlockKind, ResponseId, StopReason, StreamError, StreamEvent, ToolCallId, Usage,
};

pub(crate) fn parse<S>(byte_stream: S) -> agent_shim_core::CanonicalStream
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    let sse_stream = byte_stream.eventsource();

    let event_stream = sse_stream.flat_map(|result| {
        let events: Vec<Result<StreamEvent, StreamError>> = match result {
            Err(e) => vec![Err(StreamError::Upstream(e.to_string()))],
            Ok(event) => {
                if event.data == "[DONE]" {
                    vec![Ok(StreamEvent::ResponseStop { usage: None })]
                } else {
                    match parse_chunk(&event.data) {
                        Ok(evts) => evts.into_iter().map(Ok).collect(),
                        Err(e) => vec![Err(StreamError::Decode(e))],
                    }
                }
            }
        };
        futures::stream::iter(events)
    });

    Box::pin(event_stream)
}

fn parse_chunk(data: &str) -> Result<Vec<StreamEvent>, String> {
    let v: serde_json::Value =
        serde_json::from_str(data).map_err(|e| format!("json parse: {e}"))?;

    // Error object from OpenAI
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("upstream error")
            .to_string();
        return Ok(vec![StreamEvent::Error { message: msg }]);
    }

    let mut events = Vec::new();

    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown")
        .to_string();
    let model = v
        .get("model")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown")
        .to_string();
    let created = v.get("created").and_then(|x| x.as_u64()).unwrap_or(0);

    // Emit ResponseStart on the first chunk (callers deduplicate if needed).
    events.push(StreamEvent::ResponseStart {
        id: ResponseId(id),
        model,
        created_at_unix: created,
    });

    let choices = match v.get("choices").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => {
            // Possibly a usage-only chunk
            if let Some(usage) = parse_usage(&v) {
                events.push(StreamEvent::UsageDelta { usage });
            }
            return Ok(events);
        }
    };

    for choice in choices {
        let index = choice
            .get("index")
            .and_then(|i| i.as_u64())
            .unwrap_or(0) as u32;

        // finish_reason
        if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
            if !reason.is_empty() {
                let stop_reason = StopReason::from_provider_string(reason);
                events.push(StreamEvent::MessageStop {
                    stop_reason,
                    stop_sequence: None,
                });
            }
        }

        let delta = match choice.get("delta") {
            Some(d) => d,
            None => continue,
        };

        // role → MessageStart
        if let Some(role) = delta.get("role").and_then(|r| r.as_str()) {
            let msg_role = match role {
                "assistant" => agent_shim_core::MessageRole::Assistant,
                "user" => agent_shim_core::MessageRole::User,
                _ => agent_shim_core::MessageRole::Assistant,
            };
            events.push(StreamEvent::MessageStart { role: msg_role });
        }

        // content text delta
        if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
            if !text.is_empty() {
                events.push(StreamEvent::ContentBlockStart {
                    index,
                    kind: ContentBlockKind::Text,
                });
                events.push(StreamEvent::TextDelta {
                    index,
                    text: text.to_string(),
                });
                events.push(StreamEvent::ContentBlockStop { index });
            }
        }

        // tool_calls deltas
        if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tool_calls {
                let tc_index = tc
                    .get("index")
                    .and_then(|i| i.as_u64())
                    .unwrap_or(0) as u32;

                // ToolCallStart when we have an id/name
                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    events.push(StreamEvent::ContentBlockStart {
                        index: tc_index,
                        kind: ContentBlockKind::ToolCall,
                    });
                    events.push(StreamEvent::ToolCallStart {
                        index: tc_index,
                        id: ToolCallId::from_provider(id),
                        name,
                    });
                }

                // arguments fragment
                if let Some(args) = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                {
                    if !args.is_empty() {
                        events.push(StreamEvent::ToolCallArgumentsDelta {
                            index: tc_index,
                            json_fragment: args.to_string(),
                        });
                    }
                }
            }
        }
    }

    // Usage in the chunk (stream_options.include_usage)
    if let Some(usage) = v.get("usage").filter(|u| !u.is_null()).and_then(|_| parse_usage(&v)) {
        events.push(StreamEvent::UsageDelta { usage });
    }

    Ok(events)
}

fn parse_usage(v: &serde_json::Value) -> Option<Usage> {
    let u = v.get("usage")?;
    Some(Usage {
        input_tokens: u
            .get("prompt_tokens")
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        output_tokens: u
            .get("completion_tokens")
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        ..Default::default()
    })
}
