//! Verify Anthropic-as-provider emits the new `effort-2025-11-24` shape
//! (`thinking: {type:"adaptive"}` + `output_config: {effort: "..."}`) when the
//! route policy carries the `anthropic-beta: effort-2025-11-24` header. Falls
//! back to the legacy `thinking: {type:"enabled", budget_tokens}` shape when
//! the beta is absent.

use agent_shim_core::{policy::ResolvedPolicy, request::ReasoningEffort};
use agent_shim_core::{
    BackendTarget, CanonicalRequest, ContentBlock, ExtensionMap, FrontendInfo, FrontendKind,
    FrontendModel, GenerationOptions, Message, RequestId,
};

fn req_with_effort_and_beta(effort: ReasoningEffort, has_effort_beta: bool) -> CanonicalRequest {
    let mut headers = vec![];
    if has_effort_beta {
        headers.push((
            "anthropic-beta".to_string(),
            "effort-2025-11-24".to_string(),
        ));
    }
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::AnthropicMessages,
            requested_model: FrontendModel::from("claude-opus-4-7"),
        },
        model: FrontendModel::from("claude-opus-4-7"),
        system: vec![],
        messages: vec![Message::user(vec![ContentBlock::text("hi")])],
        tools: vec![],
        tool_choice: Default::default(),
        generation: GenerationOptions::default(),
        response_format: None,
        stream: false,
        metadata: Default::default(),
        inbound_anthropic_headers: headers.clone(),
        resolved_policy: ResolvedPolicy {
            reasoning_effort: Some(effort),
            reasoning_budget_tokens: None,
            supported_efforts: None,
            anthropic_headers: headers,
        },
        extensions: ExtensionMap::new(),
    }
}

fn target() -> BackendTarget {
    BackendTarget {
        provider: "anthropic".to_string(),
        model: "claude-opus-4-7".to_string(),
        policy: Default::default(),
    }
}

#[test]
fn emits_adaptive_and_output_config_when_effort_beta_present() {
    let req = req_with_effort_and_beta(ReasoningEffort::Max, true);
    let body = agent_shim_providers::anthropic::request::build(&req, &target());
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert!(
        body["thinking"].get("budget_tokens").is_none(),
        "adaptive path must omit budget_tokens, got: {body:#}"
    );
    assert_eq!(body["output_config"]["effort"], "max");
}

#[test]
fn legacy_path_when_effort_beta_absent() {
    let req = req_with_effort_and_beta(ReasoningEffort::Xhigh, false);
    let body = agent_shim_providers::anthropic::request::build(&req, &target());
    assert_eq!(body["thinking"]["type"], "enabled");
    assert!(body["thinking"]["budget_tokens"].as_u64().is_some());
    assert!(
        body.get("output_config").is_none(),
        "legacy path must NOT emit output_config, got: {body:#}"
    );
}

#[test]
fn minimal_maps_to_low_on_adaptive_path() {
    let req = req_with_effort_and_beta(ReasoningEffort::Minimal, true);
    let body = agent_shim_providers::anthropic::request::build(&req, &target());
    assert_eq!(
        body["output_config"]["effort"], "low",
        "Anthropic has no `minimal` — Minimal compresses to low"
    );
}
