//! Parse a real captured Copilot /models response and assert metadata flows
//! through into ModelMetadata entries.

use agent_shim_core::ModelMetadata;
use agent_shim_providers::github_copilot::models::parse_models_response;
use std::fs;

fn load_fixture() -> String {
    fs::read_to_string("tests/data/copilot_models.json").expect("fixture readable")
}

#[test]
fn parses_full_metadata_for_claude_opus_4_6() {
    let raw = load_fixture();
    let parsed = parse_models_response(&raw).expect("parse ok");

    let m: &ModelMetadata = parsed
        .get("claude-opus-4.6")
        .expect("entry for claude-opus-4.6");
    assert_eq!(m.context_window_tokens, Some(200_000));
    assert_eq!(m.max_output_tokens, Some(32_000));
    assert_eq!(m.family.as_deref(), Some("claude-opus-4.6"));
    assert_eq!(m.supports.vision, Some(true));
    assert_eq!(m.supports.tool_calls, Some(true));
    assert_eq!(m.supports.streaming, Some(true));
}

#[test]
fn parses_full_metadata_for_claude_1m_variant() {
    let raw = load_fixture();
    let parsed = parse_models_response(&raw).expect("parse ok");
    let m = parsed
        .get("claude-opus-4.6-1m")
        .expect("entry for claude-opus-4.6-1m");
    assert_eq!(m.context_window_tokens, Some(1_000_000));
    assert_eq!(m.max_output_tokens, Some(64_000));
    assert_eq!(m.family.as_deref(), Some("claude-opus-4.6-1m"));
}

#[test]
fn parses_full_metadata_for_gpt_5_5() {
    let raw = load_fixture();
    let parsed = parse_models_response(&raw).expect("parse ok");
    let m = parsed.get("gpt-5.5").expect("entry for gpt-5.5");
    assert_eq!(m.context_window_tokens, Some(1_050_000));
    assert_eq!(m.max_output_tokens, Some(128_000));
    assert_eq!(m.family.as_deref(), Some("gpt-5.5"));
}

#[test]
fn entries_without_capability_limits_get_none() {
    let raw = load_fixture();
    let parsed = parse_models_response(&raw).expect("parse ok");
    // text-embedding-ada-002 carries an empty `limits` (only max_inputs),
    // so context_window_tokens and max_output_tokens are both unreported.
    let m = parsed
        .get("text-embedding-ada-002")
        .expect("entry for text-embedding-ada-002");
    assert_eq!(m.context_window_tokens, None);
    assert_eq!(m.max_output_tokens, None);
    assert_eq!(m.family.as_deref(), Some("text-embedding-ada-002"));
}

#[test]
fn missing_data_array_is_an_error() {
    let err = parse_models_response("{}").expect_err("must reject missing data");
    let msg = format!("{err}");
    assert!(msg.contains("missing `data`"), "got: {msg}");
}

#[test]
fn invalid_json_is_a_decode_error() {
    let err = parse_models_response("not json").expect_err("must reject garbage");
    let msg = format!("{err}");
    assert!(msg.contains("models response"), "got: {msg}");
}

#[test]
fn parses_reasoning_effort_array_for_gpt_family() {
    // Bump in v0.8.0: parser now extracts the `reasoning_effort: [...]`
    // array Copilot exposes for GPT-5 family models.
    let raw = load_fixture();
    let parsed = parse_models_response(&raw).expect("parse ok");
    let m = parsed.get("gpt-5.5").expect("entry for gpt-5.5");
    // Confirms the array is captured verbatim (canonical 6-value vocabulary).
    assert_eq!(
        m.supports.reasoning_effort_values.as_deref(),
        Some(
            &vec![
                "none".to_string(),
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "xhigh".to_string(),
            ][..],
        )
    );
    // Bool form derived from the array — non-empty means yes.
    assert_eq!(m.supports.reasoning_effort, Some(true));
}

#[test]
fn parses_adaptive_thinking_and_budget_for_claude_family() {
    // Anthropic family on Copilot reports its thinking shape via
    // `adaptive_thinking: true` + min/max budget tokens.
    let raw = load_fixture();
    let parsed = parse_models_response(&raw).expect("parse ok");
    let m = parsed
        .get("claude-opus-4.6")
        .expect("entry for claude-opus-4.6");
    assert_eq!(m.adaptive_thinking, Some(true));
    assert_eq!(m.thinking_budget_min, Some(1024));
    assert_eq!(m.thinking_budget_max, Some(32000));
    // Adaptive thinking also implies reasoning effort capability.
    assert_eq!(m.supports.reasoning_effort, Some(true));
}

#[test]
fn entries_without_reasoning_keep_none() {
    let raw = load_fixture();
    let parsed = parse_models_response(&raw).expect("parse ok");
    let m = parsed.get("gpt-4o").expect("entry for gpt-4o");
    assert_eq!(m.supports.reasoning_effort_values, None);
    assert_eq!(m.adaptive_thinking, None);
    assert_eq!(m.thinking_budget_min, None);
    assert_eq!(m.thinking_budget_max, None);
}
