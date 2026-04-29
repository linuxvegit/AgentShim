/// Build an OpenAI Responses API request body from a CanonicalRequest.
use agent_shim_core::{
    request::CanonicalRequest, ContentBlock, MessageRole, ToolCallArguments, ToolChoice,
};
use serde_json::{json, Value};

pub fn build(req: &CanonicalRequest, upstream_model: &str) -> Value {
    let mut body = json!({
        "model": upstream_model,
        "stream": req.stream,
    });

    // Instructions from system messages
    let instructions: Vec<String> = req
        .system
        .iter()
        .map(|s| extract_text(&s.content))
        .collect();
    if !instructions.is_empty() {
        body["instructions"] = Value::String(instructions.join("\n"));
    }

    // Input items
    let mut input: Vec<Value> = Vec::new();

    for msg in &req.messages {
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => {
                // Tool results become function_call_output items
                for block in &msg.content {
                    if let ContentBlock::ToolResult(tr) = block {
                        let output = match &tr.content {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": tr.tool_call_id.0,
                            "output": output,
                        }));
                    }
                }
                continue;
            }
        };

        // Check for tool calls in assistant messages
        let tool_calls: Vec<&agent_shim_core::ToolCallBlock> = msg
            .content
            .iter()
            .filter_map(|b| {
                if let ContentBlock::ToolCall(tc) = b {
                    Some(tc)
                } else {
                    None
                }
            })
            .collect();

        if !tool_calls.is_empty() {
            // Emit text as a message first if present
            let text = extract_text_only(&msg.content);
            if !text.is_empty() {
                input.push(json!({
                    "type": "message",
                    "role": role,
                    "content": text,
                }));
            }
            // Then emit each tool call as a function_call item
            for tc in tool_calls {
                let args = match &tc.arguments {
                    ToolCallArguments::Complete { value } => value.to_string(),
                    ToolCallArguments::Streaming { data } => data.clone(),
                };
                input.push(json!({
                    "type": "function_call",
                    "call_id": tc.id.0,
                    "name": tc.name,
                    "arguments": args,
                }));
            }
        } else {
            let text = extract_text(&msg.content);
            input.push(json!({
                "type": "message",
                "role": role,
                "content": text,
            }));
        }
    }

    body["input"] = Value::Array(input);

    // Tools — both function tools and built-in tools
    let mut tools_arr: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            let mut tool = json!({
                "type": "function",
                "name": t.name,
                "parameters": t.input_schema,
            });
            if let Some(desc) = &t.description {
                tool["description"] = Value::String(desc.clone());
            }
            tool
        })
        .collect();

    // Append built-in tools (web_search, etc.) preserved from the frontend
    if let Some(Value::Array(builtin)) = req.extensions.get("builtin_tools") {
        tools_arr.extend(builtin.iter().cloned());
    }

    if !tools_arr.is_empty() {
        body["tools"] = Value::Array(tools_arr);
    }

    // Tool choice
    if !req.tools.is_empty() || req.extensions.get("builtin_tools").is_some() {
        match &req.tool_choice {
            ToolChoice::Auto => body["tool_choice"] = json!("auto"),
            ToolChoice::None => body["tool_choice"] = json!("none"),
            ToolChoice::Required => body["tool_choice"] = json!("required"),
            ToolChoice::Specific { name } => {
                body["tool_choice"] = json!({"type": "function", "name": name});
            }
        }
    }

    // Generation params
    if let Some(max) = req.generation.max_tokens {
        body["max_output_tokens"] = json!(max);
    }
    if let Some(temp) = req.generation.temperature {
        body["temperature"] = json!(temp);
    }
    if let Some(top_p) = req.generation.top_p {
        body["top_p"] = json!(top_p);
    }

    body
}

fn extract_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| {
            if let ContentBlock::Text(t) = b {
                Some(t.text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn extract_text_only(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| {
            if let ContentBlock::Text(t) = b {
                Some(t.text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
