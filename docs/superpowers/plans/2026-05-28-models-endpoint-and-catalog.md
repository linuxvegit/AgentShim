# `/v1/models` Endpoint and Catalog Enrichment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a public `GET /v1/models` endpoint, a richer `/admin/catalog`, an `agent-shim show-catalog` CLI subcommand, and a `ModelIndex` that carries upstream capability metadata (not just model names). Operators can see what AgentShim accepts, what each alias resolves to, and which capabilities are available — without reading the YAML.

**Architecture:** A new `agent-shim-core::catalog` module owns the data shape (`ModelMetadata`, `ModelRecord`, `ModelCatalog`). `BackendProvider::list_models` returns the full metadata map (not just IDs). `ModelIndex` is extended to back both fuzzy name resolution (existing) and metadata lookup (new). `ModelResolver` gains a `list_catalog()` method composing routes + index. Three surfaces consume it: public `/v1/models`, admin `/admin/catalog`, CLI `show-catalog`.

**Tech Stack:** Rust workspace, `axum`, `serde`, `serde_json`, `cargo nextest`. Spec source: `docs/superpowers/specs/2026-05-28-models-endpoint-and-catalog-design.md`.

---

## File Structure

**New files:**

- `crates/core/src/catalog.rs` — `ModelMetadata`, `ModelSupports`, `ModelRecord`, `UpstreamRef`, `ModelCatalog` types.
- `crates/gateway/src/handlers/models.rs` — `GET /v1/models` and `GET /v1/models/:id` handler.
- `crates/gateway/src/admin/catalog_handler.rs` — `GET /admin/catalog`, `POST /admin/discover`.
- `crates/gateway/src/commands/show_catalog.rs` — CLI subcommand.
- Integration: `crates/gateway/tests/models_endpoint.rs`.

**Modified files:**

- `crates/core/src/lib.rs` — re-export `catalog` types.
- `crates/providers/src/lib.rs` — change `BackendProvider::list_models` signature to return `Option<BTreeMap<String, ModelMetadata>>`.
- `crates/providers/src/github_copilot/models.rs` — parse full capability blob.
- `crates/providers/src/openai_compatible/mod.rs` — return metadata-shaped (with empty metadata).
- `crates/providers/src/{deepseek,gemini,anthropic}/mod.rs` — keep `Ok(None)` but update return type.
- `crates/router/src/model_index.rs` — store `BTreeMap<String, ModelMetadata>` per provider; add `metadata()` and `provider_models()` accessors; keep `from_ids` shim.
- `crates/router/src/resolver.rs` — add `list_catalog()` method.
- `crates/router/src/lib.rs` — re-exports.
- `crates/gateway/src/handlers/mod.rs` — add `models` submodule.
- `crates/gateway/src/server.rs` — register `/v1/models` routes.
- `crates/gateway/src/admin/mod.rs` — register `/admin/catalog` and `/admin/discover`.
- `crates/gateway/src/state.rs` — expose `ModelResolver` to handlers (likely already is).
- `crates/gateway/src/cli.rs` — add `show-catalog` subcommand.
- `crates/gateway/src/commands/mod.rs` — wire the new command.
- `crates/gateway/src/main.rs` — dispatch `show-catalog` + optional `print_catalog_on_start`.
- `crates/config/src/schema.rs` — add `logging.print_catalog_on_start: bool` and a new `validation: ValidationConfig { strict_upstream_models: bool }` section.
- `crates/config/src/validation.rs` — implement `strict_upstream_models` typo detection.
- Docs: `CONTEXT.md`, `README.md`, `docs/configuration.md`, `config/gateway.example.yaml`.

---

## Task 1: Create `agent-shim-core::catalog` module

**Files:**
- Create: `crates/core/src/catalog.rs`
- Modify: `crates/core/src/lib.rs`

- [ ] **Step 1: Write the file**

Create `crates/core/src/catalog.rs`:

```rust
//! Model catalog types — the composed view of "what aliases AgentShim accepts"
//! plus the upstream-discovered capabilities for each alias's backend target.
//!
//! Lives in core because it's read by router (which builds it from routes +
//! model index), by gateway handlers (which serve it on /v1/models and
//! /admin/catalog), and is part of the public surface (so changes here are
//! breaking changes for clients).

use crate::FrontendKind;
use serde::{Deserialize, Serialize};

/// One entry in the catalog returned to clients / operators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRecord {
    /// The route alias (what clients put in `model:`).
    pub id: String,
    /// All frontends on which this alias is accepted.
    pub frontends: Vec<FrontendKind>,
    /// Head of the fallback chain (the primary upstream).
    pub upstream_provider: String,
    pub upstream_model: String,
    /// Full chain (length 1 for singular routes, N for `upstreams: [...]`).
    pub upstreams_chain: Vec<UpstreamRef>,
    /// Upstream-discovered metadata. `None` when discovery didn't run or the
    /// provider doesn't publish a catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ModelMetadata>,
    /// Same-family alias on this gateway with a strictly larger context
    /// window, if any. Surfaces "you might want -1m" without forcing
    /// dynamic SKU selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_context_variant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamRef {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default)]
    pub supports: ModelSupports,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelSupports {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_outputs: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<bool>,
}

pub type ModelCatalog = Vec<ModelRecord>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_serialises_with_no_metadata() {
        let r = ModelRecord {
            id: "x".into(),
            frontends: vec![FrontendKind::AnthropicMessages],
            upstream_provider: "u".into(),
            upstream_model: "um".into(),
            upstreams_chain: vec![UpstreamRef {
                provider: "u".into(),
                model: "um".into(),
            }],
            metadata: None,
            long_context_variant: None,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["id"], "x");
        assert!(json.get("metadata").is_none());
        assert!(json.get("long_context_variant").is_none());
    }

    #[test]
    fn metadata_round_trips() {
        let m = ModelMetadata {
            context_window_tokens: Some(200000),
            max_output_tokens: Some(32000),
            family: Some("claude-opus-4.7".into()),
            supports: ModelSupports {
                vision: Some(true),
                tool_calls: Some(true),
                streaming: Some(true),
                structured_outputs: None,
                reasoning_effort: Some(true),
            },
            version: None,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ModelMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }
}
```

- [ ] **Step 2: Re-export from lib.rs**

Add to `crates/core/src/lib.rs` (find the `pub mod ...;` block):

```rust
pub mod catalog;
pub use catalog::{ModelCatalog, ModelMetadata, ModelRecord, ModelSupports, UpstreamRef};
```

- [ ] **Step 3: Run tests**

Run: `cargo nextest run -p agent-shim-core catalog`
Expected: PASS 2 tests.

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/catalog.rs crates/core/src/lib.rs
git commit -m "feat(core): add catalog module with ModelRecord/ModelMetadata"
```

---

## Task 2: Change `BackendProvider::list_models` signature

**Files:**
- Modify: `crates/providers/src/lib.rs:65-69`
- Modify: every provider's `list_models` impl
- Test: existing tests should compile after the migration

- [ ] **Step 1: Update the trait**

Modify `crates/providers/src/lib.rs:65-69`:

```rust
    async fn list_models(
        &self,
    ) -> Result<Option<std::collections::BTreeMap<String, agent_shim_core::ModelMetadata>>, ProviderError> {
        Ok(None)
    }
```

- [ ] **Step 2: Compile-fail driven update of impls**

Run: `cargo check --workspace 2>&1 | tee /tmp/cargo-check.log`
For each error pointing at an existing `list_models` impl, change its return type and convert any in-place `BTreeSet::insert(id)` to `BTreeMap::insert(id, ModelMetadata::default())`.

Providers to touch:
- `crates/providers/src/github_copilot/models.rs` (Task 3 will rewrite this; for now produce empty metadata)
- `crates/providers/src/openai_compatible/mod.rs`
- `crates/providers/src/deepseek/mod.rs`
- `crates/providers/src/gemini/mod.rs`
- `crates/providers/src/anthropic/mod.rs`

For each, transform `Ok(Some(set))` → `Ok(Some(map))` where the map keys are the original ID strings and values are `ModelMetadata::default()`.

- [ ] **Step 3: Update existing tests that read the return type**

Run: `rg "list_models\(\)" crates/`
For each call site that unwraps a `BTreeSet`, change to iterate over the new `BTreeMap`. Concretely in `crates/providers/src/openai_compatible/mod.rs:303-348`:
- `let result = provider.list_models().await.unwrap().unwrap();` — now a `BTreeMap`. Update assertions like `assert!(result.contains("foo"))` → `assert!(result.contains_key("foo"))`.

- [ ] **Step 4: Update ModelIndex construction sites**

`ModelIndex::new` currently takes `HashMap<String, BTreeSet<String>>`. We'll change it in Task 5. For now, in the gateway startup wire-up (likely `crates/gateway/src/state.rs` or `crates/gateway/src/main.rs`), find the `let mut discovered = HashMap::new();` loop and convert from `BTreeMap<String, ModelMetadata>` back to `BTreeSet<String>` so the existing `ModelIndex::new` still compiles:

```rust
let names: BTreeSet<String> = models.into_keys().collect();
discovered.insert(provider_name, names);
```

This bridge gets removed in Task 5.

- [ ] **Step 5: Compile and test**

Run: `cargo check --workspace`
Expected: clean.

Run: `cargo nextest run -p agent-shim-providers`
Expected: PASS (existing tests now operate on `BTreeMap` keys).

- [ ] **Step 6: Commit**

```bash
git add crates/providers/ crates/gateway/
git commit -m "refactor(providers): list_models returns BTreeMap<String,ModelMetadata>"
```

---

## Task 3: Copilot models — parse full capability blob

**Files:**
- Modify: `crates/providers/src/github_copilot/models.rs`
- Test: new fixture at `crates/providers/tests/data/copilot_models.json`
- Test: `crates/providers/tests/copilot_models_parse.rs` (new integration)

- [ ] **Step 1: Capture the fixture**

Copy the Copilot `/models` response body extracted from the saved capture to the test fixtures dir:

```bash
mkdir -p crates/providers/tests/data
cp "Q:/src/AgentShim/target/models_body.json" crates/providers/tests/data/copilot_models.json
```

(That file was created during spec authoring; re-extract from `/tmp/sazmodels/raw/23_s.txt` if absent — body lives after the empty line.)

- [ ] **Step 2: Write failing integration test**

Create `crates/providers/tests/copilot_models_parse.rs`:

```rust
//! Parse a real Copilot /models response and assert metadata flows through.

use agent_shim_core::ModelMetadata;
use agent_shim_providers::github_copilot::models::parse_models_response;
use std::fs;

#[test]
fn parses_full_metadata_for_claude_4_6() {
    let raw = fs::read_to_string("tests/data/copilot_models.json").expect("fixture");
    let parsed = parse_models_response(&raw).expect("parse ok");

    let m: &ModelMetadata = parsed
        .get("claude-opus-4.6")
        .expect("entry for claude-opus-4.6");
    assert_eq!(m.context_window_tokens, Some(200_000));
    assert_eq!(m.max_output_tokens, Some(32_000));
    assert_eq!(m.family.as_deref(), Some("claude-opus-4.6"));
}

#[test]
fn parses_full_metadata_for_claude_1m_variant() {
    let raw = fs::read_to_string("tests/data/copilot_models.json").expect("fixture");
    let parsed = parse_models_response(&raw).expect("parse ok");
    let m = parsed.get("claude-opus-4.6-1m").expect("entry");
    assert_eq!(m.context_window_tokens, Some(1_000_000));
    assert_eq!(m.family.as_deref(), Some("claude-opus-4.6-1m"));
}

#[test]
fn parses_full_metadata_for_gpt_5_5() {
    let raw = fs::read_to_string("tests/data/copilot_models.json").expect("fixture");
    let parsed = parse_models_response(&raw).expect("parse ok");
    let m = parsed.get("gpt-5.5").expect("entry");
    assert_eq!(m.context_window_tokens, Some(1_050_000));
    assert_eq!(m.max_output_tokens, Some(128_000));
}

#[test]
fn entries_without_capabilities_get_default_metadata() {
    let raw = fs::read_to_string("tests/data/copilot_models.json").expect("fixture");
    let parsed = parse_models_response(&raw).expect("parse ok");
    // text-embedding-ada-002 has no capabilities.limits in the fixture.
    let m = parsed.get("text-embedding-ada-002").expect("entry");
    assert_eq!(m.context_window_tokens, None);
    assert_eq!(m.max_output_tokens, None);
}
```

- [ ] **Step 3: Run, verify failure**

Run: `cargo nextest run -p agent-shim-providers --test copilot_models_parse`
Expected: build failure — `parse_models_response` doesn't exist.

- [ ] **Step 4: Rewrite `models.rs`**

Replace `crates/providers/src/github_copilot/models.rs` entirely:

```rust
use std::collections::BTreeMap;

use agent_shim_core::{ModelMetadata, ModelSupports};

use super::token_manager::CopilotToken;
use crate::ProviderError;

/// Fetch the catalog from Copilot's `/models` endpoint.
pub async fn list_models(
    http: &reqwest::Client,
    token: &CopilotToken,
) -> Result<BTreeMap<String, ModelMetadata>, ProviderError> {
    let url = format!("{}/models", token.api_base.trim_end_matches('/'));
    let resp = http
        .get(&url)
        .bearer_auth(&token.token)
        .header(reqwest::header::USER_AGENT, super::headers::USER_AGENT)
        .header("Editor-Version", super::headers::EDITOR_VERSION)
        .header(
            "Editor-Plugin-Version",
            super::headers::EDITOR_PLUGIN_VERSION,
        )
        .header(
            "Copilot-Integration-Id",
            super::headers::COPILOT_INTEGRATION_ID,
        )
        .send()
        .await
        .map_err(|e| ProviderError::Network(e.to_string()))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(ProviderError::Upstream { status, body });
    }

    let body = resp
        .text()
        .await
        .map_err(|e| ProviderError::Network(e.to_string()))?;

    parse_models_response(&body)
}

/// Pure parser, exposed for tests.
pub fn parse_models_response(raw: &str) -> Result<BTreeMap<String, ModelMetadata>, ProviderError> {
    let json: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| ProviderError::Decode(format!("models response: {e}")))?;

    let mut out = BTreeMap::new();
    let data = json
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| ProviderError::Decode("models response missing `data` array".into()))?;

    for item in data {
        let Some(id) = item.get("id").and_then(|i| i.as_str()) else {
            continue;
        };
        let caps = item.get("capabilities");
        let limits = caps.and_then(|c| c.get("limits"));
        let supports = caps.and_then(|c| c.get("supports"));
        let family = caps
            .and_then(|c| c.get("family"))
            .and_then(|f| f.as_str())
            .map(|s| s.to_string());

        let metadata = ModelMetadata {
            context_window_tokens: limits
                .and_then(|l| l.get("max_context_window_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            max_output_tokens: limits
                .and_then(|l| l.get("max_output_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            family,
            supports: ModelSupports {
                vision: supports
                    .and_then(|s| s.get("vision"))
                    .and_then(|v| v.as_bool()),
                tool_calls: supports
                    .and_then(|s| s.get("tool_calls"))
                    .and_then(|v| v.as_bool()),
                streaming: supports
                    .and_then(|s| s.get("streaming"))
                    .and_then(|v| v.as_bool()),
                structured_outputs: supports
                    .and_then(|s| s.get("structured_outputs"))
                    .and_then(|v| v.as_bool()),
                reasoning_effort: supports
                    .and_then(|s| s.get("thinking"))
                    .and_then(|v| v.as_bool()),
            },
            version: item
                .get("version")
                .and_then(|v| v.as_str())
                .map(String::from),
        };
        out.insert(id.to_string(), metadata);
    }

    Ok(out)
}
```

Make sure `pub mod models;` already exists in `crates/providers/src/github_copilot/mod.rs` (and add `pub use models::parse_models_response;` if you want the test to import via the crate root rather than the submodule path). Adjust the test's `use` line if necessary.

- [ ] **Step 5: Update `mod.rs` provider impl**

Find the `impl BackendProvider for ...Copilot` block in `crates/providers/src/github_copilot/mod.rs`. Its `list_models` should now look like:

```rust
async fn list_models(
    &self,
) -> Result<Option<std::collections::BTreeMap<String, agent_shim_core::ModelMetadata>>, ProviderError> {
    let token = self.token_manager.get_token().await?;
    let metadata = models::list_models(&self.http, &token).await?;
    Ok(Some(metadata))
}
```

- [ ] **Step 6: Run all Copilot tests**

Run: `cargo nextest run -p agent-shim-providers github_copilot`
Run: `cargo nextest run -p agent-shim-providers --test copilot_models_parse`
Expected: PASS all.

- [ ] **Step 7: Commit**

```bash
git add crates/providers/src/github_copilot/ crates/providers/tests/copilot_models_parse.rs crates/providers/tests/data/copilot_models.json
git commit -m "feat(providers/copilot): parse full capability metadata from /models"
```

---

## Task 4: Extend `ModelIndex` to store metadata

**Files:**
- Modify: `crates/router/src/model_index.rs`
- Test: same file's `mod tests`

- [ ] **Step 1: Write failing test**

Append to the existing `mod tests` in `crates/router/src/model_index.rs` (or create one if not present):

```rust
#[cfg(test)]
mod metadata_tests {
    use super::*;
    use agent_shim_core::{ModelMetadata, ModelSupports};
    use std::collections::{BTreeMap, HashMap};

    fn metadata_with_ctx(ctx: u32) -> ModelMetadata {
        ModelMetadata {
            context_window_tokens: Some(ctx),
            ..Default::default()
        }
    }

    #[test]
    fn metadata_lookup_returns_stored_value() {
        let mut p1: BTreeMap<String, ModelMetadata> = BTreeMap::new();
        p1.insert("gpt-5.5".into(), metadata_with_ctx(1_050_000));
        let mut all = HashMap::new();
        all.insert("copilot".into(), p1);
        let idx = ModelIndex::with_metadata(all);
        let m = idx.metadata("copilot", "gpt-5.5").expect("present");
        assert_eq!(m.context_window_tokens, Some(1_050_000));
    }

    #[test]
    fn metadata_lookup_returns_none_for_unknown() {
        let idx = ModelIndex::with_metadata(HashMap::new());
        assert!(idx.metadata("copilot", "x").is_none());
    }

    #[test]
    fn provider_models_iterates_in_order() {
        let mut p1: BTreeMap<String, ModelMetadata> = BTreeMap::new();
        p1.insert("a".into(), Default::default());
        p1.insert("c".into(), Default::default());
        p1.insert("b".into(), Default::default());
        let mut all = HashMap::new();
        all.insert("copilot".into(), p1);
        let idx = ModelIndex::with_metadata(all);
        let names: Vec<_> = idx.provider_models("copilot").map(|(n, _)| n.to_string()).collect();
        assert_eq!(names, vec!["a", "b", "c"]); // BTreeMap is sorted
    }

    #[test]
    fn from_ids_still_works_for_existing_callers() {
        use std::collections::BTreeSet;
        let mut set = BTreeSet::new();
        set.insert("foo".to_string());
        let mut all = HashMap::new();
        all.insert("p".into(), set);
        let idx = ModelIndex::from_ids(all);
        assert_eq!(idx.resolve("p", "foo"), Some("foo"));
    }
}
```

- [ ] **Step 2: Run, verify failure**

Run: `cargo nextest run -p agent-shim-router model_index::metadata_tests`
Expected: build error — `with_metadata`, `metadata`, `provider_models`, `from_ids` undefined.

- [ ] **Step 3: Extend `ModelIndex`**

Modify the struct in `crates/router/src/model_index.rs` (around lines 1-11):

```rust
use std::collections::{BTreeMap, BTreeSet, HashMap};

use agent_shim_core::ModelMetadata;

struct ModelEntry {
    original: String,
    normalized: String,
    tokens: Vec<String>,
}

pub struct ModelIndex {
    /// provider name → (upstream model id → metadata)
    metadata: HashMap<String, BTreeMap<String, ModelMetadata>>,
    /// Pre-tokenised entries used by the existing fuzzy resolver.
    fuzzy: HashMap<String, Vec<ModelEntry>>,
}
```

Replace the existing `impl ModelIndex { pub fn new(...) ... pub fn resolve(...) ... }` block with the new version:

```rust
impl ModelIndex {
    /// New primary constructor: takes metadata-bearing per-provider map.
    pub fn with_metadata(
        providers: HashMap<String, BTreeMap<String, ModelMetadata>>,
    ) -> Self {
        let fuzzy = providers
            .iter()
            .map(|(provider, map)| {
                let entries = map
                    .keys()
                    .map(|name| {
                        let normalized = name.to_lowercase();
                        let tokens = tokenize(name);
                        ModelEntry {
                            original: name.clone(),
                            normalized,
                            tokens,
                        }
                    })
                    .collect();
                (provider.clone(), entries)
            })
            .collect();
        Self {
            metadata: providers,
            fuzzy,
        }
    }

    /// Back-compat shim for tests that only need names. Wraps each name with
    /// `ModelMetadata::default()`.
    pub fn from_ids(providers: HashMap<String, BTreeSet<String>>) -> Self {
        let with_meta = providers
            .into_iter()
            .map(|(p, set)| {
                let map: BTreeMap<String, ModelMetadata> = set
                    .into_iter()
                    .map(|id| (id, ModelMetadata::default()))
                    .collect();
                (p, map)
            })
            .collect();
        Self::with_metadata(with_meta)
    }

    /// Legacy constructor — kept as a one-line alias for `from_ids` so existing
    /// `ModelIndex::new(...)` call sites compile unchanged.
    pub fn new(providers: HashMap<String, BTreeSet<String>>) -> Self {
        Self::from_ids(providers)
    }

    /// Existing fuzzy resolver. Unchanged.
    pub fn resolve(&self, provider: &str, requested: &str) -> Option<&str> {
        let entries = self.fuzzy.get(provider)?;
        let req_norm = requested.to_lowercase();
        let req_tokens = tokenize(requested);
        let mut best: Option<(&ModelEntry, f64)> = None;
        for entry in entries {
            let s = score(&req_tokens, &entry.tokens, &req_norm, &entry.normalized);
            if s >= THRESHOLD && best.map_or(true, |(_, bs)| s > bs) {
                best = Some((entry, s));
            }
        }
        best.map(|(e, _)| e.original.as_str())
    }

    /// New: metadata lookup for an exact upstream model id.
    pub fn metadata(&self, provider: &str, model: &str) -> Option<&ModelMetadata> {
        self.metadata.get(provider)?.get(model)
    }

    /// New: enumerate all (model_id, metadata) for a provider.
    pub fn provider_models(
        &self,
        provider: &str,
    ) -> impl Iterator<Item = (&str, &ModelMetadata)> {
        self.metadata
            .get(provider)
            .into_iter()
            .flat_map(|map| map.iter().map(|(k, v)| (k.as_str(), v)))
    }

    /// New: enumerate all providers (used by catalog builders).
    pub fn providers(&self) -> impl Iterator<Item = &str> {
        self.metadata.keys().map(String::as_str)
    }
}
```

If the original `resolve` body did something subtly different (it's likely much the same), keep its inner logic verbatim — only the data source moves from `BTreeSet<&str>` to `BTreeMap<String, ModelMetadata>::keys()`. Verify by reading the original `pub fn resolve` (lines ~80+ in the current file).

- [ ] **Step 4: Update gateway wire-up to pass metadata**

In whichever file builds `ModelIndex` at startup (likely `crates/gateway/src/state.rs` or main), replace the bridge introduced in Task 2 with direct metadata passing:

```rust
let mut all_providers: HashMap<String, BTreeMap<String, ModelMetadata>> = HashMap::new();
for (name, provider) in registry.iter() {
    match provider.list_models().await {
        Ok(Some(models)) => { all_providers.insert(name.clone(), models); }
        Ok(None) => {}
        Err(e) => tracing::warn!(?e, provider=%name, "list_models failed"),
    }
}
let model_index = Arc::new(ModelIndex::with_metadata(all_providers));
```

- [ ] **Step 5: Run all router tests**

Run: `cargo nextest run -p agent-shim-router`
Expected: PASS — existing `model_index_integration` tests now use the back-compat constructor and behave identically; new metadata tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/router/src/model_index.rs crates/gateway/src/
git commit -m "feat(router): ModelIndex stores upstream metadata"
```

---

## Task 5: Add `ModelResolver::list_catalog`

**Files:**
- Modify: `crates/router/src/resolver.rs`
- Test: same file's `mod tests`

- [ ] **Step 1: Write failing test**

Append to `crates/router/src/resolver.rs::tests`:

```rust
    #[test]
    fn list_catalog_returns_one_record_per_alias_per_frontend() {
        let cfg = cfg_with_route(
            "anthropic_messages",
            "claude-opus-4-7",
            "copilot",
            "claude-opus-4.7",
        );
        let resolver = resolver_with(cfg, "copilot", &["claude-opus-4.7"]);
        let catalog = resolver.list_catalog();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].id, "claude-opus-4-7");
        assert_eq!(catalog[0].frontends, vec![FrontendKind::AnthropicMessages]);
        assert_eq!(catalog[0].upstream_provider, "copilot");
        assert_eq!(catalog[0].upstream_model, "claude-opus-4.7");
    }

    #[test]
    fn list_catalog_collapses_same_alias_on_multiple_frontends() {
        let mut cfg = cfg_with_route(
            "anthropic_messages",
            "shared",
            "copilot",
            "claude-opus-4.7",
        );
        cfg.routes.push(RouteEntry::singular(
            "openai_chat",
            "shared",
            "copilot",
            "claude-opus-4.7",
        ));
        let resolver = resolver_with(cfg, "copilot", &["claude-opus-4.7"]);
        let catalog = resolver.list_catalog();
        let shared = catalog.iter().find(|r| r.id == "shared").expect("present");
        assert_eq!(shared.frontends.len(), 2);
        assert!(shared.frontends.contains(&FrontendKind::AnthropicMessages));
        assert!(shared.frontends.contains(&FrontendKind::OpenAiChat));
    }

    #[test]
    fn list_catalog_populates_metadata_when_index_has_it() {
        use agent_shim_core::{ModelMetadata, ModelSupports};
        let cfg = cfg_with_route(
            "openai_chat",
            "gpt-5.5",
            "copilot",
            "gpt-5.5",
        );
        let router: Arc<dyn Router> = Arc::new(StaticRouter::from_config(&cfg));
        let mut all = HashMap::new();
        let mut map = BTreeMap::new();
        map.insert(
            "gpt-5.5".to_string(),
            ModelMetadata {
                context_window_tokens: Some(1_050_000),
                family: Some("gpt-5.5".into()),
                supports: ModelSupports {
                    vision: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        all.insert("copilot".to_string(), map);
        let index = Arc::new(ModelIndex::with_metadata(all));
        let resolver = ModelResolver::new(router, index);
        let cat = resolver.list_catalog();
        let r = &cat[0];
        let m = r.metadata.as_ref().expect("metadata present");
        assert_eq!(m.context_window_tokens, Some(1_050_000));
        assert_eq!(m.supports.vision, Some(true));
    }

    #[test]
    fn list_catalog_finds_long_context_sibling_on_same_family() {
        use agent_shim_core::{ModelMetadata};
        let mut cfg = cfg_with_route(
            "anthropic_messages",
            "claude-opus-4-7",
            "copilot",
            "claude-opus-4.7",
        );
        cfg.routes.push(RouteEntry::singular(
            "anthropic_messages",
            "claude-opus-4-7-1m",
            "copilot",
            "claude-opus-4.7-1m-internal",
        ));
        let router: Arc<dyn Router> = Arc::new(StaticRouter::from_config(&cfg));
        let mut all = HashMap::new();
        let mut map = BTreeMap::new();
        map.insert(
            "claude-opus-4.7".to_string(),
            ModelMetadata {
                context_window_tokens: Some(200_000),
                family: Some("claude-opus-4.7".into()),
                ..Default::default()
            },
        );
        map.insert(
            "claude-opus-4.7-1m-internal".to_string(),
            ModelMetadata {
                context_window_tokens: Some(1_000_000),
                family: Some("claude-opus-4.7".into()), // same family = found
                ..Default::default()
            },
        );
        all.insert("copilot".to_string(), map);
        let index = Arc::new(ModelIndex::with_metadata(all));
        let resolver = ModelResolver::new(router, index);
        let cat = resolver.list_catalog();
        let small = cat.iter().find(|r| r.id == "claude-opus-4-7").unwrap();
        assert_eq!(small.long_context_variant.as_deref(), Some("claude-opus-4-7-1m"));
        let big = cat.iter().find(|r| r.id == "claude-opus-4-7-1m").unwrap();
        assert!(big.long_context_variant.is_none());
    }
```

- [ ] **Step 2: Run, verify failure**

Run: `cargo nextest run -p agent-shim-router list_catalog`
Expected: build error — `list_catalog` undefined.

- [ ] **Step 3: Implement `list_catalog`**

Add to `impl ModelResolver` in `crates/router/src/resolver.rs`:

```rust
    /// Build the public model catalog from the static route table plus the
    /// discovered upstream metadata.
    pub fn list_catalog(&self) -> agent_shim_core::ModelCatalog {
        use std::collections::BTreeMap;
        use agent_shim_core::{ModelRecord, UpstreamRef};

        // Step 1: walk routes, build a temp map keyed by alias-id.
        let mut by_alias: BTreeMap<String, ModelRecord> = BTreeMap::new();
        for (frontend, alias) in self.static_router.list_routes() {
            // Skip wildcards from the public catalog — they don't enumerate.
            if alias == "*" {
                continue;
            }
            let Ok(chain) = self.static_router.resolve(frontend, &alias) else {
                continue;
            };
            // For metadata, use chain head's (provider, model).
            let head = &chain[0];
            let metadata = self
                .model_index
                .metadata(&head.provider, &head.model)
                .cloned();
            let entry = by_alias.entry(alias.clone()).or_insert_with(|| ModelRecord {
                id: alias,
                frontends: Vec::new(),
                upstream_provider: head.provider.clone(),
                upstream_model: head.model.clone(),
                upstreams_chain: chain
                    .iter()
                    .map(|t| UpstreamRef {
                        provider: t.provider.clone(),
                        model: t.model.clone(),
                    })
                    .collect(),
                metadata,
                long_context_variant: None,
            });
            if !entry.frontends.contains(&frontend) {
                entry.frontends.push(frontend);
            }
        }

        // Step 2: long-context sibling pass — for each record, look at all
        // other records on the same family and pick the one with the largest
        // context window above this record's own.
        let records: Vec<ModelRecord> = by_alias.into_values().collect();
        let mut out: Vec<ModelRecord> = Vec::with_capacity(records.len());
        for r in &records {
            let my_family = r.metadata.as_ref().and_then(|m| m.family.clone());
            let my_ctx = r
                .metadata
                .as_ref()
                .and_then(|m| m.context_window_tokens)
                .unwrap_or(0);
            let mut variant: Option<&ModelRecord> = None;
            if let Some(fam) = my_family {
                for other in &records {
                    if other.id == r.id {
                        continue;
                    }
                    let same_family = other
                        .metadata
                        .as_ref()
                        .and_then(|m| m.family.as_deref())
                        .map_or(false, |f| f == fam);
                    let bigger_ctx = other
                        .metadata
                        .as_ref()
                        .and_then(|m| m.context_window_tokens)
                        .map_or(false, |c| c > my_ctx);
                    if same_family && bigger_ctx {
                        let other_ctx = other.metadata.as_ref().and_then(|m| m.context_window_tokens).unwrap_or(0);
                        let cur_ctx = variant
                            .and_then(|v| v.metadata.as_ref().and_then(|m| m.context_window_tokens))
                            .unwrap_or(0);
                        if other_ctx > cur_ctx {
                            variant = Some(other);
                        }
                    }
                }
            }
            let mut rec = r.clone();
            rec.long_context_variant = variant.map(|v| v.id.clone());
            out.push(rec);
        }
        out
    }
```

This requires `Router::list_routes(&self) -> Vec<(FrontendKind, String)>` on the trait. Add it:

In `crates/router/src/lib.rs`, find the `pub trait Router` definition. Add:

```rust
    /// Enumerate explicit (frontend, alias) pairs (no wildcards).
    fn list_routes(&self) -> Vec<(FrontendKind, String)>;
```

Implement in `StaticRouter`:

```rust
fn list_routes(&self) -> Vec<(FrontendKind, String)> {
    self.route_entries
        .keys()
        .map(|k| (k.frontend, k.model.clone()))
        .collect()
}
```

- [ ] **Step 4: Run, verify pass**

Run: `cargo nextest run -p agent-shim-router list_catalog`
Expected: PASS all 4.

- [ ] **Step 5: Commit**

```bash
git add crates/router/src/
git commit -m "feat(router): ModelResolver::list_catalog with long_context_variant"
```

---

## Task 6: `GET /v1/models` handler

**Files:**
- Create: `crates/gateway/src/handlers/models.rs`
- Modify: `crates/gateway/src/handlers/mod.rs`
- Modify: `crates/gateway/src/server.rs:11-21`
- Test: `crates/gateway/tests/models_endpoint.rs`

- [ ] **Step 1: Add module declaration**

Add to `crates/gateway/src/handlers/mod.rs`:

```rust
pub mod models;
```

- [ ] **Step 2: Write the handler**

Create `crates/gateway/src/handlers/models.rs`:

```rust
//! GET /v1/models — public OpenAI-shape model catalog.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// Query params for /v1/models.
#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    pub frontend: Option<String>,
    pub capability: Option<String>,
}

#[derive(Debug, Serialize)]
struct ListResponse {
    object: &'static str,
    data: Vec<ModelDto>,
}

#[derive(Debug, Serialize)]
struct ModelDto {
    id: String,
    object: &'static str,
    owned_by: &'static str,
    frontends: Vec<String>,
    upstream: UpstreamDto,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    upstreams: Vec<UpstreamDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capabilities: Option<CapabilitiesDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    long_context_variant: Option<String>,
}

#[derive(Debug, Serialize)]
struct UpstreamDto {
    provider: String,
    model: String,
}

#[derive(Debug, Serialize)]
struct CapabilitiesDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    supports: agent_shim_core::ModelSupports,
    #[serde(skip_serializing_if = "Option::is_none")]
    family: Option<String>,
}

fn record_to_dto(r: agent_shim_core::ModelRecord) -> ModelDto {
    let frontends = r
        .frontends
        .iter()
        .map(|f| match f {
            agent_shim_core::FrontendKind::AnthropicMessages => "anthropic_messages",
            agent_shim_core::FrontendKind::OpenAiChat => "openai_chat",
            agent_shim_core::FrontendKind::OpenAiResponses => "openai_responses",
        }
        .to_string())
        .collect();
    let upstream = UpstreamDto {
        provider: r.upstream_provider,
        model: r.upstream_model,
    };
    let upstreams = if r.upstreams_chain.len() > 1 {
        r.upstreams_chain
            .into_iter()
            .map(|u| UpstreamDto {
                provider: u.provider,
                model: u.model,
            })
            .collect()
    } else {
        Vec::new()
    };
    let capabilities = r.metadata.map(|m| CapabilitiesDto {
        context_window_tokens: m.context_window_tokens,
        max_output_tokens: m.max_output_tokens,
        supports: m.supports,
        family: m.family,
    });
    ModelDto {
        id: r.id,
        object: "model",
        owned_by: "agent-shim",
        frontends,
        upstream,
        upstreams,
        capabilities,
        long_context_variant: r.long_context_variant,
    }
}

pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Response {
    let catalog = state.resolver.list_catalog();
    let filtered: Vec<ModelDto> = catalog
        .into_iter()
        .filter(|r| match q.frontend.as_deref() {
            None => true,
            Some(f) => r.frontends.iter().any(|fk| match fk {
                agent_shim_core::FrontendKind::AnthropicMessages => f == "anthropic_messages",
                agent_shim_core::FrontendKind::OpenAiChat => f == "openai_chat",
                agent_shim_core::FrontendKind::OpenAiResponses => f == "openai_responses",
            }),
        })
        .filter(|r| match q.capability.as_deref() {
            None => true,
            Some(c) => match r.metadata.as_ref() {
                None => false,
                Some(m) => match c {
                    "vision" => m.supports.vision == Some(true),
                    "tool_calls" => m.supports.tool_calls == Some(true),
                    "streaming" => m.supports.streaming == Some(true),
                    "structured_outputs" => m.supports.structured_outputs == Some(true),
                    "reasoning_effort" => m.supports.reasoning_effort == Some(true),
                    _ => false,
                },
            },
        })
        .map(record_to_dto)
        .collect();
    Json(ListResponse {
        object: "list",
        data: filtered,
    })
    .into_response()
}

pub async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let catalog = state.resolver.list_catalog();
    match catalog.into_iter().find(|r| r.id == id) {
        Some(r) => Json(record_to_dto(r)).into_response(),
        None => (StatusCode::NOT_FOUND, format!("model not found: {id}")).into_response(),
    }
}
```

Adjust `crate::state::AppState`'s `resolver` field path if it's named differently — search `crates/gateway/src/state.rs` for the field. If the resolver lives behind an `Arc` (likely), `state.resolver.list_catalog()` works as-is.

- [ ] **Step 3: Register routes in server**

Modify `crates/gateway/src/server.rs:11-21`:

```rust
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { "ok" }))
        .route("/v1/messages", post(handlers::anthropic_messages::handle))
        .route(
            "/v1/messages/count_tokens",
            post(handlers::anthropic_count_tokens::handle),
        )
        .route("/v1/chat/completions", post(handlers::openai_chat::handle))
        .route("/v1/responses", post(handlers::openai_responses::handle))
        .route("/v1/models", get(handlers::models::list))
        .route("/v1/models/:id", get(handlers::models::get_one))
        .layer(TraceLayer::new_for_http())
        .layer(crate::metrics_layer::MetricsLayer)
        .layer(RequestIdLayer)
        .with_state(state)
}
```

- [ ] **Step 4: Write integration test**

Create `crates/gateway/tests/models_endpoint.rs`:

```rust
//! /v1/models endpoint smoke tests with an in-memory router.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

mod common; // re-use existing common test helpers; create if not present

#[tokio::test]
async fn list_returns_openai_shape() {
    let app = common::build_test_app_with_routes(&[(
        "anthropic_messages",
        "claude-opus-4-7",
        "copilot",
        "claude-opus-4.7",
    )]);

    let resp = app
        .oneshot(Request::builder().uri("/v1/models").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = hyper::body::to_bytes(resp.into_body()).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["object"], "list");
    assert!(v["data"].is_array());
    let arr = v["data"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "claude-opus-4-7");
    assert_eq!(arr[0]["owned_by"], "agent-shim");
}

#[tokio::test]
async fn get_one_returns_404_for_unknown() {
    let app = common::build_test_app_with_routes(&[]);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/models/does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn frontend_filter_narrows_results() {
    let app = common::build_test_app_with_routes(&[
        (
            "anthropic_messages",
            "claude",
            "copilot",
            "claude-opus-4.7",
        ),
        (
            "openai_chat",
            "gpt",
            "copilot",
            "gpt-5.5",
        ),
    ]);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/models?frontend=openai_chat")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value =
        serde_json::from_slice(&hyper::body::to_bytes(resp.into_body()).await.unwrap()).unwrap();
    let arr = v["data"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "gpt");
}
```

If `crates/gateway/tests/common.rs` doesn't exist, create it with a minimal helper that wires `StaticRouter::from_config`, an empty `ModelIndex`, into an `AppState`, into `build_router`. Inspect any existing `crates/gateway/tests/*.rs` for the pattern (e.g. `plugins_pipeline.rs`).

- [ ] **Step 5: Run, verify pass**

Run: `cargo nextest run -p agent-shim-gateway --test models_endpoint`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/gateway/src/handlers/models.rs crates/gateway/src/handlers/mod.rs crates/gateway/src/server.rs crates/gateway/tests/models_endpoint.rs
git commit -m "feat(gateway): GET /v1/models public catalog endpoint"
```

---

## Task 7: `GET /admin/catalog` + `POST /admin/discover`

**Files:**
- Create: `crates/gateway/src/admin/catalog_handler.rs`
- Create: `crates/gateway/src/admin/discover_handler.rs`
- Modify: `crates/gateway/src/admin/mod.rs:17-26`

- [ ] **Step 1: Write catalog handler**

Create `crates/gateway/src/admin/catalog_handler.rs`:

```rust
//! GET /admin/catalog — operator catalog with policy and discovery freshness.

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use crate::state::AppState;

#[derive(Debug, Serialize)]
struct AdminCatalogResponse {
    routes: Vec<AdminRecord>,
}

#[derive(Debug, Serialize)]
struct AdminRecord {
    id: String,
    frontends: Vec<String>,
    upstream_provider: String,
    upstream_model: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    upstreams_chain: Vec<(String, String)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<agent_shim_core::ModelMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    long_context_variant: Option<String>,
    /// Surfaced from RoutePolicy after the effort spec lands.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reasoning_mapping: Vec<MappingDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_reasoning_effort: Option<String>,
}

#[derive(Debug, Serialize)]
struct MappingDto {
    r#match: String,
    set: String,
}

pub async fn handle(State(state): State<AppState>) -> Response {
    let catalog = state.resolver.list_catalog();
    let records = catalog
        .into_iter()
        .map(|r| {
            // Look up the policy for richer fields; safe to drop if no
            // first-frontend resolve hits (shouldn't happen — alias came from
            // routes).
            let first_frontend = r.frontends.first().copied();
            let policy_extras = first_frontend
                .and_then(|f| state.resolver.resolve(f, &r.id).ok())
                .and_then(|chain| chain.into_iter().next())
                .map(|t| t.policy);
            let (mapping, default_effort) = match policy_extras {
                Some(p) => (
                    p.reasoning_mapping
                        .iter()
                        .map(|m| MappingDto {
                            r#match: m.r#match.as_str().to_string(),
                            set: m.set.as_str().to_string(),
                        })
                        .collect(),
                    p.default_reasoning_effort.map(|e| e.as_str().to_string()),
                ),
                None => (Vec::new(), None),
            };
            AdminRecord {
                id: r.id,
                frontends: r
                    .frontends
                    .iter()
                    .map(|f| match f {
                        agent_shim_core::FrontendKind::AnthropicMessages => {
                            "anthropic_messages".into()
                        }
                        agent_shim_core::FrontendKind::OpenAiChat => "openai_chat".into(),
                        agent_shim_core::FrontendKind::OpenAiResponses => {
                            "openai_responses".into()
                        }
                    })
                    .collect(),
                upstream_provider: r.upstream_provider,
                upstream_model: r.upstream_model,
                upstreams_chain: if r.upstreams_chain.len() > 1 {
                    r.upstreams_chain
                        .into_iter()
                        .map(|u| (u.provider, u.model))
                        .collect()
                } else {
                    Vec::new()
                },
                metadata: r.metadata,
                long_context_variant: r.long_context_variant,
                reasoning_mapping: mapping,
                default_reasoning_effort: default_effort,
            }
        })
        .collect();
    Json(AdminCatalogResponse { routes: records }).into_response()
}
```

NOTE: The `reasoning_mapping` and `default_reasoning_effort` fields depend on the effort spec landing first. If the other plan hasn't shipped, comment out the policy-extras block and return empty/None for now.

- [ ] **Step 2: Write discover handler**

Create `crates/gateway/src/admin/discover_handler.rs`:

```rust
//! POST /admin/discover — re-run upstream model discovery (no config reload).

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

use crate::state::AppState;

#[derive(Debug, Serialize)]
struct DiscoverResponse {
    refreshed: usize,
    per_provider: BTreeMap<String, String>, // provider -> "ok" | "skipped" | "error: <msg>"
}

pub async fn handle(State(state): State<AppState>) -> Response {
    let registry = &state.providers;
    let mut all_providers: HashMap<String, BTreeMap<String, agent_shim_core::ModelMetadata>> =
        HashMap::new();
    let mut per_provider = BTreeMap::new();
    let mut refreshed = 0;
    for (name, provider) in registry.iter() {
        match provider.list_models().await {
            Ok(Some(models)) => {
                per_provider.insert(name.clone(), "ok".into());
                refreshed += 1;
                all_providers.insert(name.clone(), models);
            }
            Ok(None) => {
                per_provider.insert(name.clone(), "skipped".into());
            }
            Err(e) => {
                per_provider.insert(name.clone(), format!("error: {e}"));
            }
        }
    }
    // Atomic swap-in: replace the resolver's model_index with a fresh one.
    state.replace_model_index(agent_shim_router::ModelIndex::with_metadata(all_providers));
    (
        StatusCode::OK,
        Json(DiscoverResponse {
            refreshed,
            per_provider,
        }),
    )
        .into_response()
}
```

The `state.providers` and `state.replace_model_index(...)` calls assume those APIs exist on `AppState`. Check `crates/gateway/src/state.rs` — if `providers` is private or named differently, expose an accessor. For `replace_model_index`, you'll likely need to put `ModelIndex` behind `Arc<RwLock<...>>` or `ArcSwap`. Inspect existing reload handling at `crates/gateway/src/reload_trigger.rs` and `crates/gateway/src/admin/reload_handler.rs` for the established pattern; mirror it.

If atomic replacement is too invasive for this task, defer the swap and instead return `501 Not Implemented` from this handler with a TODO comment, and surface discovery via the existing `/admin/reload` path (which already re-runs startup discovery). Pick the smaller scope based on what `state.rs` exposes.

- [ ] **Step 3: Wire into admin router**

Modify `crates/gateway/src/admin/mod.rs:17-26`:

```rust
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handlers::healthz))
        .route("/readyz", get(handlers::readyz))
        .route("/metrics", get(metrics_handler::metrics))
        .route("/admin/reload", post(reload_handler::reload))
        .route("/admin/catalog", get(catalog_handler::handle))
        .route("/admin/discover", post(discover_handler::handle))
        .with_state(state)
}
```

Add the `mod catalog_handler;` and `mod discover_handler;` declarations at the top of that file.

- [ ] **Step 4: Add a smoke test**

Append to `crates/gateway/tests/models_endpoint.rs` (or sibling `admin_catalog_test.rs`):

```rust
#[tokio::test]
async fn admin_catalog_returns_json() {
    let app = common::build_admin_app_with_routes(&[(
        "anthropic_messages",
        "claude-opus-4-7",
        "copilot",
        "claude-opus-4.7",
    )]);
    let resp = app
        .oneshot(Request::builder().uri("/admin/catalog").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value =
        serde_json::from_slice(&hyper::body::to_bytes(resp.into_body()).await.unwrap()).unwrap();
    assert_eq!(v["routes"][0]["id"], "claude-opus-4-7");
}
```

- [ ] **Step 5: Run tests**

Run: `cargo nextest run -p agent-shim-gateway`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/gateway/src/admin/ crates/gateway/tests/
git commit -m "feat(gateway/admin): /admin/catalog and /admin/discover"
```

---

## Task 8: `agent-shim show-catalog` CLI subcommand

**Files:**
- Modify: `crates/gateway/src/cli.rs`
- Create: `crates/gateway/src/commands/show_catalog.rs`
- Modify: `crates/gateway/src/commands/mod.rs`
- Modify: `crates/gateway/src/main.rs`

- [ ] **Step 1: Inspect existing CLI scaffolding**

Read `crates/gateway/src/cli.rs` and `crates/gateway/src/commands/mod.rs`. Note the pattern used by the existing `validate-config` or similar subcommand. We'll mirror it.

- [ ] **Step 2: Add subcommand to clap definition**

In `crates/gateway/src/cli.rs`, find the `enum Command` (or similar). Add:

```rust
    /// Print the model catalog from the configured upstreams.
    ShowCatalog {
        #[clap(long, value_name = "PATH")]
        config: std::path::PathBuf,
        /// Output format: "table" (default) or "json".
        #[clap(long, default_value = "table")]
        format: String,
        /// Exit non-zero if any alias has no upstream metadata or fails the
        /// typo check.
        #[clap(long)]
        strict: bool,
    },
```

- [ ] **Step 3: Implement the command**

Create `crates/gateway/src/commands/show_catalog.rs`:

```rust
//! `agent-shim show-catalog` — print the model catalog.

use anyhow::Result;
use std::path::Path;

pub async fn run(config_path: &Path, format: &str, strict: bool) -> Result<()> {
    // 1. Load config.
    let cfg = agent_shim_config::loader::load(config_path)?;

    // 2. Build provider registry like main.rs does.
    let providers = crate::commands::common::build_providers(&cfg).await?;

    // 3. Run discovery synchronously.
    use std::collections::{BTreeMap, HashMap};
    let mut discovered: HashMap<String, BTreeMap<String, agent_shim_core::ModelMetadata>> =
        HashMap::new();
    for (name, provider) in providers.iter() {
        if let Some(map) = provider.list_models().await.ok().flatten() {
            discovered.insert(name.clone(), map);
        }
    }

    // 4. Build resolver + catalog.
    let static_router: std::sync::Arc<dyn agent_shim_router::Router> =
        std::sync::Arc::new(agent_shim_router::StaticRouter::from_config(&cfg));
    let model_index =
        std::sync::Arc::new(agent_shim_router::ModelIndex::with_metadata(discovered));
    let resolver = agent_shim_router::ModelResolver::new(static_router, model_index);
    let catalog = resolver.list_catalog();

    // 5. Render.
    let mut had_typo = false;
    match format {
        "json" => println!("{}", serde_json::to_string_pretty(&catalog)?),
        _ => {
            println!("{:<22} {:<22} {:<10} {:<30} {:<22} {:>7} {:>7} {:<6} {:<6}",
                "Frontend", "Alias", "Provider", "Upstream model", "Family",
                "Ctx", "Out", "Vis", "Tool");
            println!("{}", "─".repeat(140));
            for r in &catalog {
                let frontends = r
                    .frontends
                    .iter()
                    .map(|f| match f {
                        agent_shim_core::FrontendKind::AnthropicMessages => "anthropic",
                        agent_shim_core::FrontendKind::OpenAiChat => "openai_chat",
                        agent_shim_core::FrontendKind::OpenAiResponses => "openai_resp",
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let (fam, ctx, out, vis, tool) = match &r.metadata {
                    Some(m) => (
                        m.family.clone().unwrap_or_else(|| "?".into()),
                        m.context_window_tokens
                            .map(|c| short_num(c))
                            .unwrap_or_else(|| "?".into()),
                        m.max_output_tokens
                            .map(|c| short_num(c))
                            .unwrap_or_else(|| "?".into()),
                        match m.supports.vision {
                            Some(true) => "✓".to_string(),
                            Some(false) => "✗".to_string(),
                            None => "?".to_string(),
                        },
                        match m.supports.tool_calls {
                            Some(true) => "✓".to_string(),
                            Some(false) => "✗".to_string(),
                            None => "?".to_string(),
                        },
                    ),
                    None => {
                        had_typo = true;
                        (
                            "?".into(),
                            "?".into(),
                            "?".into(),
                            "?".into(),
                            "?".into(),
                        )
                    }
                };
                println!("{:<22} {:<22} {:<10} {:<30} {:<22} {:>7} {:>7} {:<6} {:<6}",
                    frontends, r.id, r.upstream_provider, r.upstream_model, fam,
                    ctx, out, vis, tool);
            }
        }
    }

    if strict && had_typo {
        anyhow::bail!("--strict: one or more routes have no upstream metadata");
    }
    Ok(())
}

fn short_num(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        n.to_string()
    }
}
```

NOTE: `crate::commands::common::build_providers(&cfg)` is hypothetical — if a helper that builds the registry from config doesn't already exist in `crates/gateway/src/commands/`, factor out the relevant lines from `main.rs`'s startup. The point is the show-catalog command runs the same discovery as startup, sans the server.

- [ ] **Step 4: Wire into commands mod and main**

In `crates/gateway/src/commands/mod.rs`:

```rust
pub mod show_catalog;
```

In `crates/gateway/src/main.rs`, find the `match cli.command` block (or however it dispatches). Add:

```rust
Command::ShowCatalog { config, format, strict } => {
    commands::show_catalog::run(&config, &format, strict).await
}
```

- [ ] **Step 5: Smoke-test the command manually**

```bash
cargo run -p agent-shim --bin agent-shim -- show-catalog --config config/gateway.example.yaml --format table 2>&1 | head -20
```

Expected: prints a table header + any rows the configured upstreams can discover. May print all `?`s if no real upstreams are reachable (no live tokens), which is fine for a manual smoke.

- [ ] **Step 6: Commit**

```bash
git add crates/gateway/src/cli.rs crates/gateway/src/main.rs crates/gateway/src/commands/
git commit -m "feat(gateway): agent-shim show-catalog CLI subcommand"
```

---

## Task 9: `validation.strict_upstream_models` config flag

**Files:**
- Modify: `crates/config/src/schema.rs` (add `validation: ValidationConfig`)
- Modify: `crates/config/src/validation.rs` (typo detector pass)
- Test: in-file `mod tests`

- [ ] **Step 1: Write failing test**

Append to `crates/config/src/validation.rs::tests`:

```rust
    #[test]
    fn strict_mode_rejects_route_pointing_at_unknown_upstream_model() {
        // Build a config whose route references an upstream model the
        // discovered ModelIndex doesn't know about.
        // (Use whichever helper validation.rs already uses to inject
        // discovered models; mirror its signature.)
        let cfg = make_cfg_with_route("openai_chat", "x", "openai", "gpt-99999");
        let mut cfg = cfg;
        cfg.validation = ValidationConfig {
            strict_upstream_models: true,
            ..Default::default()
        };
        let discovered = vec![("openai", vec!["gpt-4o"])];
        let result = validate_with_index(&cfg, &discovered);
        assert!(result.is_err());
    }

    #[test]
    fn non_strict_mode_emits_warning_not_error() {
        let cfg = make_cfg_with_route("openai_chat", "x", "openai", "gpt-99999");
        let discovered = vec![("openai", vec!["gpt-4o"])];
        let result = validate_with_index(&cfg, &discovered);
        assert!(result.is_ok());
    }
```

- [ ] **Step 2: Add the schema fields**

Modify `crates/config/src/schema.rs`. Add a top-level `validation:` section:

```rust
pub struct GatewayConfig {
    // ... existing fields ...
    #[serde(default)]
    pub validation: ValidationConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ValidationConfig {
    pub strict_upstream_models: bool,
}
```

If `GatewayConfig` uses `deny_unknown_fields`, the new field must have `#[serde(default)]` — confirmed above. Update `Default` for `GatewayConfig` if it's hand-implemented to fill in `validation: ValidationConfig::default()`.

- [ ] **Step 3: Implement the validator**

Find the existing post-load validation entry in `crates/config/src/validation.rs` (search for `fn validate`). Add a new function or extend the existing one:

```rust
pub fn validate_with_index(
    cfg: &GatewayConfig,
    discovered: &[(String, Vec<String>)], // provider → models
) -> Result<(), ValidationError> {
    if !cfg.validation.strict_upstream_models {
        return Ok(());
    }
    for entry in &cfg.routes {
        let Some(provider) = entry.upstream.as_deref() else {
            continue;
        };
        let Some(model) = entry.upstream_model.as_deref() else {
            continue;
        };
        let found = discovered
            .iter()
            .any(|(p, models)| p == provider && models.iter().any(|m| m == model));
        if !found {
            return Err(ValidationError::UnknownUpstreamModel {
                route_id: format!("{}/{}", entry.frontend, entry.model),
                upstream: provider.to_string(),
                model: model.to_string(),
            });
        }
    }
    Ok(())
}
```

Add the new error variant to `ValidationError` (use the existing enum's pattern).

- [ ] **Step 4: Wire into startup**

In `crates/gateway/src/main.rs`, after the upstream discovery loop completes, before `serve(...)`:

```rust
if cfg.validation.strict_upstream_models {
    let discovered_for_validation: Vec<(String, Vec<String>)> = all_providers
        .iter()
        .map(|(p, m)| (p.clone(), m.keys().cloned().collect()))
        .collect();
    agent_shim_config::validation::validate_with_index(&cfg, &discovered_for_validation)?;
}
```

- [ ] **Step 5: Run tests**

Run: `cargo nextest run -p agent-shim-config`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/config/ crates/gateway/src/main.rs
git commit -m "feat(config): validation.strict_upstream_models typo detector"
```

---

## Task 10: `logging.print_catalog_on_start`

**Files:**
- Modify: `crates/config/src/schema.rs` (extend `LoggingConfig`)
- Modify: `crates/gateway/src/main.rs`

- [ ] **Step 1: Add the field**

Find the `LoggingConfig` struct in `crates/config/src/schema.rs` and add:

```rust
    #[serde(default)]
    pub print_catalog_on_start: bool,
```

(Use `#[serde(default)]` to keep backwards compat for configs that don't set it.)

- [ ] **Step 2: Wire startup print**

In `crates/gateway/src/main.rs`, after model discovery + before `serve(...)`:

```rust
if cfg.logging.print_catalog_on_start {
    let catalog = model_resolver.list_catalog();
    eprintln!("Model catalog:");
    for r in &catalog {
        eprintln!(
            "  {:>22} → {}/{} (ctx={})",
            r.id,
            r.upstream_provider,
            r.upstream_model,
            r.metadata
                .as_ref()
                .and_then(|m| m.context_window_tokens)
                .map_or_else(|| "?".to_string(), |c| format!("{c}"))
        );
    }
}
```

- [ ] **Step 3: Smoke-test**

Add `print_catalog_on_start: true` under `logging:` in `config/gateway.example.yaml` (commented), run `cargo run -- serve --config config/gateway.example.yaml` for a few seconds, observe the print to stderr, kill.

- [ ] **Step 4: Commit**

```bash
git add crates/config/src/schema.rs crates/gateway/src/main.rs config/gateway.example.yaml
git commit -m "feat(gateway): logging.print_catalog_on_start"
```

---

## Task 11: Update documentation

**Files:**
- Modify: `CONTEXT.md`
- Modify: `README.md`
- Modify: `docs/configuration.md`
- Modify: `config/gateway.example.yaml`

- [ ] **Step 1: Update `CONTEXT.md`**

Append to the "Entities" section (after the `Resolved policy` entry):

```markdown
**Model catalog** *(`ModelCatalog`)*
The composed view of (a) the route table — which model aliases AgentShim
accepts on which frontend — and (b) the upstream-discovered capabilities
for each alias's `BackendTarget`. Built by `ModelResolver::list_catalog`
from the same inputs `resolve` consumes. Surfaced on `/v1/models` (public,
OpenAI shape) and `/admin/catalog` (operator, with policy extras).

**Model record** *(`ModelRecord`)*
One entry in the catalog. Carries the alias, the resolved upstream chain
head, the full chain, the upstream metadata, and the same-family
long-context sibling alias if any. Lives in
`agent-shim-core::catalog`.

**Model metadata** *(`ModelMetadata`)*
Structured upstream capability blob: context size, output limits,
supported features (vision, tool calls, streaming, structured outputs,
reasoning effort), family, version. Same struct backs `ModelIndex`
entries and `ModelRecord.metadata`.
```

Update the existing `Model index` entry to reflect "now stores
`BTreeMap<String, ModelMetadata>` per provider, not just `BTreeSet<String>`".

- [ ] **Step 2: Update `README.md`**

Add a new section near the existing "Routing" section (after `## Reasoning / thinking effort`):

```markdown
## Model catalog

AgentShim exposes its accepted aliases on `GET /v1/models` (OpenAI shape):

```bash
$ curl http://localhost:8000/v1/models | jq '.data[] | { id, upstream }'
{
  "id": "claude-opus-4-7",
  "upstream": { "provider": "copilot", "model": "claude-opus-4.7" }
}
{
  "id": "gpt-5.5",
  "upstream": { "provider": "copilot", "model": "gpt-5.5" }
}
```

Filters: `?frontend=anthropic_messages`, `?capability=vision`.

For operator-only views (policy, discovery status, mapping tables), use
the admin endpoint:

```bash
curl http://localhost:8001/admin/catalog | jq
```

Or print it from the CLI:

```bash
agent-shim show-catalog --config gateway.yaml
agent-shim show-catalog --config gateway.yaml --format json
agent-shim show-catalog --config gateway.yaml --strict  # exits non-zero on typos
```

Force the print at startup with `logging.print_catalog_on_start: true`.
```

- [ ] **Step 3: Update `docs/configuration.md`**

Add a section documenting `logging.print_catalog_on_start` and `validation.strict_upstream_models`.

- [ ] **Step 4: Update `config/gateway.example.yaml`**

Add commented examples:

```yaml
logging:
  # ... existing fields ...
  # print_catalog_on_start: true   # uncomment to print the model catalog at startup

validation:
  # strict_upstream_models: true   # fail to start if a route points at an
                                   # upstream model the upstream catalog doesn't list
```

- [ ] **Step 5: Commit**

```bash
git add CONTEXT.md README.md docs/configuration.md config/gateway.example.yaml
git commit -m "docs: document /v1/models, /admin/catalog, show-catalog CLI"
```

---

## Task 12: Workspace test sweep

- [ ] **Step 1: Format check**

Run: `cargo fmt --all -- --check`
Expected: clean.

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Full test suite**

Run: `cargo nextest run --workspace`
Expected: PASS.

- [ ] **Step 4: Smoke the gateway end-to-end**

```bash
cargo build --release -p agent-shim
./target/release/agent-shim serve --config config/gateway.example.yaml &
sleep 1
curl -sS http://localhost:8000/v1/models | jq '.data | length'
curl -sS http://localhost:8001/admin/catalog | jq '.routes | length'
kill %1 2>/dev/null
```

Expected: both return numbers (the number depends on `gateway.example.yaml`'s routes).

- [ ] **Step 5: Final commit if anything changed**

```bash
git add -u
git commit -m "chore: workspace lint + smoke after catalog work" --allow-empty
```
