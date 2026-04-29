/// Parse a non-streaming OpenAI chat.completions JSON response into a CanonicalStream.
use futures::stream;

use agent_shim_core::{
    ContentBlockKind, ResponseId, StopReason, StreamError, StreamEvent, ToolCallId, Usage,
};

pub(crate) fn parse(body: &[u8]) -> agent_shim_core::CanonicalStream {
    let events = match parse_inner(body) {
        Ok(evts) => evts.into_iter().map(Ok).collect::<Vec<_>>(),
        Err(e) => vec![Err(StreamError::Decode(e))],
    };
    Box::pin(stream::iter(events))
}

fn parse_inner(body: &[u8]) -> Result<Vec<StreamEvent>, String> {
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("json parse: {e}"))?;

    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("upstream error")
            .to_string();
        return Ok(vec![StreamEvent::Error { message: msg }]);
    }

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

    let mut events = Vec::new();

    events.push(StreamEvent::ResponseStart {
        id: ResponseId(id),
        model,
        created_at_unix: created,
    });
    events.push(StreamEvent::MessageStart {
        role: agent_shim_core::MessageRole::Assistant,
    });

    let choices = v
        .get("choices")
        .and_then(|c| c.as_array())
        .ok_or_else(|| "missing choices".to_string())?;

    let mut stop_reason = StopReason::EndTurn;
    let stop_sequence: Option<String> = None;
    let mut block_index: u32 = 0;

    for choice in choices {
        if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
            stop_reason = StopReason::from_provider_string(reason);
        }

        let message = match choice.get("message") {
            Some(m) => m,
            None => continue,
        };

        // Text content
        if let Some(text) = message.get("content").and_then(|c| c.as_str()) {
            if !text.is_empty() {
                events.push(StreamEvent::ContentBlockStart {
                    index: block_index,
                    kind: ContentBlockKind::Text,
                });
                events.push(StreamEvent::TextDelta {
                    index: block_index,
                    text: text.to_string(),
                });
                events.push(StreamEvent::ContentBlockStop { index: block_index });
                block_index += 1;
            }
        }

        // Tool calls
        if let Some(tool_calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tool_calls {
                let id = tc
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("call_unknown")
                    .to_string();
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let args_str = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                    .unwrap_or("{}")
                    .to_string();
                let _args_value: serde_json::Value =
                    serde_json::from_str(&args_str).unwrap_or(serde_json::Value::Null);

                events.push(StreamEvent::ContentBlockStart {
                    index: block_index,
                    kind: ContentBlockKind::ToolCall,
                });
                events.push(StreamEvent::ToolCallStart {
                    index: block_index,
                    id: ToolCallId::from_provider(&id),
                    name: name.clone(),
                });
                events.push(StreamEvent::ToolCallArgumentsDelta {
                    index: block_index,
                    json_fragment: args_str.clone(),
                });
                events.push(StreamEvent::ToolCallStop { index: block_index });
                events.push(StreamEvent::ContentBlockStop { index: block_index });
                block_index += 1;
            }
        }
    }

    // Usage
    let usage = v.get("usage").and_then(|u| {
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
    });

    events.push(StreamEvent::MessageStop {
        stop_reason,
        stop_sequence,
    });
    events.push(StreamEvent::ResponseStop { usage });

    Ok(events)
}
