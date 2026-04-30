//! Build an outbound DeepSeek chat-completions request body.
//!
//! Today DeepSeek is wire-compatible with the OAI-Chat shape, so this module
//! is a thin wrapper around `oai_chat_wire::canonical_to_chat::build`. On top
//! of the shared encoder we apply one DeepSeek-specific post-processing step:
//! [`strip_cache_control`] removes any Anthropic-style `cache_control` keys
//! from message and content-block objects before the body is serialized over
//! the wire. DeepSeek's `/chat/completions` rejects unknown fields with a 400,
//! so leaving an Anthropic-injected `cache_control` marker in place would
//! break the request.
//!
//! Today the OAI-Chat encoder does not write `cache_control` to its output —
//! that field is an Anthropic wire concept and `canonical_to_chat::build`
//! ignores per-block extensions entirely. The strip is therefore a defense-in-
//! depth guard: if a future encoder change ever starts leaking extension keys,
//! we strip them here AND log a `debug!` event with a count so operators can
//! see the leak in tracing rather than as silent upstream 400s.
//!
//! The wrapper returns an owned `serde_json::Value` so the strip step can
//! mutate the body via `as_object_mut()` before the request is sent.

use agent_shim_core::{BackendTarget, CanonicalRequest};

/// Build the outbound JSON body for a DeepSeek `/chat/completions` request.
///
/// Delegates to the shared OAI-Chat encoder, then strips any Anthropic-style
/// `cache_control` keys (defense-in-depth — the OAI-Chat encoder doesn't
/// currently emit them, but a future change might).
pub(crate) fn build(req: &CanonicalRequest, target: &BackendTarget) -> serde_json::Value {
    let body = crate::oai_chat_wire::canonical_to_chat::build(req, target);
    // ChatBody is fully Serialize and never produces an error in practice;
    // panic loudly if a future field-shape change ever breaks that invariant
    // rather than silently sending `null` to the upstream.
    let mut value = serde_json::to_value(&body).expect("ChatBody serialization is infallible");
    let stripped = strip_cache_control(&mut value);
    if stripped > 0 {
        // Single event per call (not per occurrence) — easier to triage than a
        // log-storm if the encoder ever starts leaking many keys at once.
        tracing::debug!(
            count = stripped,
            "deepseek: stripped cache_control fields from outbound body \
             (DeepSeek's API doesn't accept Anthropic-style cache markers)"
        );
    }
    value
}

/// Walk the outbound body and remove any `cache_control` keys from message
/// objects and from message-content array blocks. Returns the total number of
/// keys removed.
///
/// Anthropic encodes prompt-cache markers on:
/// - the message object itself (`messages[].cache_control`), and
/// - individual content blocks (`messages[].content[].cache_control`).
///
/// Both are stripped here. Other top-level fields are left untouched.
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
            provider: "deepseek".into(),
            model: model.into(),
            policy: Default::default(),
        }
    }

    fn request_with_messages(messages: Vec<Message>) -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: FrontendModel::from("claude-test"),
            },
            model: FrontendModel::from("claude-test"),
            system: vec![],
            messages,
            tools: vec![],
            tool_choice: Default::default(),
            generation: GenerationOptions::default(),
            response_format: None,
            stream: false,
            metadata: Default::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: Default::default(),
            extensions: ExtensionMap::new(),
        }
    }

    #[test]
    fn strip_cache_control_removes_message_level_field() {
        let mut body = json!({
            "messages": [
                { "role": "user", "content": "hi", "cache_control": { "type": "ephemeral" } },
                { "role": "assistant", "content": "ok" },
            ]
        });

        let stripped = strip_cache_control(&mut body);

        assert_eq!(stripped, 1);
        assert!(body["messages"][0].get("cache_control").is_none());
        assert!(body["messages"][1].get("cache_control").is_none());
    }

    #[test]
    fn strip_cache_control_removes_block_level_field() {
        let mut body = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "hi", "cache_control": { "type": "ephemeral" } },
                        { "type": "text", "text": "world" },
                    ]
                }
            ]
        });

        let stripped = strip_cache_control(&mut body);

        assert_eq!(stripped, 1);
        assert!(body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none());
    }

    #[test]
    fn strip_cache_control_returns_total_count_across_levels() {
        // Two message-level + one block-level cache_control = 3 strips.
        let mut body = json!({
            "messages": [
                {
                    "role": "user",
                    "cache_control": { "type": "ephemeral" },
                    "content": [
                        { "type": "text", "text": "hi", "cache_control": { "type": "ephemeral" } },
                    ]
                },
                {
                    "role": "assistant",
                    "cache_control": { "type": "ephemeral" },
                    "content": "ok"
                },
            ]
        });

        let stripped = strip_cache_control(&mut body);

        assert_eq!(stripped, 3);
    }

    #[test]
    fn strip_cache_control_no_op_when_absent() {
        let mut body = json!({
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": [{ "type": "text", "text": "ok" }] },
            ]
        });
        let snapshot = body.clone();

        let stripped = strip_cache_control(&mut body);

        assert_eq!(stripped, 0);
        // Body is untouched.
        assert_eq!(body, snapshot);
    }

    #[test]
    fn strip_cache_control_handles_missing_messages_array() {
        // Malformed body without a `messages` array — strip is a no-op.
        let mut body = json!({ "model": "deepseek-chat" });
        let stripped = strip_cache_control(&mut body);
        assert_eq!(stripped, 0);
    }

    #[test]
    fn build_produces_clean_body_for_canonical_request_without_cache_control() {
        // Sanity check: the OAI-Chat encoder doesn't currently leak
        // `cache_control`, so a normal canonical request produces a body with
        // no `cache_control` keys anywhere — no strip needed, no debug log.
        let req = request_with_messages(vec![Message::user(vec![ContentBlock::text("hi")])]);
        let body = build(&req, &target("deepseek-chat"));

        assert_eq!(strip_cache_control(&mut body.clone()), 0);
        // And the body is well-formed JSON with the expected top-level shape.
        assert_eq!(body["model"], json!("deepseek-chat"));
        assert!(body["messages"].is_array());
    }
}
