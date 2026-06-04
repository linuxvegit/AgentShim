# GLM Upstream Provider — Design

**Date:** 2026-06-04
**Status:** Approved, ready for implementation
**Scope:** Add Zhipu GLM (open.bigmodel.cn) as a first-class `BackendProvider` so routes can target `glm-4.5`, `glm-4.5-air`, `glm-4.6`, `glm-z1`, `glm-5.1`, etc. Z.AI's Anthropic-compatible endpoint is reachable via the existing `anthropic` upstream with a `base_url` override — no new code path for it.

---

## 1. Architecture

GLM lands as the 6th upstream provider, sibling to `deepseek/`, at `crates/providers/src/glm/`.

- **Protocol surface:** Zhipu's OpenAI-Chat-compatible endpoint at `https://open.bigmodel.cn/api/paas/v4`. Streaming SSE plus unary JSON.
- **Boundary discipline preserved:** `glm` imports only `agent_shim_core` and crate-internal `oai_chat_wire` / `http_client` / `deepseek::response`. It never imports frontends. `gateway/src/state.rs` is the sole wiring point.
- **No `proxy_raw` implementation.** GLM offers no Responses-API shape that would benefit from byte-passthrough; the default `Ok(None)` keeps the canonical path.
- **Z.AI Anthropic endpoint:** documented in `config/gateway.example.yaml` as `type: anthropic` with `base_url: https://api.z.ai/api/anthropic`. No new `UpstreamConfig` variant.

**Capabilities** (`ProviderCapabilities`):

| Field | Value | Reason |
|---|---|---|
| `streaming` | `true` | SSE supported |
| `tool_use` | `true` | OpenAI-Chat tool_calls supported |
| `vision` | `true` | GLM-4.5V / 4.6 / 5.x support image inputs |
| `json_mode` | `true` | `response_format: { type: "json_object" }` supported |
| `accepts_xhigh` | `false` | Zhipu does not accept `reasoning_effort`; effort is translated to `thinking:{type:...}` here |

**Code volume estimate:** ~200–250 LOC new (mod.rs / request.rs / response.rs re-export shim), ~80 LOC of touch-ups in config + state + validation, ~250 LOC of tests.

---

## 2. Components

### 2.1 New module tree: `crates/providers/src/glm/`

| File | Size | Responsibility |
|---|---|---|
| `mod.rs` | ~200 LOC | `GlmProvider` struct, `new()` / `from_config()`, `BackendProvider` impl, `chat_url()` / `models_url()`, `/v1/models` fetch. Modelled on `deepseek/mod.rs`. |
| `request.rs` | ~80 LOC | `build(req, target, accepts_xhigh) -> serde_json::Value`. Calls `oai_chat_wire::canonical_to_chat::build` for the base body, then `inject_thinking_field` writes `thinking: {type: "enabled" \| "disabled"}` based on `req.resolved_policy.effort`, and `strip_reasoning_effort` removes the OpenAI-style field that Zhipu rejects. The shared `strip_cache_control` defense-in-depth pass from deepseek is applied identically. |
| `response.rs` | ~15 LOC | Re-export the deepseek SSE/unary parsers via `pub use`. Both `parse_stream` and `parse_unary` are 100% identical in behaviour requirements (interleaved `reasoning_content`, tool_calls allocated after the interleaver, OpenAI-standard usage). |

**No `usage.rs`.** GLM's usage block follows the standard OpenAI shape (`prompt_tokens` / `completion_tokens` / `total_tokens`). It has no `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` equivalents, so the default `oai_chat_wire` usage mapping suffices.

### 2.2 Shared-code policy

- `deepseek::response::{parse_stream, parse_unary}` are promoted to `pub` (currently `pub(crate)`) so `glm::response` can re-export them. The tracing target string `"deepseek SSE event received"` is **not** renamed — events carry `provider=glm` as a structured field, which is enough for triage.
- Alternative considered: extract a shared `oai_chat_wire::reasoning_content_parser` module that both deepseek and glm re-export. **Rejected on YAGNI grounds.** Wait for a 3rd "OAI-Chat + reasoning_content" provider before paying the abstraction tax.

### 2.3 Effort → `thinking` translation table

`req.resolved_policy.effort` is one of `Minimal | Low | Medium | High | Xhigh | Max`. GLM's `thinking` field is binary (`enabled` / `disabled`).

| Canonical effort | Emitted `thinking.type` |
|---|---|
| `Minimal` | `"disabled"` |
| `Low`, `Medium`, `High`, `Xhigh`, `Max` | `"enabled"` |

No additional knob is invented to expose "stronger thinking" — Zhipu doesn't accept one. Operators wanting fine-grained control set `reasoning_effort: minimal` per route.

### 2.4 Config schema additions (`crates/config/src/schema.rs`)

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
    pub tier: Tier,
    #[serde(default)]
    pub cost: Option<UpstreamCost>,
    #[serde(default)]
    pub p95_latency_budget_ms: Option<u64>,
}

fn default_glm_base_url() -> String {
    "https://open.bigmodel.cn/api/paas/v4".to_string()
}

// In UpstreamConfig:
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum UpstreamConfig {
    OpenAiCompatible(OpenAiCompatibleUpstream),
    GithubCopilot(GithubCopilotUpstream),
    Anthropic(AnthropicUpstream),
    Deepseek(DeepseekUpstream),
    Gemini(GeminiUpstream),
    Glm(GlmUpstream),  // new
}
```

Field set is intentionally identical to `DeepseekUpstream` — same routing knobs (tier / cost / p95), same defaults pattern, only `default_glm_base_url` differs. The schema reviewer should match the existing pattern, not invent something new.

### 2.5 Gateway wiring (`crates/gateway/src/state.rs`)

Two-line change at the existing `match upstream { ... }` block (~L247):

```rust
use crate::providers::{
    anthropic, deepseek, gemini, glm,  // add `glm`
    ...
};

// new match arm:
UpstreamConfig::Glm(cfg) => match glm::from_config(name, cfg) {
    Ok(p) => registry.register(name.clone(), Arc::new(p)),
    Err(e) => tracing::error!("failed to build GLM provider {name}: {e}"),
},
```

`UpstreamConfig` is non-exhaustive at the match level — Rust will refuse to compile any other call site (`upstream_accessors.rs`, `validation.rs`, etc.) until `Glm` is handled, which is the safety net.

### 2.6 Documentation / examples

- `config/gateway.example.yaml`: add a `glm-prod` upstream block plus a sample route mapping `glm-5.1` to itself. Annotate with the Z.AI Anthropic-shape alternative as a YAML comment block:
  ```yaml
  # Alternative: Z.AI's Anthropic-compatible endpoint
  # zai:
  #   type: anthropic
  #   base_url: https://api.z.ai/api/anthropic
  #   api_key: ${ZAI_API_KEY}
  #   tier: standard
  ```

---

## 3. Data flow

### 3.1 Startup (once)

```
config YAML
  └─ UpstreamConfig::Glm(cfg)                       [schema.rs]
       └─ glm::from_config(name, cfg)               [glm/mod.rs]
            ├─ leak name → &'static str
            ├─ http_client::build(timeout)           [providers::http_client]
            └─ GlmProvider { name, base_url, api_key, headers, caps }
                 └─ register in ProviderRegistry    [state.rs ~L273]
                      └─ list_models().await
                           └─ GET {base}/v1/models  (OpenAI-compatible)
                                └─ ModelIndex::with_metadata(discovered)
```

When `validation.strict_upstream_models = true`, any route pointing at a GLM model name absent from the discovered catalog fails startup. No new validation logic required; reuse the existing path.

### 3.2 Request (per inbound)

```
HTTP request (Anthropic Messages / OpenAI Chat / OpenAI Responses)
  │
  ▼ FrontendProtocol::decode_request
CanonicalRequest { messages, tools, resolved_policy { effort, headers, mapping }, … }
  │
  ▼ RoutePolicy::resolve  (route selects glm upstream)
  ▼ Router::resolve → BackendTarget { provider:"glm", model:"glm-5.1", policy }
  │
  ▼ ProviderRegistry.get("glm") → Arc<GlmProvider>
  ▼ GlmProvider::complete(req, target)
       │
       ├─ request::build(req, target, accepts_xhigh=false)
       │     ├─ oai_chat_wire::canonical_to_chat::build(...)  // standard OAI body
       │     ├─ inject_thinking_field(&mut body, req.resolved_policy.effort)
       │     │     // Minimal → thinking={type:"disabled"}
       │     │     //  other  → thinking={type:"enabled"}
       │     ├─ strip_reasoning_effort(&mut body)             // Zhipu 400s on it
       │     └─ strip_cache_control(&mut body)                // defense-in-depth
       │
       ├─ POST {base}/v1/chat/completions
       │     headers: Bearer <api_key>, Content-Type: application/json, default_headers
       │     (Anthropic negotiation headers NOT forwarded — GLM doesn't understand them)
       │
       └─ if req.stream { response::parse_stream(bytes_stream) }
         else          { response::parse_unary(&bytes) }
            // Both re-exported from deepseek::response:
            //   - reasoning_content delta → ReasoningInterleaver
            //   - tool_calls block index allocated AFTER interleaver blocks
            //   - usage via standard oai_chat_wire mapping (no cache fields)
  │
  ▼ CanonicalStream { ResponseStart, MessageStart,
                      ContentBlock*(Reasoning|Text|ToolCall),
                      MessageStop, ResponseStop }
  │
  ▼ FrontendProtocol::encode_stream → SSE bytes to client
```

### 3.3 Diff vs deepseek path

| Stage | deepseek | glm | Reason |
|---|---|---|---|
| Outbound body top-level | `reasoning_effort` written by OAI encoder | `reasoning_effort` stripped, `thinking:{type:...}` injected | Zhipu only recognises `thinking` |
| `cache_control` strip | applied | applied (same code, defense-in-depth) | identical |
| Anthropic negotiation headers | not forwarded | not forwarded | GLM doesn't recognise them |
| SSE `reasoning_content` parse | `ReasoningInterleaver` | same (re-export) | wire shape identical |
| Usage mapping | `usage::map_usage` (cache hit/miss) | `oai_chat_wire` standard mapping | GLM has no cache hit/miss fields |
| `[DONE]` + finalize lifecycle | implemented | inherited via re-export | same code |

---

## 4. Error handling

No new `ProviderError` or `StreamError` variants. All failure modes fall into existing categories.

| Failure | Handling | Retryable? |
|---|---|---|
| `from_config` invalid header name/value | `ProviderError::Encode(...)`; startup `tracing::error!`; other providers still register | n/a |
| `http_client::build` fails | Same as above, error bubbles | n/a |
| `POST /chat/completions` network error | `ProviderError::Network(...)` | yes (matches `retry_on: ["network"]`) |
| Upstream non-2xx | Read body → `ProviderError::Upstream { status, body }` + `tracing::warn!` | 429 / 5xx via existing retry; other 4xx pass through |
| `inject_thinking_field` hit non-object body | `ProviderError::Encode("GLM request body must be a JSON object")` | no (programmer error) |
| SSE byte-level transport error | Inherited `StreamState.completed` semantics — silenced after upstream EOS | n/a |
| SSE chunk JSON parse fail | `StreamError::Decode` via inherited parser | no |
| SSE chunk contains `error: { message }` | `StreamEvent::Error { message }` → frontend encodes per protocol | no |
| Upstream disconnect without `[DONE]` | `StreamState.finalize()` synthesises missing `MessageStop`/`ResponseStop` (inherited) | no (already streaming) |
| `/v1/models` non-2xx | `list_models()` returns `Ok(None)`; provider registers without a catalog | n/a |
| `strict_upstream_models=true` + route names a GLM model not in catalog | Existing validation hard-fails at startup | n/a |
| `thinking` rejected by older GLM SKU (4.5 and below) | Upstream 400 → `ProviderError::Upstream` passes through; documented as "set `reasoning_effort: minimal` for legacy SKUs" | no |

Nothing in the gateway pipeline, H7 hooks, admission, or cost filter is touched.

---

## 5. Testing

### 5.1 Unit tests (`crates/providers/src/glm/` `#[cfg(test)]`)

`request.rs`:
- `build_injects_thinking_enabled_for_high_effort`
- `build_injects_thinking_disabled_for_minimal_effort`
- `build_strips_reasoning_effort_top_level` — Zhipu 400 prevention
- `build_strips_cache_control_message_and_block_level` — defense-in-depth, matches deepseek
- `build_well_formed_when_body_is_minimal_request`

`mod.rs`:
- `glm_provider_constructs_with_capabilities` — streaming/tool_use/vision/json_mode true, accepts_xhigh false
- `chat_url_joins_base_and_path`
- `chat_url_trims_trailing_slash_from_base`
- `models_url_joins_base_and_path`
- `list_models_returns_discovered_models` (mockito)
- `list_models_returns_none_on_404` (mockito)

`response.rs`:
- `re_exported_parsers_resolve_to_deepseek_impl` — compile-time sanity check, ensures a future deepseek-parser signature change forces a glm rebuild and explicit acknowledgement.

### 5.2 Config tests (`crates/config/src/schema.rs`)

Three tests matching the deepseek trio:
- `glm_upstream_yaml_round_trip_with_defaults`
- `glm_upstream_yaml_with_overrides`
- `glm_upstream_unknown_field_rejected`

### 5.3 Gateway integration tests (`crates/gateway/tests/`)

- `glm_smoke.rs` — mockito-backed end-to-end. Anthropic Messages inbound → glm provider → mock Zhipu → assert `thinking` was injected on the outbound body, `reasoning_content` deltas surface as Anthropic-shape Reasoning blocks on the outbound SSE.
- `glm_live.rs` — `#[ignore]`-gated, reads `GLM_API_KEY` from env. Matches the existing `deepseek_live.rs` / `anthropic_live.rs` template. Manually invoked via `cargo nextest run -p agent-shim-gateway --run-ignored glm_live`.

### 5.4 protocol-tests

Not modified. The existing cross-dialect lifecycle harness covers any provider that produces a canonical stream, so GLM is automatically exercised once its smoke test passes.

### 5.5 Out of scope

- No GLM-specific SSE parser tests (parser is shared with deepseek; duplication would be noise).
- No GLM-specific cost / breaker / retry tests (paths are identical to deepseek by construction; existing coverage applies).

### 5.6 Coverage

Project floor is 80%. The `glm/` subtree estimate is ≥85% — parsing logic is 100% shared, leaving only config plumbing and capability assembly, both fully covered by the unit tests above.

---

## 6. Out of scope

- Z.AI Anthropic-shape variant as a dedicated `UpstreamConfig::ZaiAnthropic` — operators use `type: anthropic` + `base_url` override, documented in the example YAML.
- Refactoring `deepseek/response.rs` into a shared `oai_chat_wire::reasoning_content_parser`. YAGNI: only two consumers, defer until the third arrives.
- Adding a new `ReasoningEffort` variant to express GLM's binary `thinking` toggle. The existing 6-state enum collapses cleanly via the table in §2.3.
- Plugin extensions for Zhipu-specific features (cache control, web search tool). Future work, separate spec.

---

## 7. References

- Existing pattern files: `crates/providers/src/deepseek/{mod,request,response,usage}.rs`
- Wiring point: `crates/gateway/src/state.rs` (`match upstream { ... }` block, ~L247)
- Schema: `crates/config/src/schema.rs` (`UpstreamConfig` enum, `DeepseekUpstream` struct)
- Provider trait: `crates/providers/src/lib.rs` (`BackendProvider`, `ProviderCapabilities`)
- Canonical effort: `crates/core/src/request.rs` (`ReasoningEffort` enum)
