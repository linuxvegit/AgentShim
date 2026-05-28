//! End-to-end: Anthropic frontend sending `output_config.effort: max`,
//! routed through a route with `reasoning_mapping: [max → xhigh]`, and
//! asserting the outbound OpenAI-Chat-shape body carries
//! `reasoning_effort: "xhigh"` (because the target accepts the Copilot
//! `xhigh` extension).
//!
//! Wires together:
//!   1. `agent_shim_frontends::anthropic_messages::decode::decode`
//!      — inbound JSON → `CanonicalRequest`
//!   2. `agent_shim_core::policy::RoutePolicy::resolve`
//!      — apply mapping table → `ResolvedPolicy`
//!   3. `agent_shim_providers::oai_chat_wire::canonical_to_chat::build_json`
//!      — canonical → outbound JSON, with `accepts_xhigh: true`
//!
//! Plan A Task 16 (2026-05-28 reasoning effort mapping spec).

use agent_shim_core::{
    policy::{MappingRule, RoutePolicy},
    request::ReasoningEffort,
    BackendTarget,
};
use agent_shim_frontends::anthropic_messages::decode::decode;
use agent_shim_providers::oai_chat_wire::canonical_to_chat::build_json;

/// Per-route policy that rewrites canonical `Max` to `Xhigh`. Stand-in for
/// the `reasoning_mapping: [{ match: max, set: xhigh }]` config block.
fn policy_max_to_xhigh() -> RoutePolicy {
    RoutePolicy {
        default_reasoning_effort: None,
        default_anthropic_beta: None,
        reasoning_mapping: vec![MappingRule {
            r#match: ReasoningEffort::Max,
            set: ReasoningEffort::Xhigh,
        }],
    }
}

fn copilot_target() -> BackendTarget {
    BackendTarget {
        provider: "copilot".to_string(),
        model: "claude-opus-4.7".to_string(),
        policy: policy_max_to_xhigh(),
    }
}

#[test]
fn claude_code_max_effort_becomes_copilot_xhigh() {
    // ── 1. Inbound: Claude Code "ultrathink" — Anthropic Messages body
    //               with `output_config.effort: max`.
    let inbound = serde_json::json!({
        "model": "claude-opus-4-7",
        "max_tokens": 1024,
        "messages": [{ "role": "user", "content": "hi" }],
        "thinking": { "type": "adaptive" },
        "output_config": { "effort": "max" }
    });
    let body_bytes = serde_json::to_vec(&inbound).unwrap();

    // ── 2. Frontend decode: Anthropic Messages → CanonicalRequest.
    let canonical = decode(&body_bytes).expect("anthropic decode succeeds");
    assert_eq!(
        canonical
            .generation
            .reasoning
            .as_ref()
            .and_then(|r| r.effort),
        Some(ReasoningEffort::Max),
        "decode must extract Max from output_config.effort"
    );

    // ── 3. Apply route policy: Max → Xhigh via the mapping table.
    let target = copilot_target();
    let mut req = canonical;
    req.resolved_policy = target.policy.resolve(&req);
    assert_eq!(
        req.resolved_policy.reasoning_effort,
        Some(ReasoningEffort::Xhigh),
        "mapping table should rewrite Max → Xhigh"
    );

    // ── 4. Provider encode: canonical → outbound Chat body, accepts_xhigh = true.
    //       This mirrors what github_copilot's complete() does with
    //       `self.capabilities.accepts_xhigh = true`.
    let chat_body = build_json(&req, &target, /* accepts_xhigh */ true);
    assert_eq!(
        chat_body["reasoning_effort"], "xhigh",
        "outbound body must carry reasoning_effort: xhigh on a Copilot-capable target"
    );
}

#[test]
fn pure_openai_target_compresses_mapped_xhigh_to_high() {
    // Same mapping table, but the target rejects the xhigh extension
    // (pure OpenAI Chat). The provider compression step turns the canonical
    // Xhigh into the wire-level "high". Guards that the mapping table
    // doesn't accidentally bypass the per-target capability gate.
    let inbound = serde_json::json!({
        "model": "claude-opus-4-7",
        "max_tokens": 1024,
        "messages": [{ "role": "user", "content": "hi" }],
        "thinking": { "type": "adaptive" },
        "output_config": { "effort": "max" }
    });
    let body_bytes = serde_json::to_vec(&inbound).unwrap();
    let canonical = decode(&body_bytes).expect("anthropic decode succeeds");
    let target = copilot_target();
    let mut req = canonical;
    req.resolved_policy = target.policy.resolve(&req);

    let chat_body = build_json(&req, &target, /* accepts_xhigh */ false);
    assert_eq!(
        chat_body["reasoning_effort"], "high",
        "pure-OpenAI target compresses Xhigh → high at the wire boundary"
    );
}
