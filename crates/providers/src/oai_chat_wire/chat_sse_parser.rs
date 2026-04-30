//! Parse an SSE byte-stream from OpenAI into a CanonicalStream.

use std::collections::HashSet;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use futures_core::Stream;

use agent_shim_core::{
    ContentBlockKind, MessageRole, ResponseId, StopReason, StreamError, StreamEvent, ToolCallId,
    Usage,
};

pub(crate) fn parse<S>(byte_stream: S) -> agent_shim_core::CanonicalStream
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    let sse_stream = byte_stream.eventsource();

    let mut emitted_response_start = false;
    let mut emitted_message_start = false;
    let mut text_block_open = false;
    let mut open_tool_blocks: HashSet<u32> = HashSet::new();
    let mut response_id = String::new();
    let mut response_model = String::new();

    let event_stream = sse_stream.flat_map(move |result| {
        let events: Vec<Result<StreamEvent, StreamError>> = match result {
            Err(e) => {
                tracing::warn!(error = %e, "SSE stream error");
                vec![Err(StreamError::Upstream(e.to_string()))]
            }
            Ok(event) => {
                tracing::debug!(event_type = %event.event, data_len = event.data.len(), "SSE event received");
                if event.data == "[DONE]" {
                    let mut evts = Vec::new();
                    // Close any open text block
                    if text_block_open {
                        evts.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
                        text_block_open = false;
                    }
                    // Close any open tool blocks
                    for idx in open_tool_blocks.drain() {
                        evts.push(Ok(StreamEvent::ToolCallStop { index: idx }));
                        evts.push(Ok(StreamEvent::ContentBlockStop { index: idx }));
                    }
                    evts.push(Ok(StreamEvent::ResponseStop { usage: None }));
                    evts
                } else {
                    match parse_chunk(
                        &event.data,
                        &mut emitted_response_start,
                        &mut emitted_message_start,
                        &mut text_block_open,
                        &mut open_tool_blocks,
                        &mut response_id,
                        &mut response_model,
                    ) {
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

fn parse_chunk(
    data: &str,
    emitted_response_start: &mut bool,
    emitted_message_start: &mut bool,
    text_block_open: &mut bool,
    open_tool_blocks: &mut HashSet<u32>,
    response_id: &mut String,
    response_model: &mut String,
) -> Result<Vec<StreamEvent>, String> {
    let v: serde_json::Value =
        serde_json::from_str(data).map_err(|e| format!("json parse: {e}"))?;

    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("upstream error")
            .to_string();
        return Ok(vec![StreamEvent::Error { message: msg }]);
    }

    let mut events = Vec::new();

    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("unknown");
    let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("unknown");
    let created = v.get("created").and_then(|x| x.as_u64()).unwrap_or(0);

    if !*emitted_response_start {
        *emitted_response_start = true;
        *response_id = id.to_string();
        *response_model = model.to_string();
        events.push(StreamEvent::ResponseStart {
            id: ResponseId(id.to_string()),
            model: model.to_string(),
            created_at_unix: created,
        });
    }

    let choices = match v.get("choices").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => {
            if let Some(usage) = parse_usage(&v) {
                events.push(StreamEvent::UsageDelta { usage });
            }
            return Ok(events);
        }
    };

    for choice in choices {
        // role → MessageStart (once)
        if let Some(delta) = choice.get("delta") {
            if let Some(_role) = delta.get("role").and_then(|r| r.as_str()) {
                if !*emitted_message_start {
                    *emitted_message_start = true;
                    events.push(StreamEvent::MessageStart {
                        role: MessageRole::Assistant,
                    });
                }
            }

            // content text delta
            if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    if !*emitted_message_start {
                        *emitted_message_start = true;
                        events.push(StreamEvent::MessageStart {
                            role: MessageRole::Assistant,
                        });
                    }
                    if !*text_block_open {
                        *text_block_open = true;
                        events.push(StreamEvent::ContentBlockStart {
                            index: 0,
                            kind: ContentBlockKind::Text,
                        });
                    }
                    events.push(StreamEvent::TextDelta {
                        index: 0,
                        text: text.to_string(),
                    });
                }
            }

            // tool_calls deltas
            if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                if !*emitted_message_start {
                    *emitted_message_start = true;
                    events.push(StreamEvent::MessageStart {
                        role: MessageRole::Assistant,
                    });
                }
                // Close text block if open before tool calls start
                if *text_block_open {
                    *text_block_open = false;
                    events.push(StreamEvent::ContentBlockStop { index: 0 });
                }

                for tc in tool_calls {
                    let tc_index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                    // Offset tool indices by 1 since text uses index 0
                    let block_index = tc_index + 1;

                    if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !open_tool_blocks.contains(&block_index) {
                            open_tool_blocks.insert(block_index);
                            events.push(StreamEvent::ContentBlockStart {
                                index: block_index,
                                kind: ContentBlockKind::ToolCall,
                            });
                        }
                        events.push(StreamEvent::ToolCallStart {
                            index: block_index,
                            id: ToolCallId::from_provider(id),
                            name,
                        });
                    }

                    if let Some(args) = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                    {
                        if !args.is_empty() {
                            events.push(StreamEvent::ToolCallArgumentsDelta {
                                index: block_index,
                                json_fragment: args.to_string(),
                            });
                        }
                    }
                }
            }
        }

        // finish_reason — close blocks and emit MessageStop
        if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
            if !reason.is_empty() {
                if *text_block_open {
                    *text_block_open = false;
                    events.push(StreamEvent::ContentBlockStop { index: 0 });
                }
                for idx in open_tool_blocks.drain() {
                    events.push(StreamEvent::ToolCallStop { index: idx });
                    events.push(StreamEvent::ContentBlockStop { index: idx });
                }
                events.push(StreamEvent::MessageStop {
                    stop_reason: StopReason::from_provider_string(reason),
                    stop_sequence: None,
                });
            }
        }
    }

    if let Some(usage) = v
        .get("usage")
        .filter(|u| !u.is_null())
        .and_then(|_| parse_usage(&v))
    {
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
        cache_creation_input_tokens: u
            .get("cache_creation_input_tokens")
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        cache_read_input_tokens: u
            .get("cache_read_input_tokens")
            .or_else(|| u.get("prompt_tokens_details").and_then(|d| d.get("cached_tokens")))
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        ..Default::default()
    })
}
