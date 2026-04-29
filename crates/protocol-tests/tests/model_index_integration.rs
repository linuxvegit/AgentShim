use agent_shim_router::model_index::ModelIndex;
use std::collections::{BTreeSet, HashMap};

fn copilot_models() -> BTreeSet<String> {
    [
        "claude-sonnet-4-5-20250514",
        "claude-opus-4-5-20250514",
        "claude-haiku-3-5-20241022",
        "gpt-4o",
        "gpt-4o-mini",
        "gpt-4.1",
        "gpt-4.1-mini",
        "gpt-4.1-nano",
        "o3",
        "o3-mini",
        "o4-mini",
        "gemini-2.0-flash",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn build_index() -> ModelIndex {
    let mut map = HashMap::new();
    map.insert("copilot".to_string(), copilot_models());
    ModelIndex::new(map)
}

#[test]
fn claude_short_name_matches_dated() {
    let idx = build_index();
    assert_eq!(
        idx.resolve("copilot", "claude-sonnet-4-5"),
        Some("claude-sonnet-4-5-20250514")
    );
}

#[test]
fn claude_opus_short_matches() {
    let idx = build_index();
    assert_eq!(
        idx.resolve("copilot", "claude-opus-4-5"),
        Some("claude-opus-4-5-20250514")
    );
}

#[test]
fn exact_gpt4o_matches() {
    let idx = build_index();
    assert_eq!(idx.resolve("copilot", "gpt-4o"), Some("gpt-4o"));
}

#[test]
fn unknown_model_returns_none() {
    let idx = build_index();
    assert_eq!(idx.resolve("copilot", "llama-3.1-405b"), None);
}

#[test]
fn case_insensitive_match() {
    let idx = build_index();
    assert_eq!(idx.resolve("copilot", "GPT-4o"), Some("gpt-4o"));
}
