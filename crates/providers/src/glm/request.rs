//! Build an outbound GLM chat-completions request body.
//!
//! Two GLM-specific transforms on top of the shared OAI-Chat encoder:
//!
//! 1. Strip the top-level `reasoning_effort` field. Zhipu's API rejects
//!    unknown fields with 400; the OpenAI-Chat encoder always writes it
//!    when the canonical policy carries an effort.
//! 2. Inject `thinking: {type: "enabled" | "disabled"}` driven by the
//!    canonical `ReasoningEffort`. Mapping: `Minimal` -> disabled, every
//!    other variant -> enabled. GLM's `thinking` field is binary; we don't
//!    invent additional knobs.
//!
//! `strip_cache_control` from the DeepSeek pattern is also applied as
//! defense-in-depth — the OAI encoder doesn't currently emit cache_control,
//! but a future change might, and Zhipu would 400 on it.

use agent_shim_core::{request::ReasoningEffort, BackendTarget, CanonicalRequest};

use crate::ProviderError;

/// Build the outbound JSON body for a GLM `/chat/completions` request.
pub(crate) fn build(
    req: &CanonicalRequest,
    target: &BackendTarget,
    accepts_xhigh: bool,
) -> Result<serde_json::Value, ProviderError> {
    let body = crate::oai_chat_wire::canonical_to_chat::build_json(req, target, accepts_xhigh);
    let mut value = body;
    let obj = value.as_object_mut().ok_or_else(|| {
        ProviderError::Encode("GLM request body must be a JSON object".to_string())
    })?;

    // 1. Strip the OpenAI-style reasoning_effort field — Zhipu rejects it.
    obj.remove("reasoning_effort");

    // 2. Inject thinking:{type:...} from canonical effort.
    let thinking_type = match req.resolved_policy.reasoning_effort {
        Some(ReasoningEffort::Minimal) => "disabled",
        Some(_) => "enabled",
        None => "enabled", // default: keep GLM's native thinking on
    };
    obj.insert(
        "thinking".to_string(),
        serde_json::json!({ "type": thinking_type }),
    );

    // 3. Defense-in-depth: strip Anthropic-style cache_control markers.
    let stripped = strip_cache_control(&mut value);
    if stripped > 0 {
        tracing::debug!(
            count = stripped,
            "glm: stripped cache_control fields from outbound body"
        );
    }

    Ok(value)
}

/// Walk the outbound body and remove any `cache_control` keys from message
/// objects and from message-content array blocks. Returns the total count.
/// Mirrors the deepseek defense-in-depth pass.
fn strip_cache_control(body: &mut serde_json::Value) -> usize {
    let mut count = 0;
    let Some(obj) = body.as_object_mut() else {
        return count;
    };
    let Some(messages) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return count;
    };
    for msg in messages {
        let Some(msg_obj) = msg.as_object_mut() else {
            continue;
        };
        if msg_obj.remove("cache_control").is_some() {
            count += 1;
        }
        if let Some(content) = msg_obj.get_mut("content").and_then(|c| c.as_array_mut()) {
            for block in content {
                if let Some(block_obj) = block.as_object_mut() {
                    if block_obj.remove("cache_control").is_some() {
                        count += 1;
                    }
                }
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        ContentBlock, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel, GenerationOptions,
        Message, RequestId,
    };
    use serde_json::json;

    fn target(model: &str) -> BackendTarget {
        BackendTarget {
            provider: "glm".into(),
            model: model.into(),
            policy: Default::default(),
        }
    }

    fn req_with_effort(effort: Option<ReasoningEffort>) -> CanonicalRequest {
        let mut req = CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::OpenAiChat,
                requested_model: FrontendModel::from("glm-test"),
            },
            model: FrontendModel::from("glm-test"),
            system: vec![],
            messages: vec![Message::user(vec![ContentBlock::text("hi")])],
            tools: vec![],
            tool_choice: Default::default(),
            generation: GenerationOptions::default(),
            response_format: None,
            stream: false,
            metadata: Default::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: Default::default(),
            extensions: ExtensionMap::new(),
        };
        req.resolved_policy.reasoning_effort = effort;
        req
    }

    #[test]
    fn build_injects_thinking_enabled_for_high_effort() {
        let req = req_with_effort(Some(ReasoningEffort::High));
        let body = build(&req, &target("glm-5.1"), false).unwrap();
        assert_eq!(body["thinking"]["type"], json!("enabled"));
    }

    #[test]
    fn build_injects_thinking_disabled_for_minimal_effort() {
        let req = req_with_effort(Some(ReasoningEffort::Minimal));
        let body = build(&req, &target("glm-5.1"), false).unwrap();
        assert_eq!(body["thinking"]["type"], json!("disabled"));
    }

    #[test]
    fn build_injects_thinking_enabled_when_effort_absent() {
        let req = req_with_effort(None);
        let body = build(&req, &target("glm-5.1"), false).unwrap();
        // No effort set -> keep GLM's native thinking on.
        assert_eq!(body["thinking"]["type"], json!("enabled"));
    }

    #[test]
    fn build_strips_reasoning_effort_top_level() {
        let req = req_with_effort(Some(ReasoningEffort::High));
        let body = build(&req, &target("glm-5.1"), false).unwrap();
        assert!(
            body.get("reasoning_effort").is_none(),
            "reasoning_effort must be stripped: {body}"
        );
    }

    #[test]
    fn build_strips_cache_control_message_and_block_level() {
        // Hand-craft a body shape with cache_control to exercise the strip
        // pass — the canonical OAI encoder doesn't currently write
        // cache_control, but this test pins the defense-in-depth behavior
        // for the day a future encoder change starts leaking it.
        let mut body = json!({
            "messages": [
                {
                    "role": "user",
                    "cache_control": {"type": "ephemeral"},
                    "content": [
                        {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}
                    ]
                }
            ]
        });
        let stripped = strip_cache_control(&mut body);
        assert_eq!(stripped, 2);
        assert!(body["messages"][0].get("cache_control").is_none());
        assert!(body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none());
    }

    #[test]
    fn build_well_formed_for_minimal_request() {
        let req = req_with_effort(None);
        let body = build(&req, &target("glm-5.1"), false).unwrap();
        assert_eq!(body["model"], json!("glm-5.1"));
        assert!(body["messages"].is_array());
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert!(body.get("reasoning_effort").is_none());
    }
}
