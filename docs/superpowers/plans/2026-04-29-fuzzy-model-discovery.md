# Fuzzy Model Discovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Automatically discover available models from upstream providers at startup and fuzzy-match agent-requested model names to the closest supported model.

**Architecture:** Extend `BackendProvider` with an optional `list_models()` method. At startup, collect discovered models into a `ModelIndex` (new module in router crate) that uses token-based fuzzy scoring. Handlers call `model_index.resolve()` after static route resolution to substitute the best match.

**Tech Stack:** Rust, async-trait, serde_json, reqwest, proptest

---

### Task 1: Add `list_models()` to BackendProvider Trait

**Files:**
- Modify: `crates/providers/src/lib.rs:38-47`

- [ ] **Step 1: Write the failing test**

Create a test that asserts a default `list_models()` implementation returns `Ok(None)`:

```rust
// in crates/providers/src/lib.rs, at the bottom
#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream};

    struct DummyProvider;

    #[async_trait]
    impl BackendProvider for DummyProvider {
        fn name(&self) -> &'static str { "dummy" }
        fn capabilities(&self) -> &ProviderCapabilities {
            &ProviderCapabilities { streaming: false, tool_use: false, vision: false, json_mode: false }
        }
        async fn complete(&self, _req: CanonicalRequest, _target: BackendTarget) -> Result<CanonicalStream, ProviderError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn default_list_models_returns_none() {
        let p = DummyProvider;
        let result = p.list_models().await.unwrap();
        assert!(result.is_none());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p agent-shim-providers default_list_models_returns_none`
Expected: FAIL — `list_models` method not found on `BackendProvider`.

- [ ] **Step 3: Add `list_models()` default method to `BackendProvider`**

In `crates/providers/src/lib.rs`, add to the trait (after the `complete` method, inside the `#[async_trait]` block):

```rust
    async fn list_models(&self) -> Result<Option<std::collections::BTreeSet<String>>, ProviderError> {
        Ok(None)
    }
```

Also add the `use std::collections::BTreeSet;` import at the top of the file if not already present.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p agent-shim-providers default_list_models_returns_none`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/lib.rs
git commit -m "feat(providers): add optional list_models() to BackendProvider trait"
```

---

### Task 2: Implement `list_models()` for CopilotProvider

**Files:**
- Modify: `crates/providers/src/github_copilot/mod.rs`

- [ ] **Step 1: Write the trait method**

In the `#[async_trait] impl BackendProvider for CopilotProvider` block in `crates/providers/src/github_copilot/mod.rs`, add:

```rust
    async fn list_models(&self) -> Result<Option<BTreeSet<String>>, ProviderError> {
        let token = self.manager.get().await?;
        let models = models::list_models(&self.http, &token).await?;
        if models.is_empty() {
            return Ok(None);
        }
        Ok(Some(models))
    }
```

Add `use std::collections::BTreeSet;` to the imports if not present.

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p agent-shim-providers`
Expected: compiles without errors.

- [ ] **Step 3: Commit**

```bash
git add crates/providers/src/github_copilot/mod.rs
git commit -m "feat(providers): implement list_models() for CopilotProvider"
```

---

### Task 3: Implement `list_models()` for OpenAiCompatibleProvider

**Files:**
- Modify: `crates/providers/src/openai_compatible/mod.rs`

- [ ] **Step 1: Write a unit test using mockito**

In `crates/providers/src/openai_compatible/mod.rs` (or a test module):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_models_returns_discovered_models() {
        let mut server = mockito::Server::new_async().await;
        let mock = server.mock("GET", "/v1/models")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{
                "object": "list",
                "data": [
                    {"id": "gpt-4o", "object": "model"},
                    {"id": "gpt-4o-mini", "object": "model"},
                    {"id": "deepseek-chat", "object": "model"}
                ]
            }"#)
            .create_async().await;

        let provider = OpenAiCompatibleProvider::new(
            "test",
            server.url(),
            "test-key",
            Default::default(),
            30,
        ).unwrap();

        let result = provider.list_models().await.unwrap().unwrap();
        assert!(result.contains("gpt-4o"));
        assert!(result.contains("gpt-4o-mini"));
        assert!(result.contains("deepseek-chat"));
        assert_eq!(result.len(), 3);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_models_returns_none_on_404() {
        let mut server = mockito::Server::new_async().await;
        let mock = server.mock("GET", "/v1/models")
            .with_status(404)
            .with_body("not found")
            .create_async().await;

        let provider = OpenAiCompatibleProvider::new(
            "test",
            server.url(),
            "test-key",
            Default::default(),
            30,
        ).unwrap();

        let result = provider.list_models().await.unwrap();
        assert!(result.is_none());
        mock.assert_async().await;
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p agent-shim-providers list_models_returns_discovered_models list_models_returns_none_on_404`
Expected: FAIL — `list_models` not overridden, returns `Ok(None)` for the first test.

- [ ] **Step 3: Implement `list_models()` on OpenAiCompatibleProvider**

In the `#[async_trait] impl BackendProvider for OpenAiCompatibleProvider` block:

```rust
    async fn list_models(&self) -> Result<Option<BTreeSet<String>>, ProviderError> {
        let url = format!("{}/v1/models", self.base_url.trim_end_matches('/'));
        let resp = self.client
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Ok(None);
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Decode(e.to_string()))?;

        let models = body
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|id| id.as_str()).map(String::from))
                    .collect::<BTreeSet<String>>()
            })
            .unwrap_or_default();

        if models.is_empty() {
            return Ok(None);
        }
        Ok(Some(models))
    }
```

Add `use std::collections::BTreeSet;` to imports if not present.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p agent-shim-providers list_models_returns_discovered_models list_models_returns_none_on_404`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/openai_compatible/mod.rs
git commit -m "feat(providers): implement list_models() for OpenAiCompatibleProvider"
```

---

### Task 4: Create ModelIndex with Token-Based Fuzzy Matching

**Files:**
- Create: `crates/router/src/model_index.rs`
- Modify: `crates/router/src/lib.rs` (add `pub mod model_index;`)
- Modify: `crates/router/Cargo.toml` (no new deps needed)

- [ ] **Step 1: Write tokenizer tests**

Create `crates/router/src/model_index.rs`:

```rust
use std::collections::{BTreeSet, HashMap};

struct ModelEntry {
    original: String,
    normalized: String,
    tokens: Vec<String>,
}

pub struct ModelIndex {
    providers: HashMap<String, Vec<ModelEntry>>,
}

fn tokenize(name: &str) -> Vec<String> {
    name.to_lowercase()
        .split(|c: char| c == '-' || c == '_' || c == '.' || c == '/')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn score(requested_tokens: &[String], candidate_tokens: &[String], req_norm: &str, cand_norm: &str) -> f64 {
    if req_norm == cand_norm {
        return 1.0;
    }

    if req_norm.starts_with(cand_norm) || cand_norm.starts_with(req_norm) {
        let shorter = req_norm.len().min(cand_norm.len()) as f64;
        let longer = req_norm.len().max(cand_norm.len()) as f64;
        return 0.8 + 0.2 * (shorter / longer);
    }

    let max_len = requested_tokens.len().max(candidate_tokens.len());
    if max_len == 0 {
        return 0.0;
    }

    let mut weighted_matches = 0.0;
    let mut total_weight = 0.0;

    for (i, req_tok) in requested_tokens.iter().enumerate() {
        let weight = 1.0 / (1.0 + i as f64);
        total_weight += weight;
        if candidate_tokens.contains(req_tok) {
            weighted_matches += weight;
        }
    }

    for (i, cand_tok) in candidate_tokens.iter().enumerate() {
        if i >= requested_tokens.len() {
            let weight = 1.0 / (1.0 + i as f64);
            total_weight += weight;
        }
    }

    weighted_matches / total_weight
}

const THRESHOLD: f64 = 0.4;

impl ModelIndex {
    pub fn new(discovered: HashMap<String, BTreeSet<String>>) -> Self {
        let providers = discovered
            .into_iter()
            .map(|(provider, models)| {
                let entries = models
                    .into_iter()
                    .map(|name| {
                        let normalized = name.to_lowercase();
                        let tokens = tokenize(&name);
                        ModelEntry { original: name, normalized, tokens }
                    })
                    .collect();
                (provider, entries)
            })
            .collect();
        Self { providers }
    }

    pub fn empty() -> Self {
        Self { providers: HashMap::new() }
    }

    pub fn resolve(&self, provider: &str, requested: &str) -> Option<&str> {
        let entries = self.providers.get(provider)?;
        let req_norm = requested.to_lowercase();
        let req_tokens = tokenize(requested);

        let mut best_score = 0.0_f64;
        let mut best: Option<&ModelEntry> = None;

        for entry in entries {
            let s = score(&req_tokens, &entry.tokens, &req_norm, &entry.normalized);
            if s > best_score
                || (s == best_score
                    && best.map_or(true, |b| {
                        entry.original.len() < b.original.len()
                            || (entry.original.len() == b.original.len()
                                && entry.original < b.original)
                    }))
            {
                best_score = s;
                best = Some(entry);
            }
        }

        if best_score >= THRESHOLD {
            best.map(|e| e.original.as_str())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_with(provider: &str, models: &[&str]) -> ModelIndex {
        let set: BTreeSet<String> = models.iter().map(|s| s.to_string()).collect();
        let mut map = HashMap::new();
        map.insert(provider.to_string(), set);
        ModelIndex::new(map)
    }

    #[test]
    fn tokenize_splits_on_delimiters() {
        assert_eq!(tokenize("claude-sonnet-4-5-20250514"), vec!["claude", "sonnet", "4", "5", "20250514"]);
        assert_eq!(tokenize("gpt-4o-mini"), vec!["gpt", "4o", "mini"]);
        assert_eq!(tokenize("Qwen/Qwen3-235B-A22B"), vec!["qwen", "qwen3", "235b", "a22b"]);
        assert_eq!(tokenize("deepseek_chat"), vec!["deepseek", "chat"]);
        assert_eq!(tokenize("model.v2.1"), vec!["model", "v2", "1"]);
    }

    #[test]
    fn exact_match_case_insensitive() {
        let idx = index_with("p", &["gpt-4o", "gpt-4o-mini"]);
        assert_eq!(idx.resolve("p", "gpt-4o"), Some("gpt-4o"));
        assert_eq!(idx.resolve("p", "GPT-4o"), Some("gpt-4o"));
    }

    #[test]
    fn prefix_match_finds_dated_variant() {
        let idx = index_with("p", &["claude-sonnet-4-5-20250514"]);
        assert_eq!(idx.resolve("p", "claude-sonnet-4-5"), Some("claude-sonnet-4-5-20250514"));
    }

    #[test]
    fn prefix_match_prefers_shorter_canonical() {
        let idx = index_with("p", &["claude-sonnet-4-5", "claude-sonnet-4-5-20250514"]);
        assert_eq!(idx.resolve("p", "claude-sonnet-4-5"), Some("claude-sonnet-4-5"));
    }

    #[test]
    fn unrelated_model_returns_none() {
        let idx = index_with("p", &["gpt-4o", "gpt-4o-mini"]);
        assert_eq!(idx.resolve("p", "llama-3-70b"), None);
    }

    #[test]
    fn unknown_provider_returns_none() {
        let idx = index_with("copilot", &["gpt-4o"]);
        assert_eq!(idx.resolve("deepseek", "gpt-4o"), None);
    }

    #[test]
    fn empty_index_returns_none() {
        let idx = ModelIndex::empty();
        assert_eq!(idx.resolve("p", "gpt-4o"), None);
    }

    #[test]
    fn token_overlap_selects_best_match() {
        let idx = index_with("p", &["claude-opus-4-5", "claude-sonnet-4-5", "claude-haiku-3-5"]);
        assert_eq!(idx.resolve("p", "claude-opus-4-5"), Some("claude-opus-4-5"));
        assert_eq!(idx.resolve("p", "claude-sonnet-4-5"), Some("claude-sonnet-4-5"));
    }

    #[test]
    fn tie_breaking_prefers_shorter_then_alphabetical() {
        let idx = index_with("p", &["model-b", "model-a"]);
        assert_eq!(idx.resolve("p", "model"), Some("model-a"));
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/router/src/lib.rs`, add:

```rust
pub mod model_index;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo nextest run -p agent-shim-router model_index`
Expected: all tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/router/src/model_index.rs crates/router/src/lib.rs
git commit -m "feat(router): add ModelIndex with token-based fuzzy model matching"
```

---

### Task 5: Integrate ModelIndex into AppState and Startup

**Files:**
- Modify: `crates/gateway/src/state.rs:14-66`

- [ ] **Step 1: Add ModelIndex field to AppState**

In `crates/gateway/src/state.rs`, add to the struct:

```rust
use agent_shim_router::model_index::ModelIndex;
```

Add the field to `AppState`:

```rust
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<GatewayConfig>,
    pub anthropic: Arc<AnthropicMessages>,
    pub openai: Arc<OpenAiChat>,
    pub providers: Arc<ProviderRegistry>,
    pub router: Arc<StaticRouter>,
    pub model_index: Arc<ModelIndex>,
}
```

- [ ] **Step 2: Make AppState::new() async and add model discovery**

Change `AppState::new()` from sync to async. After building the `ProviderRegistry` and before constructing `Self`, add:

```rust
    pub async fn new(config: GatewayConfig) -> Self {
        // ... existing provider registration code ...

        let router = StaticRouter::from_config(&config);

        // Model discovery
        let mut discovered = std::collections::HashMap::new();
        for (name, provider) in registry.iter() {
            match provider.list_models().await {
                Ok(Some(models)) => {
                    tracing::info!(provider = %name, count = models.len(), "discovered models");
                    discovered.insert(name.clone(), models);
                }
                Ok(None) => {
                    tracing::debug!(provider = %name, "provider does not support model discovery");
                }
                Err(e) => {
                    tracing::warn!(provider = %name, error = %e, "model discovery failed, skipping");
                }
            }
        }
        let model_index = ModelIndex::new(discovered);

        Self {
            config: Arc::new(config),
            anthropic: Arc::new(anthropic),
            openai: Arc::new(openai),
            providers: Arc::new(registry),
            router: Arc::new(router),
            model_index: Arc::new(model_index),
        }
    }
```

- [ ] **Step 3: Add `iter()` method to ProviderRegistry**

In `crates/providers/src/lib.rs`, add to `impl ProviderRegistry`:

```rust
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Arc<dyn BackendProvider>)> {
        self.providers.iter()
    }
```

- [ ] **Step 4: Update serve.rs to await AppState::new()**

In `crates/gateway/src/commands/serve.rs`, change:

```rust
    let state = AppState::new(cfg).await;
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo check -p agent-shim`
Expected: compiles. Fix any compilation issues from the sync→async change.

- [ ] **Step 6: Commit**

```bash
git add crates/gateway/src/state.rs crates/gateway/src/commands/serve.rs crates/providers/src/lib.rs
git commit -m "feat(gateway): discover models at startup and build ModelIndex"
```

---

### Task 6: Wire ModelIndex into Request Handlers

**Files:**
- Modify: `crates/gateway/src/handlers/anthropic_messages.rs:67-70`
- Modify: `crates/gateway/src/handlers/openai_chat.rs:35-38`

- [ ] **Step 1: Add fuzzy resolution to anthropic_messages handler**

In `crates/gateway/src/handlers/anthropic_messages.rs`, after the `router.resolve()` call (around line 70) and before provider lookup, add:

```rust
    let mut target = target;
    if let Some(resolved) = state.model_index.resolve(&target.provider, &target.model) {
        if resolved != target.model {
            tracing::info!(
                requested = %target.model,
                resolved = %resolved,
                provider = %target.provider,
                "fuzzy model match"
            );
            target = BackendTarget {
                provider: target.provider,
                model: resolved.to_string(),
            };
        }
    }
```

- [ ] **Step 2: Add fuzzy resolution to openai_chat handler**

In `crates/gateway/src/handlers/openai_chat.rs`, after the `router.resolve()` call (around line 38) and before provider lookup, add the identical block:

```rust
    let mut target = target;
    if let Some(resolved) = state.model_index.resolve(&target.provider, &target.model) {
        if resolved != target.model {
            tracing::info!(
                requested = %target.model,
                resolved = %resolved,
                provider = %target.provider,
                "fuzzy model match"
            );
            target = BackendTarget {
                provider: target.provider,
                model: resolved.to_string(),
            };
        }
    }
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p agent-shim`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add crates/gateway/src/handlers/anthropic_messages.rs crates/gateway/src/handlers/openai_chat.rs
git commit -m "feat(gateway): wire fuzzy model resolution into request handlers"
```

---

### Task 7: Property Tests and Integration Tests

**Files:**
- Modify: `crates/router/src/model_index.rs` (add proptest tests)
- Create or modify: `crates/protocol-tests/tests/model_index_integration.rs`
- Modify: `crates/router/Cargo.toml` (add proptest dev-dependency if not present)

- [ ] **Step 1: Add proptest to router Cargo.toml**

Check `crates/router/Cargo.toml` for a `[dev-dependencies]` section. If `proptest` is not present, add:

```toml
[dev-dependencies]
proptest = { workspace = true }
```

- [ ] **Step 2: Add property test — exact match always wins**

In `crates/router/src/model_index.rs`, add to the `#[cfg(test)] mod tests` block:

```rust
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn exact_match_always_wins(model in "[a-z][a-z0-9-]{1,30}") {
            let idx = index_with("p", &[&model, "unrelated-model-xyz"]);
            let result = idx.resolve("p", &model);
            prop_assert_eq!(result, Some(model.as_str()));
        }
    }
```

- [ ] **Step 3: Add integration test with realistic Copilot models**

Create `crates/protocol-tests/tests/model_index_integration.rs`:

```rust
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
```

- [ ] **Step 4: Run all tests**

Run: `cargo nextest run --workspace`
Expected: all tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/router/Cargo.toml crates/router/src/model_index.rs crates/protocol-tests/tests/model_index_integration.rs
git commit -m "test(router): add property tests and integration tests for fuzzy model matching"
```

---

### Task 8: Full Build Verification

**Files:** None (verification only)

- [ ] **Step 1: Run full workspace checks**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo nextest run --workspace
```

Expected: all pass with no warnings.

- [ ] **Step 2: Fix any issues found**

Address any clippy warnings, formatting issues, or test failures.

- [ ] **Step 3: Final commit if needed**

```bash
git add -A
git commit -m "chore: fix clippy warnings and formatting"
```
