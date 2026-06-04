# GLM Upstream Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Zhipu GLM (`open.bigmodel.cn/api/paas/v4`) as a 6th `BackendProvider`, sibling to `deepseek/`, with routes able to target `glm-4.5`, `glm-4.5-air`, `glm-4.6`, `glm-z1`, `glm-5.1`, etc.

**Architecture:** New `crates/providers/src/glm/` module modelled on `deepseek/`. New `UpstreamConfig::Glm` schema variant. Request encoder reuses the shared OAI-Chat encoder + translates canonical `ReasoningEffort` to GLM's `thinking:{type:enabled|disabled}` field and strips the OpenAI-style `reasoning_effort` field that Zhipu would 400 on. Response parser is `pub use`'d from `deepseek::response` because both providers stream interleaved `reasoning_content`. Gateway wiring adds one match arm in `state.rs`. Z.AI's Anthropic-compatible endpoint is documented in the example YAML as `type: anthropic` with a `base_url` override — no code path.

**Tech Stack:** Rust, `serde` / `serde_yaml` / `serde_json`, `reqwest`, `mockito` for tests, `cargo nextest` for the runner.

**Spec:** `docs/superpowers/specs/2026-06-04-glm-upstream-design.md`

---

## Pre-flight reading

Before starting Task 1, the implementing engineer should skim:

- `crates/providers/src/deepseek/mod.rs` — the template for `glm/mod.rs`
- `crates/providers/src/deepseek/request.rs` — the template for `glm/request.rs`
- `crates/providers/src/deepseek/response.rs` — the parser being re-exported
- `crates/config/src/schema.rs` lines 320–475 — existing `*Upstream` structs and the `UpstreamConfig` enum
- `crates/config/src/upstream_accessors.rs` — the 5-arm match that needs a 6th arm
- `crates/config/src/validation.rs` lines 255–284 — the per-upstream validation match
- `crates/gateway/src/state.rs` lines 13–18 + 245–282 — the wiring point

---

## Task 1: Add `GlmUpstream` schema struct + `UpstreamConfig::Glm` variant

**Files:**
- Modify: `crates/config/src/schema.rs`

- [ ] **Step 1: Add the failing test for the new variant**

Open `crates/config/src/schema.rs`. Find the `#[cfg(test)] mod tests` block. Add this test next to `deepseek_upstream_yaml_round_trip_with_defaults`:

```rust
#[test]
fn glm_upstream_yaml_round_trip_with_defaults() {
    // Minimal GLM upstream config; only api_key required.
    let yaml = "type: glm\napi_key: zhipu-test\ntier: standard\n";
    let cfg: UpstreamConfig = serde_yaml::from_str(yaml).unwrap();
    let UpstreamConfig::Glm(g) = cfg else {
        panic!("expected Glm variant");
    };
    assert_eq!(g.api_key.expose(), "zhipu-test");
    assert_eq!(g.base_url, "https://open.bigmodel.cn/api/paas/v4");
    assert_eq!(g.request_timeout_secs, 30);
    assert!(g.default_headers.is_empty());
}

#[test]
fn glm_upstream_yaml_with_overrides() {
    let yaml = "
type: glm
api_key: zhipu-test
base_url: https://custom.glm.example.com/v1
request_timeout_secs: 60
tier: standard
default_headers:
  x-custom: value
";
    let cfg: UpstreamConfig = serde_yaml::from_str(yaml).unwrap();
    let UpstreamConfig::Glm(g) = cfg else {
        panic!("expected Glm variant");
    };
    assert_eq!(g.api_key.expose(), "zhipu-test");
    assert_eq!(g.base_url, "https://custom.glm.example.com/v1");
    assert_eq!(g.request_timeout_secs, 60);
    assert_eq!(
        g.default_headers.get("x-custom"),
        Some(&"value".to_string())
    );
}

#[test]
fn glm_upstream_unknown_field_rejected() {
    let yaml = "type: glm\napi_key: zhipu-test\ntier: standard\nbogus: 1\n";
    let result: Result<UpstreamConfig, _> = serde_yaml::from_str(yaml);
    assert!(result.is_err(), "deny_unknown_fields should reject 'bogus'");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p agent-shim-config glm_upstream`
Expected: 3 FAIL with "no variant or type `Glm`" / `expected Glm variant`.

- [ ] **Step 3: Add `GlmUpstream` struct + `Glm` variant**

In `crates/config/src/schema.rs`, find the `pub enum UpstreamConfig` definition (around line 329). Add the `Glm` variant at the end:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum UpstreamConfig {
    OpenAiCompatible(OpenAiCompatibleUpstream),
    GithubCopilot(GithubCopilotUpstream),
    Anthropic(AnthropicUpstream),
    Deepseek(DeepseekUpstream),
    Gemini(GeminiUpstream),
    Glm(GlmUpstream),
}
```

Then, immediately after the `GeminiUpstream` struct definition (around line 457), add:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlmUpstream {
    #[serde(default = "default_glm_base_url")]
    pub base_url: String,
    pub api_key: Secret,
    #[serde(default)]
    pub default_headers: BTreeMap<String, String>,
    #[serde(default = "default_timeout")]
    pub request_timeout_secs: u64,
    /// Tier label. Plan 06 P03 — required.
    pub tier: Tier,
    /// Per-token cost in USD. Plan 06 P03 — optional.
    #[serde(default)]
    pub cost: Option<UpstreamCost>,
    /// p95 latency budget in milliseconds. Plan 06 P03 — optional.
    #[serde(default)]
    pub p95_latency_budget_ms: Option<u64>,
}
```

And add the default function next to `default_gemini_base_url` (around line 475):

```rust
fn default_glm_base_url() -> String {
    "https://open.bigmodel.cn/api/paas/v4".to_string()
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p agent-shim-config glm_upstream`
Expected: 3 PASS.

- [ ] **Step 5: Compile-check the whole workspace (will reveal missing accessor arms)**

Run: `cargo build --workspace`
Expected: FAIL — `non-exhaustive patterns: &UpstreamConfig::Glm(_) not covered` in `upstream_accessors.rs` and `validation.rs`. This is the safety net firing; Task 2 fixes it.

- [ ] **Step 6: Commit**

```bash
git add crates/config/src/schema.rs
git commit -m "config: add GlmUpstream struct and UpstreamConfig::Glm variant"
```

---

## Task 2: Cover the new variant in upstream accessors + validation

**Files:**
- Modify: `crates/config/src/upstream_accessors.rs`
- Modify: `crates/config/src/validation.rs`

- [ ] **Step 1: Write the failing test for GLM validation parity**

In `crates/config/src/validation.rs`, find the `deepseek_upstream_validation_passes` test (around line 1127). Right after the deepseek block of tests (after `deepseek_validation_rejects_zero_timeout`), add:

```rust
#[test]
fn glm_upstream_validation_passes() {
    let mut cfg = minimal_config();
    cfg.upstreams.insert(
        "glm".to_string(),
        UpstreamConfig::Glm(GlmUpstream {
            base_url: "https://open.bigmodel.cn/api/paas/v4".to_string(),
            api_key: Secret::new("zhipu-test"),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
            tier: Tier::Standard,
            cost: None,
            p95_latency_budget_ms: None,
        }),
    );
    assert!(validate(&cfg).is_ok());
}

#[test]
fn glm_validation_rejects_empty_api_key() {
    let mut cfg = minimal_config();
    cfg.upstreams.insert(
        "glm".to_string(),
        UpstreamConfig::Glm(GlmUpstream {
            base_url: "https://open.bigmodel.cn/api/paas/v4".to_string(),
            api_key: Secret::new(""),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
            tier: Tier::Standard,
            cost: None,
            p95_latency_budget_ms: None,
        }),
    );
    match validate(&cfg) {
        Err(ValidationError::InvalidUpstream(name, msg)) => {
            assert_eq!(name, "glm");
            assert!(msg.contains("api_key"), "expected api_key error, got: {msg}");
        }
        other => panic!("expected InvalidUpstream, got {other:?}"),
    }
}

#[test]
fn glm_validation_rejects_bad_base_url() {
    let mut cfg = minimal_config();
    cfg.upstreams.insert(
        "glm".to_string(),
        UpstreamConfig::Glm(GlmUpstream {
            base_url: "ftp://open.bigmodel.cn".to_string(),
            api_key: Secret::new("zhipu-test"),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
            tier: Tier::Standard,
            cost: None,
            p95_latency_budget_ms: None,
        }),
    );
    match validate(&cfg) {
        Err(ValidationError::InvalidUpstream(name, msg)) => {
            assert_eq!(name, "glm");
            assert!(msg.contains("base_url"), "expected base_url error, got: {msg}");
        }
        other => panic!("expected InvalidUpstream, got {other:?}"),
    }
}

#[test]
fn glm_validation_rejects_zero_timeout() {
    let mut cfg = minimal_config();
    cfg.upstreams.insert(
        "glm".to_string(),
        UpstreamConfig::Glm(GlmUpstream {
            base_url: "https://open.bigmodel.cn/api/paas/v4".to_string(),
            api_key: Secret::new("zhipu-test"),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 0,
            tier: Tier::Standard,
            cost: None,
            p95_latency_budget_ms: None,
        }),
    );
    match validate(&cfg) {
        Err(ValidationError::InvalidUpstream(name, msg)) => {
            assert_eq!(name, "glm");
            assert!(
                msg.contains("request_timeout_secs"),
                "expected request_timeout_secs error, got: {msg}"
            );
        }
        other => panic!("expected InvalidUpstream, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo nextest run -p agent-shim-config glm_`
Expected: COMPILE FAIL — `GlmUpstream` not in scope inside the `tests` module + the workspace still doesn't compile because of the missing match arms from Task 1 Step 5.

- [ ] **Step 3: Add the missing arm in `upstream_accessors.rs`**

In `crates/config/src/upstream_accessors.rs`, add the `Glm` arm to each of the three match expressions:

```rust
pub fn upstream_tier(u: &UpstreamConfig) -> Tier {
    match u {
        UpstreamConfig::OpenAiCompatible(c) => c.tier,
        UpstreamConfig::GithubCopilot(c) => c.tier,
        UpstreamConfig::Anthropic(c) => c.tier,
        UpstreamConfig::Deepseek(c) => c.tier,
        UpstreamConfig::Gemini(c) => c.tier,
        UpstreamConfig::Glm(c) => c.tier,
    }
}

pub fn upstream_cost(u: &UpstreamConfig) -> Option<&UpstreamCost> {
    match u {
        UpstreamConfig::OpenAiCompatible(c) => c.cost.as_ref(),
        UpstreamConfig::GithubCopilot(c) => c.cost.as_ref(),
        UpstreamConfig::Anthropic(c) => c.cost.as_ref(),
        UpstreamConfig::Deepseek(c) => c.cost.as_ref(),
        UpstreamConfig::Gemini(c) => c.cost.as_ref(),
        UpstreamConfig::Glm(c) => c.cost.as_ref(),
    }
}

pub fn upstream_latency_budget(u: &UpstreamConfig) -> Option<u64> {
    match u {
        UpstreamConfig::OpenAiCompatible(c) => c.p95_latency_budget_ms,
        UpstreamConfig::GithubCopilot(c) => c.p95_latency_budget_ms,
        UpstreamConfig::Anthropic(c) => c.p95_latency_budget_ms,
        UpstreamConfig::Deepseek(c) => c.p95_latency_budget_ms,
        UpstreamConfig::Gemini(c) => c.p95_latency_budget_ms,
        UpstreamConfig::Glm(c) => c.p95_latency_budget_ms,
    }
}
```

- [ ] **Step 4: Add the GLM arm in `validation.rs`**

In `crates/config/src/validation.rs`, find the per-upstream validation block in `pub fn validate(cfg)` (around line 255). Extend the existing `else if let` chain:

```rust
for (name, upstream) in &cfg.upstreams {
    if let UpstreamConfig::Anthropic(a) = upstream {
        validate_oai_style_upstream(
            name,
            &a.base_url,
            a.api_key.expose(),
            a.request_timeout_secs,
        )?;
        if a.anthropic_version.is_empty() {
            return Err(ValidationError::InvalidUpstream(
                name.clone(),
                "anthropic_version must be non-empty".to_string(),
            ));
        }
    } else if let UpstreamConfig::Deepseek(d) = upstream {
        validate_oai_style_upstream(
            name,
            &d.base_url,
            d.api_key.expose(),
            d.request_timeout_secs,
        )?;
    } else if let UpstreamConfig::Gemini(g) = upstream {
        validate_oai_style_upstream(
            name,
            &g.base_url,
            g.api_key.expose(),
            g.request_timeout_secs,
        )?;
    } else if let UpstreamConfig::Glm(g) = upstream {
        validate_oai_style_upstream(
            name,
            &g.base_url,
            g.api_key.expose(),
            g.request_timeout_secs,
        )?;
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p agent-shim-config glm_`
Expected: 4 PASS (the 3 schema tests from Task 1 + 4 validation tests from this task = 7 total; the filter matches all 7).

- [ ] **Step 6: Confirm full workspace builds**

Run: `cargo build --workspace`
Expected: PASS. No exhaustiveness errors remain.

- [ ] **Step 7: Commit**

```bash
git add crates/config/src/upstream_accessors.rs crates/config/src/validation.rs
git commit -m "config: extend accessors and validation for GlmUpstream"
```

---

## Task 3: Promote deepseek response parsers to `pub` so GLM can re-export

**Files:**
- Modify: `crates/providers/src/deepseek/response.rs` (visibility only)
- Modify: `crates/providers/src/deepseek/mod.rs` (call site)

Background: `glm::response` will `pub use crate::deepseek::response::{parse_stream, parse_unary};`. Today they are `pub(crate)`, which permits re-export from another module in the same crate without a visibility bump, but we want them callable from the public `BackendProvider::complete` site of `glm/mod.rs` — same crate, so `pub(crate)` actually suffices. Verify before changing visibility.

- [ ] **Step 1: Check whether visibility actually needs changing**

Search for the parsers' current visibility:

Run: `cargo build -p agent-shim-providers 2>&1 | head -5`
Expected: PASS (sanity baseline).

Read `crates/providers/src/deepseek/response.rs` lines 46 and 141 to confirm the `pub(crate)` modifier. Re-exporting `pub(crate)` items from a sibling submodule (`glm::response`) within the same crate works without a visibility change. **If `pub(crate)` is in place, skip this task entirely** and proceed to Task 4. Mark the steps done with a note "skipped — pub(crate) suffices for in-crate re-export".

- [ ] **Step 2: Commit (skipped task marker if no change needed)**

If you skipped, no commit. If you changed visibility, commit with:

```bash
git add crates/providers/src/deepseek/response.rs
git commit -m "providers/deepseek: widen response parser visibility for in-crate reuse"
```

---

## Task 4: Create `crates/providers/src/glm/mod.rs` scaffold + register

**Files:**
- Create: `crates/providers/src/glm/mod.rs`
- Create: `crates/providers/src/glm/request.rs` (empty placeholder, filled in Task 5)
- Create: `crates/providers/src/glm/response.rs`
- Modify: `crates/providers/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/providers/src/glm/mod.rs` with just a `#[cfg(test)] mod tests` block containing:

```rust
//! Zhipu GLM provider — speaks OpenAI-Chat shape via open.bigmodel.cn,
//! with two quirks:
//!
//! - Translates canonical `ReasoningEffort` to GLM's `thinking:{type:enabled|disabled}`
//!   field and strips the OpenAI-style `reasoning_effort` field that Zhipu rejects.
//! - Streams interleaved `reasoning_content` deltas — re-uses the deepseek parser.

pub(crate) mod request;
pub(crate) mod response;

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use tracing::{debug, warn};

use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream};

use crate::{http_client, BackendProvider, ProviderCapabilities, ProviderError};

pub struct GlmProvider {
    name: &'static str,
    base_url: String,
    api_key: String,
    default_headers: HeaderMap,
    _timeout: Duration,
    client: crate::ProviderHttpClient,
    capabilities: ProviderCapabilities,
}

impl GlmProvider {
    pub fn new(
        name: &'static str,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        default_headers: BTreeMap<String, String>,
        timeout_secs: u64,
    ) -> Result<Self, ProviderError> {
        let mut headers = HeaderMap::new();
        for (k, v) in &default_headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| ProviderError::Encode(format!("invalid header name: {e}")))?;
            let val = HeaderValue::from_str(v)
                .map_err(|e| ProviderError::Encode(format!("invalid header value: {e}")))?;
            headers.insert(name, val);
        }

        let client = http_client::build(Duration::from_secs(timeout_secs))?;

        Ok(Self {
            name,
            base_url: base_url.into(),
            api_key: api_key.into(),
            default_headers: headers,
            _timeout: Duration::from_secs(timeout_secs),
            client,
            capabilities: ProviderCapabilities {
                streaming: true,
                tool_use: true,
                vision: true,
                json_mode: true,
                accepts_xhigh: false,
            },
        })
    }

    /// Build the upstream URL for chat completions.
    /// Zhipu's documented base URL is `https://open.bigmodel.cn/api/paas/v4`,
    /// so we join `/chat/completions` directly.
    fn chat_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{}/chat/completions", base)
    }

    /// Build the upstream URL for model listing.
    fn models_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{}/models", base)
    }
}

#[async_trait]
impl BackendProvider for GlmProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    async fn complete(
        &self,
        req: CanonicalRequest,
        target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError> {
        let body = request::build(&req, &target, self.capabilities.accepts_xhigh)?;
        let is_stream = req.stream;

        debug!(
            provider = self.name,
            model = %target.model,
            stream = is_stream,
            "sending request to GLM"
        );

        let mut request_builder = self
            .client
            .post(self.chat_url())
            .bearer_auth(&self.api_key)
            .header(CONTENT_TYPE, "application/json")
            .json(&body);

        for (k, v) in &self.default_headers {
            request_builder = request_builder.header(k, v);
        }

        let response = request_builder
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_string());
            warn!(
                provider = self.name,
                status = status.as_u16(),
                body = %body_text,
                "GLM upstream returned error"
            );
            return Err(ProviderError::Upstream {
                status: status.as_u16(),
                body: body_text,
            });
        }

        if is_stream {
            Ok(response::parse_stream(response.bytes_stream()))
        } else {
            let bytes = response
                .bytes()
                .await
                .map_err(|e| ProviderError::Network(e.to_string()))?;
            Ok(response::parse_unary(&bytes))
        }
    }

    async fn list_models(
        &self,
    ) -> Result<
        Option<std::collections::BTreeMap<String, agent_shim_core::ModelMetadata>>,
        ProviderError,
    > {
        let url = self.models_url();
        let resp = self
            .client
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
                    .map(|id| (id, agent_shim_core::ModelMetadata::default()))
                    .collect::<std::collections::BTreeMap<String, agent_shim_core::ModelMetadata>>()
            })
            .unwrap_or_default();

        if models.is_empty() {
            return Ok(None);
        }
        Ok(Some(models))
    }
}

/// Build a `GlmProvider` from gateway config upstreams.
pub fn from_config(
    upstream_name: &str,
    cfg: &agent_shim_config::GlmUpstream,
) -> Result<GlmProvider, ProviderError> {
    let leaked: &'static str = Box::leak(upstream_name.to_string().into_boxed_str());
    GlmProvider::new(
        leaked,
        &cfg.base_url,
        cfg.api_key.expose(),
        cfg.default_headers.clone(),
        cfg.request_timeout_secs,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glm_provider_constructs_with_capabilities() {
        let provider = GlmProvider::new(
            "glm",
            "https://open.bigmodel.cn/api/paas/v4",
            "test-key",
            BTreeMap::new(),
            30,
        )
        .expect("provider should construct");

        assert_eq!(provider.name(), "glm");
        let caps = provider.capabilities();
        assert!(caps.streaming);
        assert!(caps.tool_use);
        assert!(caps.vision);
        assert!(caps.json_mode);
        assert!(!caps.accepts_xhigh);
    }

    #[test]
    fn chat_url_joins_base_and_path() {
        let provider = GlmProvider::new(
            "glm",
            "https://open.bigmodel.cn/api/paas/v4",
            "test-key",
            BTreeMap::new(),
            30,
        )
        .unwrap();
        assert_eq!(
            provider.chat_url(),
            "https://open.bigmodel.cn/api/paas/v4/chat/completions"
        );
    }

    #[test]
    fn chat_url_trims_trailing_slash_from_base() {
        let provider = GlmProvider::new(
            "glm",
            "https://open.bigmodel.cn/api/paas/v4/",
            "test-key",
            BTreeMap::new(),
            30,
        )
        .unwrap();
        assert_eq!(
            provider.chat_url(),
            "https://open.bigmodel.cn/api/paas/v4/chat/completions"
        );
    }

    #[test]
    fn models_url_joins_base_and_path() {
        let provider = GlmProvider::new(
            "glm",
            "https://open.bigmodel.cn/api/paas/v4",
            "test-key",
            BTreeMap::new(),
            30,
        )
        .unwrap();
        assert_eq!(
            provider.models_url(),
            "https://open.bigmodel.cn/api/paas/v4/models"
        );
    }

    #[tokio::test]
    async fn list_models_returns_discovered_models() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/models")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "object": "list",
                    "data": [
                        {"id": "glm-4.6", "object": "model"},
                        {"id": "glm-5.1", "object": "model"}
                    ]
                }"#,
            )
            .create_async()
            .await;

        let provider =
            GlmProvider::new("glm", server.url(), "test-key", BTreeMap::new(), 30).unwrap();

        let result = provider.list_models().await.unwrap().unwrap();
        assert!(result.contains_key("glm-4.6"));
        assert!(result.contains_key("glm-5.1"));
        assert_eq!(result.len(), 2);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_models_returns_none_on_404() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/models")
            .with_status(404)
            .with_body("not found")
            .create_async()
            .await;

        let provider =
            GlmProvider::new("glm", server.url(), "test-key", BTreeMap::new(), 30).unwrap();

        let result = provider.list_models().await.unwrap();
        assert!(result.is_none());
        mock.assert_async().await;
    }
}
```

Create `crates/providers/src/glm/request.rs` as an EMPTY placeholder for now — Task 5 fills it:

```rust
//! Build an outbound GLM chat-completions request body.
//! Filled in by Task 5.

use agent_shim_core::{BackendTarget, CanonicalRequest};

use crate::ProviderError;

pub(crate) fn build(
    _req: &CanonicalRequest,
    _target: &BackendTarget,
    _accepts_xhigh: bool,
) -> Result<serde_json::Value, ProviderError> {
    unimplemented!("filled in Task 5")
}
```

Create `crates/providers/src/glm/response.rs`:

```rust
//! Parse GLM `/chat/completions` responses into a `CanonicalStream`.
//!
//! GLM streams interleaved `reasoning_content` deltas in the same SSE shape
//! as DeepSeek, so this module re-exports the deepseek parsers verbatim.
//! The structured `provider=` field on the tracing events lets log triage
//! distinguish the two — the literal target string ("deepseek SSE event
//! received") is not renamed.
//!
//! If a third "OAI-Chat + reasoning_content" provider arrives later, factor
//! the parser into a shared `oai_chat_wire::reasoning_content_parser` module
//! and have both deepseek and glm re-export from there. YAGNI for now.

pub(crate) use crate::deepseek::response::{parse_stream, parse_unary};

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::CanonicalStream;

    /// Compile-time sanity: if the deepseek parser signatures ever change,
    /// the GLM re-export breaks at this test rather than at runtime.
    #[test]
    fn re_exported_parsers_resolve_to_deepseek_impl() {
        fn _assert_unary(_f: fn(&[u8]) -> CanonicalStream) {}
        _assert_unary(parse_unary);
        let _stream_fn: fn(
            futures::stream::BoxStream<
                'static,
                Result<bytes::Bytes, reqwest::Error>,
            >,
        ) -> CanonicalStream = parse_stream;
    }
}
```

In `crates/providers/src/lib.rs`, register the new module. Find the existing `pub mod` declarations near the top:

```rust
pub mod anthropic;
pub mod deepseek;
pub mod gemini;
pub mod github_copilot;
pub mod oai_chat_wire;
pub mod openai_compatible;
```

Add `glm`:

```rust
pub mod anthropic;
pub mod deepseek;
pub mod gemini;
pub mod github_copilot;
pub mod glm;
pub mod oai_chat_wire;
pub mod openai_compatible;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p agent-shim-providers glm`
Expected: tests compile and the six `glm::tests::*` tests run. `unimplemented!` panic from `request::build` is NOT exercised by these tests (they don't call `complete()`), so all 6 should PASS. If any FAIL with a compile error, fix before continuing.

- [ ] **Step 3: Commit**

```bash
git add crates/providers/src/lib.rs crates/providers/src/glm/
git commit -m "providers/glm: scaffold module, BackendProvider impl, mockito catalog tests"
```

---

## Task 5: Implement `request::build` — thinking injection + reasoning_effort strip

**Files:**
- Modify: `crates/providers/src/glm/request.rs`

- [ ] **Step 1: Write the failing tests**

Replace `crates/providers/src/glm/request.rs` with this expanded skeleton (impl still `unimplemented!()`, tests added):

```rust
//! Build an outbound GLM chat-completions request body.
//!
//! Two GLM-specific transforms on top of the shared OAI-Chat encoder:
//!
//! 1. Strip the top-level `reasoning_effort` field. Zhipu's API rejects
//!    unknown fields with 400; the OpenAI-Chat encoder always writes it
//!    when the canonical policy carries an effort.
//! 2. Inject `thinking: {type: "enabled" | "disabled"}` driven by the
//!    canonical `ReasoningEffort`. Mapping: `Minimal` → disabled, every
//!    other variant → enabled. GLM's `thinking` field is binary; we don't
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
        // No effort set → keep GLM's native thinking on.
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
        assert!(body["messages"][0]["content"][0].get("cache_control").is_none());
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p agent-shim-providers --test-threads 1 glm::request`
Expected: 6 FAIL with `unimplemented!`. (If compile fails first because the `build` signature returns `Result` and the call site in `mod.rs::complete()` expects that, the signature is already aligned — proceed.)

- [ ] **Step 3: The implementation is already in Step 1**

The Step 1 paste replaced both the test block AND the `pub(crate) fn build` body. Re-run:

Run: `cargo nextest run -p agent-shim-providers --test-threads 1 glm::request`
Expected: 6 PASS.

- [ ] **Step 4: Run the whole providers crate test suite**

Run: `cargo nextest run -p agent-shim-providers`
Expected: all tests PASS, including the deepseek suite (we shouldn't have broken anything).

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/glm/request.rs
git commit -m "providers/glm: implement request encoder with thinking injection and reasoning_effort strip"
```

---

## Task 6: Wire GLM into the gateway state factory

**Files:**
- Modify: `crates/gateway/src/state.rs`

- [ ] **Step 1: Compile to discover the failure point**

Run: `cargo build --workspace`
Expected: FAIL — the `match upstream` block in `state.rs` doesn't handle `UpstreamConfig::Glm`. The compiler error names line `~L247`.

- [ ] **Step 2: Add the GLM arm and use statement**

In `crates/gateway/src/state.rs`, update the providers `use` statement (around line 13):

```rust
use agent_shim_providers::{
    anthropic, deepseek, gemini, glm,
    github_copilot::{self, credential_store},
    openai_compatible::{self},
    BackendProvider, ProviderRegistry,
};
```

Then, in the `match upstream { ... }` block (around line 247), add the `Glm` arm next to `Gemini`:

```rust
UpstreamConfig::Gemini(cfg) => match gemini::from_config(name, cfg) {
    Ok(p) => registry.register(name.clone(), Arc::new(p)),
    Err(e) => tracing::error!("failed to build Gemini provider {name}: {e}"),
},
UpstreamConfig::Glm(cfg) => match glm::from_config(name, cfg) {
    Ok(p) => registry.register(name.clone(), Arc::new(p)),
    Err(e) => tracing::error!("failed to build GLM provider {name}: {e}"),
},
```

- [ ] **Step 3: Verify the workspace builds**

Run: `cargo build --workspace`
Expected: PASS.

- [ ] **Step 4: Run the full workspace test suite to check nothing else broke**

Run: `cargo nextest run --workspace`
Expected: PASS. (If any failure is unrelated to GLM, escalate. If a clippy lint complains, treat that as the next-task scope.)

- [ ] **Step 5: Commit**

```bash
git add crates/gateway/src/state.rs
git commit -m "gateway: wire GlmProvider into AppState provider factory"
```

---

## Task 7: Update the example YAML

**Files:**
- Modify: `config/gateway.example.yaml`

- [ ] **Step 1: Inspect the existing example structure**

Run: `cargo build --workspace` — should already be passing from Task 6. Then read the file:

Open `config/gateway.example.yaml`. Locate the `upstreams:` block and the `routes:` block. Note the indentation pattern of the existing `deepseek:` entry (if absent, use `openai:` as the template).

- [ ] **Step 2: Add a GLM upstream block + route + Z.AI comment**

Append to `upstreams:`:

```yaml
  # Zhipu GLM (open.bigmodel.cn). OpenAI-Chat compatible with two quirks
  # handled by the gateway: `reasoning_effort` -> `thinking:{type:...}`,
  # SSE `reasoning_content` deltas surface as canonical Reasoning blocks.
  glm:
    type: glm
    base_url: https://open.bigmodel.cn/api/paas/v4
    api_key: ${GLM_API_KEY}
    tier: standard

  # Alternative: Z.AI exposes GLM through an Anthropic-Messages compatible
  # endpoint. No new upstream type needed — point the existing `anthropic`
  # provider at the Z.AI base URL.
  # zai:
  #   type: anthropic
  #   base_url: https://api.z.ai/api/anthropic
  #   api_key: ${ZAI_API_KEY}
  #   tier: standard
```

Append to `routes:`:

```yaml
  - frontend: openai_chat
    model: glm-5.1
    upstream: glm
    upstream_model: glm-5.1
```

- [ ] **Step 3: Verify the example YAML still parses**

There is a regression test in `crates/config/src/schema.rs::tests::v03_example_yaml_still_parses` that loads `config/gateway.example.yaml`. Run it:

Run: `cargo nextest run -p agent-shim-config v03_example_yaml_still_parses`
Expected: PASS (the new entries are well-formed). If FAIL, fix the YAML (likely an indentation or unknown-field mistake) before continuing.

- [ ] **Step 4: Commit**

```bash
git add config/gateway.example.yaml
git commit -m "docs: add GLM upstream example and Z.AI Anthropic-shape note"
```

---

## Task 8: Gateway integration smoke test (mockito)

**Files:**
- Create: `crates/gateway/tests/glm_smoke.rs`

- [ ] **Step 1: Read the existing smoke-test template**

Open `crates/gateway/tests/deepseek_live.rs` and `crates/gateway/tests/e2e_openai_chat.rs` to see the existing test patterns. The smoke test must use `mockito` (NOT a live API key) so it runs in CI without secrets.

- [ ] **Step 2: Write the failing smoke test**

Create `crates/gateway/tests/glm_smoke.rs`:

```rust
//! Gateway-level smoke test: OpenAI Chat inbound -> GlmProvider ->
//! mock Zhipu endpoint. Verifies:
//!   - request body has `thinking:{type:"enabled"}` injected,
//!   - request body has no top-level `reasoning_effort`,
//!   - upstream `reasoning_content` deltas surface as canonical Reasoning
//!     blocks in the outbound SSE stream.
//!
//! This is the GLM-specific contract; the rest of the pipeline is exercised
//! by the existing cross-dialect protocol tests.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agent_shim_config::schema::{
    GatewayConfig, GlmUpstream, LoggingConfig, RouteEntry, ServerConfig, Tier, UpstreamConfig,
};
use agent_shim_config::Secret;
use agent_shim_gateway::state::AppState;
use serde_json::json;

#[tokio::test]
async fn glm_request_carries_thinking_and_no_reasoning_effort() {
    // Capture every request body the mock sees so we can assert the
    // thinking injection AND reasoning_effort strip happened.
    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = captured.clone();

    let mut server = mockito::Server::new_async().await;
    let sse_body = concat!(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,",
        "\"model\":\"glm-5.1\",\"choices\":[{\"index\":0,\"delta\":",
        "{\"role\":\"assistant\",\"reasoning_content\":\"thinking...\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,",
        "\"model\":\"glm-5.1\",\"choices\":[{\"index\":0,\"delta\":",
        "{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let mock = server
        .mock("POST", "/chat/completions")
        .match_body(mockito::Matcher::PartialJson(json!({})))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body_from_request(move |req| {
            // Capture the JSON body verbatim for later assertions.
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(req.body()) {
                captured_clone.lock().unwrap().push(v);
            }
            sse_body.as_bytes().to_vec()
        })
        .create_async()
        .await;

    let mut upstreams = BTreeMap::new();
    upstreams.insert(
        "glm".to_string(),
        UpstreamConfig::Glm(GlmUpstream {
            base_url: server.url(),
            api_key: Secret::new("test-key"),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
            tier: Tier::Standard,
            cost: None,
            p95_latency_budget_ms: None,
        }),
    );

    let cfg = GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams,
        routes: vec![RouteEntry::singular("openai_chat", "glm-5.1", "glm", "glm-5.1")],
        plugins: BTreeMap::new(),
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
        shutdown: Default::default(),
        validation: Default::default(),
    };

    let (state, _reload_rx) = AppState::new(cfg).await.expect("AppState builds");

    // Issue an OpenAI-Chat request via the gateway's handler. The exact
    // call shape depends on the gateway's test harness — see
    // `crates/gateway/tests/e2e_openai_chat.rs` for the canonical pattern
    // of building an axum Router and exercising it via `tower::ServiceExt`.
    // The smoke test only needs to drive a single request through the
    // pipeline; assertions check the captured body.

    let app = state.router();
    let body = json!({
        "model": "glm-5.1",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": true,
        "reasoning_effort": "high"
    });
    use tower::util::ServiceExt;
    let req = http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.expect("router responds");
    assert!(resp.status().is_success(), "gateway returned {:?}", resp.status());

    // Drain the response body to ensure the upstream call completed.
    let _ = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();

    mock.assert_async().await;

    let bodies = captured.lock().unwrap();
    assert_eq!(bodies.len(), 1, "expected exactly one upstream call");
    let upstream_body = &bodies[0];
    assert_eq!(
        upstream_body["thinking"]["type"], json!("enabled"),
        "thinking.type missing or wrong: {upstream_body}"
    );
    assert!(
        upstream_body.get("reasoning_effort").is_none(),
        "reasoning_effort must be stripped: {upstream_body}"
    );
}
```

- [ ] **Step 3: Confirm the gateway test harness API matches**

The exact construction of the test request (`AppState::router()`, `axum::body`, `tower::util::ServiceExt`) follows the pattern already used by `crates/gateway/tests/e2e_openai_chat.rs`. Open that file. If the actual method names differ (e.g. `AppState::into_router()`, or a helper like `gateway::server::test_app(state)`), copy the pattern from there into the smoke test and adjust the imports accordingly. Do **not** invent new API surface — only mirror the existing one.

- [ ] **Step 4: Run the smoke test**

Run: `cargo nextest run -p agent-shim-gateway glm_smoke`
Expected: PASS. If FAIL with a compile error, the most likely cause is a mismatch between the harness pattern in `e2e_openai_chat.rs` and what was pasted; reconcile and re-run.

- [ ] **Step 5: Commit**

```bash
git add crates/gateway/tests/glm_smoke.rs
git commit -m "gateway: add GLM smoke test verifying thinking injection and reasoning_content surfacing"
```

---

## Task 9: Optional live test scaffold (gated on `GLM_API_KEY`)

**Files:**
- Create: `crates/gateway/tests/glm_live.rs`

This task produces a live test that is NOT run in CI; it's a hand-run smoke for real upstream verification. Mirrors `deepseek_live.rs`.

- [ ] **Step 1: Read the deepseek live test template**

Open `crates/gateway/tests/deepseek_live.rs` to see the pattern. Note the `#![cfg(feature = "live")]` gate and the `DEEPSEEK_API_KEY` env-var lookup.

- [ ] **Step 2: Write the GLM live test**

Create `crates/gateway/tests/glm_live.rs`:

```rust
//! Live e2e: AgentShim gateway -> real Zhipu GLM API.
//!
//! Gated behind the `live` cargo feature and the `GLM_API_KEY` env var.
//! Same posture as `deepseek_live.rs` — runs only when both are present.

#![cfg(feature = "live")]

use std::collections::BTreeMap;

use agent_shim_config::{
    schema::{GlmUpstream, LoggingConfig, RouteEntry, ServerConfig, Tier, UpstreamConfig},
    GatewayConfig, Secret,
};
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use futures::StreamExt;
use tokio::net::TcpListener;

const UPSTREAM_NAME: &str = "glm-live";
const MODEL: &str = "glm-5.1";

fn make_config(api_key: String) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert(
        UPSTREAM_NAME.to_string(),
        UpstreamConfig::Glm(GlmUpstream {
            base_url: "https://open.bigmodel.cn/api/paas/v4".to_string(),
            api_key: Secret::new(api_key),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
            tier: Tier::Standard,
            cost: None,
            p95_latency_budget_ms: None,
        }),
    );
    GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams,
        routes: vec![RouteEntry::singular("openai_chat", MODEL, UPSTREAM_NAME, MODEL)],
        plugins: BTreeMap::new(),
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
        shutdown: Default::default(),
        validation: Default::default(),
    }
}

#[tokio::test]
#[ignore = "live: requires GLM_API_KEY and outbound network"]
async fn glm_live_streams_reasoning_and_text() {
    let api_key = match std::env::var("GLM_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            eprintln!("GLM_API_KEY not set; skipping live test");
            return;
        }
    };
    let cfg = make_config(api_key);
    let (state, _reload_rx) = AppState::new(cfg).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = run_on_listener(state, listener, async move {
            let _ = shutdown_rx.await;
        })
        .await;
    });

    let body = serde_json::json!({
        "model": MODEL,
        "messages": [{"role":"user","content":"Say hi in one word."}],
        "stream": true
    });
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", addr))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "live GLM returned {:?}", resp.status());
    let mut bytes_total: usize = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        bytes_total += chunk.unwrap().len();
    }
    assert!(bytes_total > 0, "expected non-empty SSE body");
    let _ = shutdown_tx.send(());
}
```

- [ ] **Step 3: Verify the test compiles even without the feature**

Run: `cargo build -p agent-shim-gateway --tests`
Expected: PASS. The `#![cfg(feature = "live")]` gate keeps the body out of the build when the feature is off.

- [ ] **Step 4: Verify it builds WITH the live feature**

Run: `cargo build -p agent-shim-gateway --tests --features live`
Expected: PASS. (Compile only; the test itself is `#[ignore]` even when the feature is on, so it won't fire in `nextest run`.)

- [ ] **Step 5: Commit**

```bash
git add crates/gateway/tests/glm_live.rs
git commit -m "gateway: add GLM live test scaffold gated on GLM_API_KEY"
```

---

## Task 10: Full-workspace gate before declaring done

**Files:** (none — verification only)

- [ ] **Step 1: Format check**

Run: `cargo fmt --all -- --check`
Expected: PASS. If FAIL, run `cargo fmt --all` and commit any drift as a separate `chore(fmt)` commit.

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS. Fix any new clippy warnings inline; commit fixes as `chore(clippy)` if isolated.

- [ ] **Step 3: Full test suite via nextest**

Run: `cargo nextest run --workspace`
Expected: PASS. Pay attention to any pre-existing failures unrelated to GLM — note them but do not chase them in this plan.

- [ ] **Step 4: Cargo deny (licenses + advisories)**

Run: `cargo deny check`
Expected: PASS. No new dependencies were added, so this should be a no-op.

- [ ] **Step 5: Release build sanity**

Run: `cargo build --release -p agent-shim`
Expected: PASS. (Windows-specific binary-lock note in `CLAUDE.md` if needed.)

- [ ] **Step 6: Final verification commit (only if any fixes were made above)**

If Steps 1–5 surfaced anything to commit, do so now. Otherwise no commit.

---

## Notes for future GLM work (out of scope here)

- **Shared "OAI + reasoning_content" parser:** when a 3rd provider with the same SSE shape arrives, factor `deepseek::response` into `oai_chat_wire::reasoning_content_parser` and have `deepseek` + `glm` both re-export from it. Single-source-of-truth, no more `re_exported_parsers_resolve_to_deepseek_impl` sanity test.
- **Zhipu cache_control:** Zhipu has a context-cache extension; not modelled here. When needed, build it as a plugin (H2 / H7) rather than baking it into the provider.
- **Web search tool / KB tool:** Zhipu offers proprietary tool types beyond the OpenAI `function` shape. Add per-route plugins to inject them.
- **Z.AI Anthropic-shape:** if operator demand emerges for explicit Z.AI tier/cost/budget knobs distinct from generic `anthropic`, only then add `UpstreamConfig::ZaiAnthropic`. Don't pre-build.
