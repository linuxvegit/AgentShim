# Reasoning Effort Mapping Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add per-route canonical-effort rewrite tables, expand the canonical `ReasoningEffort` enum to align with Anthropic's `output_config.effort` vocabulary, and migrate the Anthropic provider to emit `{ thinking: adaptive, output_config: { effort } }` per the `effort-2025-11-24` beta.

**Architecture:** Mapping operates exclusively on the canonical effort axis between frontend decode (inbound dialect → canonical) and provider encode (canonical → outbound dialect). Inbound budget_tokens → discarded (we no longer consume budget on the canonical path); new effort axis runs as `Minimal | Low | Medium | High | Xhigh | Max` (6 variants). Provider encoders compress at their wire boundaries (OpenAI/Responses: `Xhigh|Max → "high"`; Anthropic: `Minimal → "low"`).

**Tech Stack:** Rust workspace, `cargo nextest`, `serde`, `serde_yaml`, `axum`. Spec source: `docs/superpowers/specs/2026-05-28-reasoning-effort-mapping-design.md`.

---

## File Structure

**New / heavily-modified files:**

- `crates/core/src/request.rs` — extend `ReasoningEffort` to 6 variants, add `parse("max")`, drop `Minimal` from Anthropic-side output once provider compresses.
- `crates/core/src/policy.rs` — add `MappingRule`, `MatchClause` (single effort), `SetClause`, `reasoning_mapping: Vec<MappingRule>` on `RoutePolicy`; add `reasoning_budget_tokens: Option<u32>` on `ResolvedPolicy`; rewrite `RoutePolicy::resolve`.
- `crates/frontends/src/anthropic_messages/wire.rs` — replace `ThinkingConfig.budget_tokens` decode path with `OutputConfig { effort }` parsing; tolerate `thinking.type: "adaptive"`.
- `crates/frontends/src/anthropic_messages/decode.rs` — read `output_config.effort`, drop legacy `thinking.budget_tokens` → `ReasoningOptions.budget_tokens` mapping.
- `crates/frontends/src/openai_chat/decode.rs` — `ReasoningEffort::parse` now accepts `"max"`.
- `crates/frontends/src/openai_responses/decode.rs` — same.
- `crates/providers/src/anthropic/wire.rs` — add `OutputConfig { effort }` to `OutgoingRequest`; keep `OutgoingThinking` with extra `type` variants (`"adaptive"`).
- `crates/providers/src/anthropic/request.rs` — replace `build_thinking` with new `build_reasoning_blocks` that emits `thinking: adaptive` + `output_config: { effort }` when `effort-2025-11-24` beta is present, falls back to legacy `thinking.budget_tokens` otherwise.
- `crates/providers/src/oai_chat_wire/canonical_to_chat.rs` — add `effort_for_chat` compression helper; thread `accepts_xhigh` capability flag.
- `crates/providers/src/openai_compatible/responses_api/encode_request.rs` — compress `Xhigh|Max → "high"`.
- `crates/providers/src/gemini/request.rs` — add `Max` to `effort_to_budget` (value `24576`).
- `crates/providers/src/lib.rs` — add `accepts_xhigh: bool` to `ProviderCapabilities`.
- `crates/providers/src/github_copilot/mod.rs` — set `accepts_xhigh: true` in capabilities.
- `crates/config/src/schema.rs` — add `MappingRuleConfig { match, set }` and `reasoning_mapping: Vec<MappingRuleConfig>` on `RouteEntry`.
- `crates/config/src/validation.rs` — validate effort strings via `ReasoningEffort::parse`; reject malformed at config-load.
- `crates/router/src/static_routes.rs` — wire `MappingRuleConfig` → `core::policy::MappingRule` into `RoutePolicy::reasoning_mapping`.
- `config/gateway.example.yaml` — add a worked example route with `reasoning_mapping`.
- `docs/configuration.md` — document the new YAML field.
- `README.md` — update "Reasoning / thinking effort" section.
- `CONTEXT.md` — add `Reasoning mapping table`, `Mapping rule`, `Effort vocabulary` domain terms.

**Test files (new):**

- `crates/core/src/request.rs::tests` (extend in-file `mod tests`).
- `crates/core/src/policy.rs::tests` (extend in-file `mod tests`).
- `crates/providers/tests/anthropic_effort_beta.rs` (new integration).
- `crates/protocol-tests/tests/effort_mapping_end_to_end.rs` (new).

---

## Task 1: Extend `ReasoningEffort` to six variants

**Files:**
- Modify: `crates/core/src/request.rs:44-76`
- Test: `crates/core/src/request.rs` (in-file `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write failing parse + as_str tests**

Add to `crates/core/src/request.rs` at the end of the file (create `#[cfg(test)] mod tests` if absent):

```rust
#[cfg(test)]
mod effort_tests {
    use super::*;

    #[test]
    fn parse_known_efforts() {
        assert_eq!(ReasoningEffort::parse("minimal"), Some(ReasoningEffort::Minimal));
        assert_eq!(ReasoningEffort::parse("low"), Some(ReasoningEffort::Low));
        assert_eq!(ReasoningEffort::parse("medium"), Some(ReasoningEffort::Medium));
        assert_eq!(ReasoningEffort::parse("high"), Some(ReasoningEffort::High));
        assert_eq!(ReasoningEffort::parse("xhigh"), Some(ReasoningEffort::Xhigh));
        assert_eq!(ReasoningEffort::parse("max"), Some(ReasoningEffort::Max));
    }

    #[test]
    fn parse_aliases() {
        assert_eq!(ReasoningEffort::parse("none"), Some(ReasoningEffort::Minimal));
        assert_eq!(ReasoningEffort::parse("default"), Some(ReasoningEffort::Medium));
        assert_eq!(ReasoningEffort::parse("x-high"), Some(ReasoningEffort::Xhigh));
        assert_eq!(ReasoningEffort::parse("extra_high"), Some(ReasoningEffort::Xhigh));
        assert_eq!(ReasoningEffort::parse("MAX"), Some(ReasoningEffort::Max));
    }

    #[test]
    fn parse_rejects_unknown() {
        assert_eq!(ReasoningEffort::parse("super"), None);
        assert_eq!(ReasoningEffort::parse(""), None);
    }

    #[test]
    fn as_str_round_trip() {
        for e in [
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Xhigh,
            ReasoningEffort::Max,
        ] {
            assert_eq!(ReasoningEffort::parse(e.as_str()), Some(e));
        }
    }
}
```

- [ ] **Step 2: Run tests, verify compile failure on `Max`**

Run: `cargo nextest run -p agent-shim-core effort_tests`
Expected: build error — `no variant or associated item named 'Max'`.

- [ ] **Step 3: Add `Max` variant + parse/as_str arms**

Modify `crates/core/src/request.rs` lines 44-76:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Xhigh => "xhigh",
            ReasoningEffort::Max => "max",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "minimal" | "none" => Some(ReasoningEffort::Minimal),
            "low" => Some(ReasoningEffort::Low),
            "medium" | "default" => Some(ReasoningEffort::Medium),
            "high" => Some(ReasoningEffort::High),
            "xhigh" | "x-high" | "extra_high" => Some(ReasoningEffort::Xhigh),
            "max" => Some(ReasoningEffort::Max),
            _ => None,
        }
    }
}
```

- [ ] **Step 4: Run tests, verify pass**

Run: `cargo nextest run -p agent-shim-core effort_tests`
Expected: PASS 4 tests.

- [ ] **Step 5: Compile downstream crates**

Run: `cargo check --workspace`
Expected: build error in providers/anthropic, providers/gemini complaining about non-exhaustive match on `ReasoningEffort` (variants don't cover `Max`). This is intentional — Task 4/5/6 cover those.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/request.rs
git commit -m "feat(core): add ReasoningEffort::Max variant"
```

---

## Task 2: Add `ResolvedPolicy.reasoning_budget_tokens` field

**Files:**
- Modify: `crates/core/src/policy.rs:42-51`
- Test: `crates/core/src/policy.rs` (in-file `mod tests`)

- [ ] **Step 1: Write failing test that reads the new field**

Append to `crates/core/src/policy.rs::tests`:

```rust
    #[test]
    fn resolved_policy_carries_budget_tokens() {
        let rp = ResolvedPolicy {
            reasoning_effort: Some(ReasoningEffort::High),
            reasoning_budget_tokens: Some(8192),
            anthropic_headers: vec![],
        };
        assert_eq!(rp.reasoning_budget_tokens, Some(8192));
    }
```

- [ ] **Step 2: Run, verify failure**

Run: `cargo nextest run -p agent-shim-core resolved_policy_carries_budget_tokens`
Expected: build error — `ResolvedPolicy` has no field `reasoning_budget_tokens`.

- [ ] **Step 3: Add field**

Modify `crates/core/src/policy.rs:42-51`:

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_budget_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anthropic_headers: Vec<(String, String)>,
}
```

- [ ] **Step 4: Run, verify pass**

Run: `cargo nextest run -p agent-shim-core resolved_policy_carries_budget_tokens`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/policy.rs
git commit -m "feat(core): add ResolvedPolicy.reasoning_budget_tokens"
```

---

## Task 3: Add `MappingRule` + `RoutePolicy.reasoning_mapping`

**Files:**
- Modify: `crates/core/src/policy.rs:25-34` and `crates/core/src/policy.rs::tests`

- [ ] **Step 1: Write failing tests**

Append to `crates/core/src/policy.rs::tests`:

```rust
    #[test]
    fn mapping_rule_max_to_xhigh() {
        let policy = RoutePolicy {
            reasoning_mapping: vec![MappingRule {
                r#match: ReasoningEffort::Max,
                set: ReasoningEffort::Xhigh,
            }],
            ..Default::default()
        };
        let mut r = req();
        r.generation.reasoning = Some(ReasoningOptions {
            effort: Some(ReasoningEffort::Max),
            budget_tokens: None,
        });
        let resolved = policy.resolve(&r);
        assert_eq!(resolved.reasoning_effort, Some(ReasoningEffort::Xhigh));
    }

    #[test]
    fn mapping_first_rule_wins() {
        let policy = RoutePolicy {
            reasoning_mapping: vec![
                MappingRule { r#match: ReasoningEffort::High, set: ReasoningEffort::Xhigh },
                MappingRule { r#match: ReasoningEffort::High, set: ReasoningEffort::Max },
            ],
            ..Default::default()
        };
        let mut r = req();
        r.generation.reasoning = Some(ReasoningOptions {
            effort: Some(ReasoningEffort::High),
            budget_tokens: None,
        });
        assert_eq!(policy.resolve(&r).reasoning_effort, Some(ReasoningEffort::Xhigh));
    }

    #[test]
    fn mapping_no_match_passthrough() {
        let policy = RoutePolicy {
            reasoning_mapping: vec![MappingRule {
                r#match: ReasoningEffort::Max,
                set: ReasoningEffort::Xhigh,
            }],
            ..Default::default()
        };
        let mut r = req();
        r.generation.reasoning = Some(ReasoningOptions {
            effort: Some(ReasoningEffort::Low),
            budget_tokens: None,
        });
        assert_eq!(policy.resolve(&r).reasoning_effort, Some(ReasoningEffort::Low));
    }

    #[test]
    fn mapping_runs_after_default_fallback() {
        let policy = RoutePolicy {
            default_reasoning_effort: Some(ReasoningEffort::Medium),
            reasoning_mapping: vec![MappingRule {
                r#match: ReasoningEffort::Medium,
                set: ReasoningEffort::High,
            }],
            ..Default::default()
        };
        let resolved = policy.resolve(&req()); // inbound has no effort
        assert_eq!(resolved.reasoning_effort, Some(ReasoningEffort::High));
    }
```

- [ ] **Step 2: Run, verify failure**

Run: `cargo nextest run -p agent-shim-core mapping`
Expected: build errors — `MappingRule` undefined, `RoutePolicy` has no `reasoning_mapping`.

- [ ] **Step 3: Add types and field**

Modify `crates/core/src/policy.rs` — replace the `RoutePolicy` struct (lines 24-34) and add `MappingRule` before `impl RoutePolicy`:

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutePolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_anthropic_beta: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_mapping: Vec<MappingRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingRule {
    pub r#match: ReasoningEffort,
    pub set: ReasoningEffort,
}
```

- [ ] **Step 4: Update `RoutePolicy::resolve` to consult mapping**

Replace `RoutePolicy::resolve` (currently around lines 68-90) with:

```rust
pub fn resolve(&self, req: &CanonicalRequest) -> ResolvedPolicy {
    // Step 1: gather inbound effort, falling back to route default.
    let inbound_effort = req
        .generation
        .reasoning
        .as_ref()
        .and_then(|r| r.effort);
    let inbound_budget = req
        .generation
        .reasoning
        .as_ref()
        .and_then(|r| r.budget_tokens);
    let post_default = inbound_effort.or(self.default_reasoning_effort);

    // Step 2: apply mapping table. First match wins; unmatched passes through.
    let final_effort = match post_default {
        Some(e) => self
            .reasoning_mapping
            .iter()
            .find(|rule| rule.r#match == e)
            .map(|rule| rule.set)
            .or(Some(e)),
        None => None,
    };

    // Step 3: anthropic headers (unchanged).
    let mut anthropic_headers = req.inbound_anthropic_headers.clone();
    let inbound_has_beta = anthropic_headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"));
    if !inbound_has_beta {
        if let Some(beta) = &self.default_anthropic_beta {
            anthropic_headers.push(("anthropic-beta".to_string(), beta.clone()));
        }
    }

    ResolvedPolicy {
        reasoning_effort: final_effort,
        reasoning_budget_tokens: inbound_budget,
        anthropic_headers,
    }
}
```

- [ ] **Step 5: Run mapping tests + existing policy tests**

Run: `cargo nextest run -p agent-shim-core policy::tests`
Expected: all existing tests pass (including `empty_policy_resolves_to_empty`, `route_default_reasoning_applies_when_request_silent`, `inbound_reasoning_wins_over_route_default`) + new 4 tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/policy.rs
git commit -m "feat(core): add reasoning_mapping per-route rewrite table"
```

---

## Task 4: Anthropic frontend decode reads `output_config.effort`

**Files:**
- Modify: `crates/frontends/src/anthropic_messages/wire.rs:29-40`
- Modify: `crates/frontends/src/anthropic_messages/decode.rs:105-132`
- Test: in-file `mod tests` of `decode.rs`

- [ ] **Step 1: Write failing decode test**

Append to `crates/frontends/src/anthropic_messages/decode.rs::tests` (search for existing `mod tests` and add inside):

```rust
    #[test]
    fn output_config_effort_decodes_to_canonical() {
        let body = serde_json::json!({
            "model": "claude-opus-4.7",
            "max_tokens": 1024,
            "messages": [{ "role": "user", "content": "hi" }],
            "thinking": { "type": "adaptive" },
            "output_config": { "effort": "max" }
        });
        let req: MessagesRequest = serde_json::from_value(body).expect("parses");
        let decoded = decode(req, FrontendInfo {
            kind: FrontendKind::AnthropicMessages,
            requested_model: FrontendModel::from("claude-opus-4.7"),
        }, vec![]).expect("decodes");
        let r = decoded.generation.reasoning.expect("reasoning set");
        assert_eq!(r.effort, Some(ReasoningEffort::Max));
        assert_eq!(r.budget_tokens, None);
    }

    #[test]
    fn legacy_thinking_budget_no_longer_populates_canonical() {
        let body = serde_json::json!({
            "model": "claude-opus-4.7",
            "max_tokens": 1024,
            "messages": [{ "role": "user", "content": "hi" }],
            "thinking": { "type": "enabled", "budget_tokens": 8192 }
        });
        let req: MessagesRequest = serde_json::from_value(body).expect("parses");
        let decoded = decode(req, FrontendInfo {
            kind: FrontendKind::AnthropicMessages,
            requested_model: FrontendModel::from("claude-opus-4.7"),
        }, vec![]).expect("decodes");
        assert!(decoded.generation.reasoning.is_none(),
            "legacy thinking.budget_tokens path is intentionally dropped per 2026-05-28 spec");
    }

    #[test]
    fn adaptive_thinking_without_output_config_is_silent() {
        let body = serde_json::json!({
            "model": "claude-opus-4.7",
            "max_tokens": 1024,
            "messages": [{ "role": "user", "content": "hi" }],
            "thinking": { "type": "adaptive" }
        });
        let req: MessagesRequest = serde_json::from_value(body).expect("parses");
        let decoded = decode(req, FrontendInfo {
            kind: FrontendKind::AnthropicMessages,
            requested_model: FrontendModel::from("claude-opus-4.7"),
        }, vec![]).expect("decodes");
        assert!(decoded.generation.reasoning.is_none(),
            "adaptive without output_config means no effort signal");
    }
```

- [ ] **Step 2: Run, verify build/test failure**

Run: `cargo nextest run -p agent-shim-frontends output_config_effort_decodes_to_canonical`
Expected: build error — `MessagesRequest` has no field `output_config`.

- [ ] **Step 3: Add wire struct for output_config**

Modify `crates/frontends/src/anthropic_messages/wire.rs:7-32`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<InboundMessage>,
    #[serde(default)]
    pub system: Option<SystemField>,
    #[serde(default)]
    pub tools: Option<Vec<InboundTool>>,
    #[serde(default)]
    pub tool_choice: Option<InboundToolChoice>,
    pub max_tokens: u32,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub metadata: Option<Value>,
    /// Anthropic extended-thinking config: `{ "type": "enabled" | "adaptive" | "disabled" }`.
    /// We tolerate all three modes; we no longer extract budget_tokens for the
    /// canonical effort axis (effort comes from `output_config.effort` post the
    /// `effort-2025-11-24` beta).
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    /// Anthropic `effort-2025-11-24` beta: `{ "effort": "low|medium|high|xhigh|max" }`.
    #[serde(default)]
    pub output_config: Option<OutputConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OutputConfig {
    #[serde(default)]
    pub effort: Option<String>,
}
```

- [ ] **Step 4: Update decode to read output_config.effort**

Modify `crates/frontends/src/anthropic_messages/decode.rs` around lines 105-132. Replace the `let reasoning = req.thinking.as_ref().and_then(...)` block with:

```rust
    // -- generation options --
    // Effort signal lives in output_config.effort (Anthropic `effort-2025-11-24`).
    // Legacy `thinking.budget_tokens` is intentionally NOT consumed for the
    // canonical effort axis — see 2026-05-28 spec.
    let reasoning = req
        .output_config
        .as_ref()
        .and_then(|oc| oc.effort.as_deref())
        .and_then(ReasoningEffort::parse)
        .map(|effort| ReasoningOptions {
            effort: Some(effort),
            budget_tokens: None,
        });
```

Ensure the imports near top include `ReasoningEffort` (already there).

- [ ] **Step 5: Run, verify tests pass**

Run: `cargo nextest run -p agent-shim-frontends`
Expected: all decode tests pass. Existing tests that exercised the
budget-derived effort path (search the file for `budget_tokens` in test
context, e.g. `thinking_with_budget_decodes_to_effort` or similar) will
now have their assertion `assert!(reasoning.is_some())` fail —
because the spec retires that decode path. Update each such test to
assert `assert!(decoded.generation.reasoning.is_none())` and rename it
to reflect the new contract (e.g.
`legacy_thinking_budget_no_longer_populates_canonical` from Step 1
already covers the new contract; delete the old positive variant).

If a downstream provider test (e.g. in Anthropic provider tests) was
constructing a `CanonicalRequest` with `generation.reasoning.budget_tokens`
set as input, leave it alone — that path is still used by callers that
explicitly want a budget, and only the *decode* of inbound Anthropic
HTTP requests changed.

- [ ] **Step 6: Commit**

```bash
git add crates/frontends/src/anthropic_messages/wire.rs crates/frontends/src/anthropic_messages/decode.rs
git commit -m "feat(frontends/anthropic): decode output_config.effort, drop budget_tokens decode"
```

---

## Task 5: OpenAI Chat / Responses decode accepts `"max"`

**Files:**
- No change required (Task 1 already extended `ReasoningEffort::parse`).
- Test: `crates/frontends/src/openai_chat/decode.rs::tests` and `openai_responses/decode.rs::tests`

- [ ] **Step 1: Write failing test for OpenAI Chat**

Append to `crates/frontends/src/openai_chat/decode.rs::tests`:

```rust
    #[test]
    fn reasoning_effort_max_decodes() {
        let body = serde_json::json!({
            "model": "gpt-5.5",
            "messages": [{ "role": "user", "content": "hi" }],
            "reasoning_effort": "max"
        });
        let req: ChatCompletionsRequest = serde_json::from_value(body).expect("parses");
        let decoded = decode(req, FrontendInfo {
            kind: FrontendKind::OpenAiChat,
            requested_model: FrontendModel::from("gpt-5.5"),
        }, vec![]).expect("decodes");
        let r = decoded.generation.reasoning.expect("reasoning set");
        assert_eq!(r.effort, Some(ReasoningEffort::Max));
    }
```

Same for `crates/frontends/src/openai_responses/decode.rs::tests`:

```rust
    #[test]
    fn responses_reasoning_effort_max_decodes() {
        let body = serde_json::json!({
            "model": "gpt-5.5",
            "input": "hi",
            "reasoning": { "effort": "max" }
        });
        // ... (decode through whatever helper the existing tests use)
        // Assert decoded.generation.reasoning.unwrap().effort == Some(Max).
    }
```

(Use the exact decode entry-point the existing OpenAI Responses tests call — search `openai_responses/decode.rs` for a `fn decode` test sibling and mirror its setup.)

- [ ] **Step 2: Run, verify pass (no impl changes needed)**

Run: `cargo nextest run -p agent-shim-frontends reasoning_effort_max`
Expected: PASS (Task 1 made `parse("max")` work).

- [ ] **Step 3: Commit**

```bash
git add crates/frontends/src/openai_chat/decode.rs crates/frontends/src/openai_responses/decode.rs
git commit -m "test(frontends): assert openai decoders accept reasoning_effort: max"
```

---

## Task 6: Add `accepts_xhigh` capability flag

**Files:**
- Modify: `crates/providers/src/lib.rs` (find `pub struct ProviderCapabilities`)
- Modify: `crates/providers/src/github_copilot/mod.rs` (initialise `accepts_xhigh: true`)
- Modify: other providers' capabilities to default `accepts_xhigh: false`

- [ ] **Step 1: Find existing ProviderCapabilities**

Run: `rg "struct ProviderCapabilities" crates/providers/`
Read the file and note the existing fields.

- [ ] **Step 2: Add field with `Default::default()` = `false`**

Modify the struct (path will be `crates/providers/src/lib.rs` or `crates/providers/src/capabilities.rs` — confirm via step 1):

```rust
pub struct ProviderCapabilities {
    // ... existing fields ...
    /// Provider's OpenAI-Chat-shape upstream accepts `reasoning_effort: "xhigh"`
    /// as a non-OpenAI extension. Today: Copilot only.
    pub accepts_xhigh: bool,
}
```

- [ ] **Step 3: Set `true` for github_copilot**

In `crates/providers/src/github_copilot/mod.rs`, find the `capabilities()` impl returning a `ProviderCapabilities`. Add `accepts_xhigh: true` to its constructor.

- [ ] **Step 4: All other providers default `accepts_xhigh: false`**

Compile-fail driven: `cargo check --workspace`. For each error pointing at a `ProviderCapabilities { .. }` literal, add `accepts_xhigh: false`. Skip providers that derive `Default` and use `..Default::default()` — they're automatic.

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/
git commit -m "feat(providers): add accepts_xhigh capability, Copilot sets true"
```

---

## Task 7: OpenAI Chat encoder compresses `Xhigh`/`Max`

**Files:**
- Modify: `crates/providers/src/oai_chat_wire/canonical_to_chat.rs` (function at lines ~200-216 currently writes `reasoning_effort`)
- Test: same file's `mod tests`

- [ ] **Step 1: Write failing tests**

Append to `crates/providers/src/oai_chat_wire/canonical_to_chat.rs::tests`:

```rust
    #[test]
    fn xhigh_serialises_as_xhigh_when_target_accepts() {
        let mut req = empty_request();
        req.resolved_policy.reasoning_effort = Some(ReasoningEffort::Xhigh);
        let body = build(&req, &target_with(true));
        assert_eq!(body.reasoning_effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn xhigh_compresses_to_high_when_target_rejects() {
        let mut req = empty_request();
        req.resolved_policy.reasoning_effort = Some(ReasoningEffort::Xhigh);
        let body = build(&req, &target_with(false));
        assert_eq!(body.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn max_compresses_to_high_on_pure_openai() {
        let mut req = empty_request();
        req.resolved_policy.reasoning_effort = Some(ReasoningEffort::Max);
        let body = build(&req, &target_with(false));
        assert_eq!(body.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn max_serialises_as_xhigh_when_copilot() {
        // Canonical Max → Copilot xhigh: Copilot's top tier on the chat path.
        let mut req = empty_request();
        req.resolved_policy.reasoning_effort = Some(ReasoningEffort::Max);
        let body = build(&req, &target_with(true));
        assert_eq!(body.reasoning_effort.as_deref(), Some("xhigh"));
    }
```

Make sure `target_with(accepts_xhigh: bool)` test helper exists; if not, add:

```rust
    fn target_with(accepts_xhigh: bool) -> BackendTarget {
        BackendTarget {
            provider: "test".into(),
            model: "test-model".into(),
            policy: Default::default(),
            capabilities: ProviderCapabilities {
                accepts_xhigh,
                ..Default::default()
            },
        }
    }
```

NOTE: if `BackendTarget` doesn't carry capabilities today, the `accepts_xhigh` must be passed via the existing target/route mechanism. Inspect `canonical_to_chat::build` signature; if it takes `&CanonicalRequest` only, thread capabilities via `req.resolved_policy` — or pass an extra `accepts_xhigh: bool` param at the call sites. Decide based on existing call shape and document in commit message.

- [ ] **Step 2: Run, verify failure**

Run: `cargo nextest run -p agent-shim-providers max_compresses_to_high_on_pure_openai`
Expected: FAIL — current code outputs `"max"` or unknown.

- [ ] **Step 3: Implement compression helper + thread the flag**

Replace the current `reasoning_effort: req.resolved_policy.reasoning_effort.map(|e| e.as_str().to_string())` line (around 211-214) with:

```rust
reasoning_effort: req
    .resolved_policy
    .reasoning_effort
    .map(|e| effort_for_chat(e, accepts_xhigh).to_string()),
```

Add the helper to the same file (above `build`):

```rust
fn effort_for_chat(e: ReasoningEffort, accepts_xhigh: bool) -> &'static str {
    use ReasoningEffort::*;
    match e {
        Minimal => "minimal",
        Low => "low",
        Medium => "medium",
        High => "high",
        Xhigh if accepts_xhigh => "xhigh",
        Xhigh => "high",
        Max if accepts_xhigh => "xhigh",
        Max => "high",
    }
}
```

If `build` doesn't accept a `BackendTarget` today, change its signature to do so (or pass `accepts_xhigh: bool` directly). Update all call sites.

- [ ] **Step 4: Run tests, verify pass**

Run: `cargo nextest run -p agent-shim-providers oai_chat_wire`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/oai_chat_wire/
git commit -m "feat(providers/oai-chat): compress Xhigh/Max for non-Copilot targets"
```

---

## Task 8: Responses API encoder compresses `Xhigh`/`Max`

**Files:**
- Modify: `crates/providers/src/openai_compatible/responses_api/encode_request.rs:147-152`
- Test: in-file or sibling test file

- [ ] **Step 1: Write failing test**

Append to whatever test module exists in `crates/providers/src/openai_compatible/responses_api/encode_request.rs::tests`:

```rust
    #[test]
    fn responses_xhigh_compresses_to_high() {
        let mut req = empty_request();
        req.resolved_policy.reasoning_effort = Some(ReasoningEffort::Xhigh);
        let body = build(&req, &target());
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn responses_max_compresses_to_high() {
        let mut req = empty_request();
        req.resolved_policy.reasoning_effort = Some(ReasoningEffort::Max);
        let body = build(&req, &target());
        assert_eq!(body["reasoning"]["effort"], "high");
    }
```

(`empty_request` and `target` should exist in this file already — verify and reuse.)

- [ ] **Step 2: Run, verify failure**

Run: `cargo nextest run -p agent-shim-providers responses_xhigh_compresses_to_high`
Expected: FAIL — outputs `"xhigh"`.

- [ ] **Step 3: Implement compression**

Modify lines 149-152:

```rust
    // Reasoning effort (Responses API uses `reasoning: { effort: "..." }`).
    if let Some(effort) = req.resolved_policy.reasoning_effort {
        let effort_str = match effort {
            ReasoningEffort::Xhigh | ReasoningEffort::Max => "high",
            other => other.as_str(),
        };
        body["reasoning"] = json!({ "effort": effort_str });
    }
```

- [ ] **Step 4: Run, verify pass**

Run: `cargo nextest run -p agent-shim-providers responses_`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/openai_compatible/responses_api/
git commit -m "feat(providers/responses-api): compress Xhigh/Max to high"
```

---

## Task 9: Anthropic provider emits adaptive+output_config when effort beta present

**Files:**
- Modify: `crates/providers/src/anthropic/wire.rs` (add `OutgoingOutputConfig`)
- Modify: `crates/providers/src/anthropic/request.rs:55-83, 290-311`
- Test: new `crates/providers/tests/anthropic_effort_beta.rs`

- [ ] **Step 1: Add `OutgoingOutputConfig` to wire**

Modify `crates/providers/src/anthropic/wire.rs:57-62` (around `OutgoingThinking`):

```rust
#[derive(Debug, Clone, Serialize)]
pub(crate) struct OutgoingThinking {
    #[serde(rename = "type")]
    pub(crate) ty: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) budget_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct OutgoingOutputConfig {
    pub(crate) effort: &'static str,
}
```

Note `budget_tokens` is now `Option<u32>` so we can omit it on the adaptive path.

Add the field to `OutgoingRequest` after `thinking`:

```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) output_config: Option<OutgoingOutputConfig>,
```

- [ ] **Step 2: Write failing integration test**

Create `crates/providers/tests/anthropic_effort_beta.rs`:

```rust
//! Verify Anthropic-as-provider emits the new `effort-2025-11-24` shape
//! when the route policy has the beta header.

use agent_shim_core::{
    policy::ResolvedPolicy,
    request::{CanonicalRequest, GenerationOptions, ReasoningEffort, ReasoningOptions, RequestMetadata},
    target::{BackendTarget, FrontendInfo, FrontendModel},
    ExtensionMap, FrontendKind, RequestId,
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
        messages: vec![],
        tools: vec![],
        tool_choice: Default::default(),
        generation: GenerationOptions {
            reasoning: Some(ReasoningOptions {
                effort: Some(effort),
                budget_tokens: None,
            }),
            ..Default::default()
        },
        response_format: None,
        stream: false,
        metadata: RequestMetadata::default(),
        inbound_anthropic_headers: headers.clone(),
        resolved_policy: ResolvedPolicy {
            reasoning_effort: Some(effort),
            reasoning_budget_tokens: None,
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
    assert!(body["thinking"].get("budget_tokens").is_none(),
        "adaptive path must omit budget_tokens");
    assert_eq!(body["output_config"]["effort"], "max");
}

#[test]
fn legacy_path_when_effort_beta_absent() {
    let req = req_with_effort_and_beta(ReasoningEffort::Xhigh, false);
    let body = agent_shim_providers::anthropic::request::build(&req, &target());
    assert_eq!(body["thinking"]["type"], "enabled");
    assert!(body["thinking"]["budget_tokens"].as_u64().is_some());
    assert!(body.get("output_config").is_none(),
        "legacy path must NOT emit output_config");
}

#[test]
fn minimal_maps_to_low_on_adaptive_path() {
    let req = req_with_effort_and_beta(ReasoningEffort::Minimal, true);
    let body = agent_shim_providers::anthropic::request::build(&req, &target());
    assert_eq!(body["output_config"]["effort"], "low",
        "Anthropic has no `minimal` — Minimal compresses to low");
}
```

NOTE: `agent_shim_providers::anthropic::request::build` must be re-exportable. If currently `pub(crate)`, make it `pub` for tests.

- [ ] **Step 3: Run, verify build failure**

Run: `cargo nextest run -p agent-shim-providers --test anthropic_effort_beta`
Expected: build error or test failure — `output_config` missing in the produced body, or `build` not accessible.

- [ ] **Step 4: Implement the branching logic**

In `crates/providers/src/anthropic/request.rs`, replace the existing `build_thinking` and its call site:

```rust
// In `build`, replace the `let thinking = build_thinking(req);` line with:
let (thinking, output_config) = build_reasoning_blocks(req);

// And add to OutgoingRequest construction (around line 81):
    thinking,
    output_config,
```

Then replace `build_thinking` with:

```rust
// ── reasoning blocks ────────────────────────────────────────────────────────

fn build_reasoning_blocks(
    req: &CanonicalRequest,
) -> (Option<OutgoingThinking>, Option<OutgoingOutputConfig>) {
    let effort = req.resolved_policy.reasoning_effort;
    if effort.is_none() && req.resolved_policy.reasoning_budget_tokens.is_none() {
        return (None, None);
    }

    let use_effort_beta = req
        .resolved_policy
        .anthropic_headers
        .iter()
        .any(|(k, v)| {
            k.eq_ignore_ascii_case("anthropic-beta")
                && v.split(',').any(|seg| seg.trim() == "effort-2025-11-24")
        });

    if use_effort_beta {
        // New shape: adaptive thinking + explicit output_config.effort.
        let e = effort.unwrap_or(ReasoningEffort::High);
        (
            Some(OutgoingThinking {
                ty: "adaptive",
                budget_tokens: None,
            }),
            Some(OutgoingOutputConfig {
                effort: anthropic_effort_str(e),
            }),
        )
    } else {
        // Legacy shape: thinking.budget_tokens.
        let budget = req
            .resolved_policy
            .reasoning_budget_tokens
            .or_else(|| effort.map(effort_to_budget));
        budget.map_or((None, None), |b| {
            (
                Some(OutgoingThinking {
                    ty: "enabled",
                    budget_tokens: Some(b),
                }),
                None,
            )
        })
    }
}

fn anthropic_effort_str(e: ReasoningEffort) -> &'static str {
    match e {
        ReasoningEffort::Minimal => "low", // Anthropic has no minimal
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh => "xhigh",
        ReasoningEffort::Max => "max",
    }
}

fn effort_to_budget(effort: ReasoningEffort) -> u32 {
    match effort {
        ReasoningEffort::Minimal => 128,
        ReasoningEffort::Low => 512,
        ReasoningEffort::Medium => 2048,
        ReasoningEffort::High => 8192,
        ReasoningEffort::Xhigh => 16384,
        ReasoningEffort::Max => 16384,
    }
}
```

Make sure `OutgoingOutputConfig` is imported in `request.rs`:

```rust
use super::wire::{
    OutgoingContentBlock, OutgoingMessage, OutgoingOutputConfig, OutgoingRequest,
    OutgoingSystem, OutgoingThinking, OutgoingTool, OutgoingToolChoice, OutgoingToolResultContent,
};
```

Make `build` public if it isn't:

```rust
pub fn build(req: &CanonicalRequest, target: &BackendTarget) -> Value {
```

- [ ] **Step 5: Run tests**

Run: `cargo nextest run -p agent-shim-providers --test anthropic_effort_beta`
Expected: PASS all three.

Also re-run existing anthropic provider tests:

Run: `cargo nextest run -p agent-shim-providers anthropic::request`
Expected: existing tests pass. If old `thinking_with_explicit_budget_is_emitted` or similar tests fail, update their expectations:
- They now need to confirm legacy mode requires absent `effort-2025-11-24` beta. Add the beta-absent setup if missing.

- [ ] **Step 6: Commit**

```bash
git add crates/providers/src/anthropic/
git commit -m "feat(providers/anthropic): emit adaptive+output_config when effort beta present"
```

---

## Task 10: Gemini provider supports `Max` effort

**Files:**
- Modify: `crates/providers/src/gemini/request.rs:463-475`

- [ ] **Step 1: Write failing test**

Append to `crates/providers/src/gemini/request.rs::tests`:

```rust
    #[test]
    fn effort_to_budget_includes_max() {
        assert_eq!(effort_to_budget(ReasoningEffort::Max), 24576);
    }
```

Also update `effort_to_budget_table_matches_spec` if it exists by adding the Max line.

- [ ] **Step 2: Run, verify failure**

Run: `cargo nextest run -p agent-shim-providers effort_to_budget_includes_max`
Expected: FAIL — non-exhaustive match or wrong value.

- [ ] **Step 3: Add arm**

Modify `crates/providers/src/gemini/request.rs:463-475`:

```rust
fn effort_to_budget(effort: ReasoningEffort) -> i64 {
    match effort {
        ReasoningEffort::Minimal => 128,
        ReasoningEffort::Low => 256,
        ReasoningEffort::Medium => 1024,
        ReasoningEffort::High => 4096,
        ReasoningEffort::Xhigh => 16384,
        ReasoningEffort::Max => 24576,
    }
}
```

Also update the docstring table near the top of `request.rs` to include the Max row.

- [ ] **Step 4: Run, verify pass**

Run: `cargo nextest run -p agent-shim-providers gemini`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/gemini/
git commit -m "feat(providers/gemini): map ReasoningEffort::Max to 24576 thinking budget"
```

---

## Task 11: Switch Gemini to read from `resolved_policy.reasoning_budget_tokens`

**Files:**
- Modify: `crates/providers/src/gemini/request.rs:429-460` (`thinking_config` function)

- [ ] **Step 1: Write failing test**

Append to `crates/providers/src/gemini/request.rs::tests`:

```rust
    #[test]
    fn gemini_reads_budget_from_resolved_policy() {
        let mut req = empty_request();
        req.resolved_policy.reasoning_budget_tokens = Some(7777);
        // resolved_policy.reasoning_effort intentionally None to verify
        // the budget path takes precedence.
        let body = build(&req, &target());
        let tc = body.generation_config.unwrap().thinking_config.unwrap();
        assert_eq!(tc.thinking_budget, Some(7777));
    }
```

- [ ] **Step 2: Run, verify failure**

Expected: FAIL — Gemini currently reads from `req.generation.reasoning.budget_tokens`, not `resolved_policy`.

- [ ] **Step 3: Update `thinking_config`**

Replace `crates/providers/src/gemini/request.rs:429-460` with:

```rust
fn thinking_config(req: &CanonicalRequest) -> Option<ThinkingConfig> {
    // Source 1 — explicit budget on the resolved policy (which folds
    //              inbound budget + mapping `set.budget_tokens`).
    if let Some(b) = req.resolved_policy.reasoning_budget_tokens {
        return Some(ThinkingConfig {
            thinking_budget: Some(b as i64),
            include_thoughts: Some(true),
        });
    }
    // Source 2 — effort string from the resolved policy.
    if let Some(effort) = req.resolved_policy.reasoning_effort {
        return Some(ThinkingConfig {
            thinking_budget: Some(effort_to_budget(effort)),
            include_thoughts: Some(true),
        });
    }
    None
}
```

Remove the old "source 3" branch (`req.generation.reasoning.effort`) — the canonical pipeline now always populates `resolved_policy`.

- [ ] **Step 4: Run all Gemini tests**

Run: `cargo nextest run -p agent-shim-providers gemini`
Expected: PASS. Some existing tests (`explicit_budget_tokens_wins_over_resolved_policy`, `request_effort_used_when_no_resolved_policy_and_no_explicit_budget`) likely need updating because they set `req.generation.reasoning.budget_tokens` rather than `req.resolved_policy.reasoning_budget_tokens`. Update them so they set the resolved-policy field instead — that's the contract now.

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/gemini/
git commit -m "refactor(providers/gemini): read reasoning from resolved_policy only"
```

---

## Task 12: Anthropic provider also reads budget from resolved policy

**Files:**
- Already done in Task 9 via `req.resolved_policy.reasoning_budget_tokens` in the legacy branch. Verify:

- [ ] **Step 1: Run all anthropic provider tests**

Run: `cargo nextest run -p agent-shim-providers anthropic`
Expected: PASS.

- [ ] **Step 2: Find any remaining direct reads of `req.generation.reasoning`**

Run: `rg "generation\.reasoning" crates/providers/src/anthropic/`
Expected: no matches (or matches only in non-effort context). If any remain that pertain to effort/budget, refactor them to read from `resolved_policy`.

- [ ] **Step 3: Commit any cleanup**

```bash
git add crates/providers/src/anthropic/
git commit -m "refactor(providers/anthropic): finalize resolved_policy-only reads" --allow-empty
```

---

## Task 13: Config schema for `reasoning_mapping`

**Files:**
- Modify: `crates/config/src/schema.rs:464-518` (`RouteEntry`)
- Test: `crates/config/src/schema.rs::tests`

- [ ] **Step 1: Write failing YAML parse test**

Append to `crates/config/src/schema.rs::tests`:

```rust
    #[test]
    fn route_entry_parses_reasoning_mapping() {
        let yaml = r#"
            frontend: anthropic_messages
            model: claude-opus-4-7
            upstream: copilot
            upstream_model: claude-opus-4-7
            reasoning_mapping:
              - match: max
                set: xhigh
              - match: high
                set: xhigh
        "#;
        let entry: RouteEntry = serde_yaml::from_str(yaml).expect("parses");
        assert_eq!(entry.reasoning_mapping.len(), 2);
        assert_eq!(entry.reasoning_mapping[0].r#match, "max");
        assert_eq!(entry.reasoning_mapping[0].set, "xhigh");
    }

    #[test]
    fn reasoning_mapping_rejects_unknown_set_field() {
        let yaml = r#"
            frontend: anthropic_messages
            model: x
            upstream: u
            upstream_model: um
            reasoning_mapping:
              - match: high
                set: high
                unknown: oops
        "#;
        let result: Result<RouteEntry, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "deny_unknown_fields must reject");
    }
```

- [ ] **Step 2: Run, verify failure**

Run: `cargo nextest run -p agent-shim-config route_entry_parses_reasoning_mapping`
Expected: FAIL — field unknown.

- [ ] **Step 3: Add fields**

Modify `crates/config/src/schema.rs:464-518` to add `reasoning_mapping` to `RouteEntry`:

```rust
pub struct RouteEntry {
    // ... existing fields ...
    /// Per-route canonical effort rewrite table. Each entry maps an inbound
    /// canonical effort to an outbound canonical effort. Both fields are
    /// strings from {minimal, low, medium, high, xhigh, max}.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_mapping: Vec<MappingRuleConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingRuleConfig {
    pub r#match: String,
    pub set: String,
}
```

Also update the convenience `RouteEntry::singular` constructor (around line 520) to set `reasoning_mapping: Vec::new()`.

- [ ] **Step 4: Run, verify pass**

Run: `cargo nextest run -p agent-shim-config`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/config/src/schema.rs
git commit -m "feat(config): add reasoning_mapping to RouteEntry"
```

---

## Task 14: Validate effort strings at config-load

**Files:**
- Modify: `crates/config/src/validation.rs` (find existing validation entrypoint via `rg "fn validate" crates/config/src/`)
- Test: `crates/config/src/validation.rs::tests` or sibling

- [ ] **Step 1: Find the existing routes validator**

Run: `rg "fn validate" crates/config/src/`
Read the file. Look for `validate_routes` or similar.

- [ ] **Step 2: Write failing test**

Append to whichever test module owns `validate_routes`:

```rust
    #[test]
    fn invalid_effort_in_mapping_is_rejected() {
        let cfg = GatewayConfig {
            routes: vec![RouteEntry {
                frontend: "anthropic_messages".into(),
                model: "x".into(),
                upstream: Some("u".into()),
                upstream_model: Some("um".into()),
                upstreams: vec![],
                reasoning_effort: None,
                anthropic_beta: None,
                retry: Default::default(),
                breaker: Default::default(),
                min_tier: None,
                max_cost_usd: None,
                plugins: None,
                reasoning_mapping: vec![MappingRuleConfig {
                    r#match: "high".into(),
                    set: "super-mega".into(),
                }],
            }],
            // ... fill in other config fields with defaults
            ..Default::default() // adjust per actual GatewayConfig shape
        };
        let result = validate_routes(&cfg);
        assert!(result.is_err(), "unknown effort 'super-mega' must be rejected");
    }
```

- [ ] **Step 3: Run, verify failure**

Run: `cargo nextest run -p agent-shim-config invalid_effort_in_mapping_is_rejected`
Expected: FAIL — validator currently does not check mapping efforts.

- [ ] **Step 4: Implement validation**

In the function that loops over routes (`validate_routes`), after the existing `reasoning_effort` check, add:

```rust
for (idx, rule) in entry.reasoning_mapping.iter().enumerate() {
    use agent_shim_core::request::ReasoningEffort;
    if ReasoningEffort::parse(&rule.r#match).is_none() {
        return Err(format!(
            "route {}/{}: reasoning_mapping[{}].match has unknown effort '{}' (expected minimal/low/medium/high/xhigh/max)",
            entry.frontend, entry.model, idx, rule.r#match
        ).into());
    }
    if ReasoningEffort::parse(&rule.set).is_none() {
        return Err(format!(
            "route {}/{}: reasoning_mapping[{}].set has unknown effort '{}' (expected minimal/low/medium/high/xhigh/max)",
            entry.frontend, entry.model, idx, rule.set
        ).into());
    }
}
```

(Match the existing error type — if `validate_routes` returns `Result<(), ValidationError>` use the same variant pattern.)

- [ ] **Step 5: Run, verify pass**

Run: `cargo nextest run -p agent-shim-config`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/config/src/
git commit -m "feat(config): reject unknown effort strings in reasoning_mapping at load"
```

---

## Task 15: Static router wires mapping config → policy

**Files:**
- Modify: `crates/router/src/static_routes.rs:61-74`
- Test: `crates/router/src/static_routes.rs::tests`

- [ ] **Step 1: Write failing test**

Append to `crates/router/src/static_routes.rs::tests`:

```rust
    #[test]
    fn mapping_config_propagates_to_policy() {
        use agent_shim_config::{GatewayConfig, MappingRuleConfig, RouteEntry, RetryConfig, BreakerConfig};
        let cfg = GatewayConfig {
            routes: vec![RouteEntry {
                frontend: "anthropic_messages".into(),
                model: "claude-opus".into(),
                upstream: Some("copilot".into()),
                upstream_model: Some("claude-opus-4.7".into()),
                upstreams: vec![],
                reasoning_effort: None,
                anthropic_beta: None,
                retry: RetryConfig::default(),
                breaker: BreakerConfig::default(),
                min_tier: None,
                max_cost_usd: None,
                plugins: None,
                reasoning_mapping: vec![
                    MappingRuleConfig { r#match: "max".into(), set: "xhigh".into() },
                ],
            }],
            ..Default::default()
        };
        let router = StaticRouter::from_config(&cfg);
        let target = &router
            .resolve(FrontendKind::AnthropicMessages, "claude-opus")
            .unwrap()[0];
        assert_eq!(target.policy.reasoning_mapping.len(), 1);
        assert_eq!(
            target.policy.reasoning_mapping[0].r#match,
            ReasoningEffort::Max
        );
        assert_eq!(
            target.policy.reasoning_mapping[0].set,
            ReasoningEffort::Xhigh
        );
    }
```

- [ ] **Step 2: Run, verify failure**

Run: `cargo nextest run -p agent-shim-router mapping_config_propagates_to_policy`
Expected: FAIL — `reasoning_mapping` is empty on policy.

- [ ] **Step 3: Implement translation in `from_config`**

In `crates/router/src/static_routes.rs:61-74`, after building `default_reasoning_effort`, add:

```rust
let reasoning_mapping = entry
    .reasoning_mapping
    .iter()
    .filter_map(|rule| {
        let m = ReasoningEffort::parse(&rule.r#match)?;
        let s = ReasoningEffort::parse(&rule.set)?;
        Some(agent_shim_core::policy::MappingRule {
            r#match: m,
            set: s,
        })
    })
    .collect::<Vec<_>>();

let policy = RoutePolicy {
    default_reasoning_effort,
    default_anthropic_beta: entry.anthropic_beta.clone(),
    reasoning_mapping,
};
```

The `filter_map` is safe because Task 14's validator already errored on unknown strings before this point. The skip is a belt-and-braces for hand-built `GatewayConfig` test fixtures.

- [ ] **Step 4: Run, verify pass**

Run: `cargo nextest run -p agent-shim-router`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/router/src/static_routes.rs
git commit -m "feat(router): translate reasoning_mapping config to policy"
```

---

## Task 16: End-to-end integration test

**Files:**
- Create: `crates/protocol-tests/tests/effort_mapping_end_to_end.rs`

- [ ] **Step 1: Inspect protocol-tests fixtures**

Run: `ls crates/protocol-tests/tests/` and read `model_index_integration.rs` to understand the harness conventions used in this crate.

- [ ] **Step 2: Write the e2e test**

Create `crates/protocol-tests/tests/effort_mapping_end_to_end.rs`:

```rust
//! End-to-end: Anthropic frontend sending `output_config.effort: max`,
//! routed through a Copilot-backed route with `reasoning_mapping: [max → xhigh]`,
//! and asserting the outbound Copilot/OpenAI-Chat-shape body carries
//! `reasoning_effort: "xhigh"` (because Copilot accepts_xhigh).

use agent_shim_config::{
    BreakerConfig, GatewayConfig, MappingRuleConfig, RetryConfig, RouteEntry, UpstreamConfig,
};
use agent_shim_core::FrontendKind;
use agent_shim_router::StaticRouter;
use std::collections::BTreeMap;

fn cfg() -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert(
        "copilot".to_string(),
        UpstreamConfig::github_copilot_test_stub(),
    );
    GatewayConfig {
        upstreams,
        routes: vec![RouteEntry {
            frontend: "anthropic_messages".into(),
            model: "claude-opus-4-7".into(),
            upstream: Some("copilot".into()),
            upstream_model: Some("claude-opus-4.7".into()),
            upstreams: vec![],
            reasoning_effort: None,
            anthropic_beta: None,
            retry: RetryConfig::default(),
            breaker: BreakerConfig::default(),
            min_tier: None,
            max_cost_usd: None,
            plugins: None,
            reasoning_mapping: vec![MappingRuleConfig {
                r#match: "max".into(),
                set: "xhigh".into(),
            }],
        }],
        ..Default::default()
    }
}

#[test]
fn claude_code_max_effort_becomes_copilot_xhigh() {
    let inbound = serde_json::json!({
        "model": "claude-opus-4-7",
        "max_tokens": 1024,
        "messages": [{ "role": "user", "content": "hi" }],
        "thinking": { "type": "adaptive" },
        "output_config": { "effort": "max" }
    });

    // Decode through anthropic_messages frontend.
    let wire_req: agent_shim_frontends::anthropic_messages::wire::MessagesRequest =
        serde_json::from_value(inbound).unwrap();
    let canonical_req = agent_shim_frontends::anthropic_messages::decode::decode(
        wire_req,
        agent_shim_core::target::FrontendInfo {
            kind: FrontendKind::AnthropicMessages,
            requested_model: "claude-opus-4-7".into(),
        },
        vec![],
    )
    .unwrap();

    // Resolve via static router (gives BackendTarget with policy).
    let router = StaticRouter::from_config(&cfg());
    let mut targets = router
        .resolve(FrontendKind::AnthropicMessages, "claude-opus-4-7")
        .unwrap();
    let target = targets.remove(0);

    // Apply route policy → resolved_policy.
    let mut req = canonical_req;
    req.resolved_policy = target.policy.resolve(&req);
    assert_eq!(
        req.resolved_policy.reasoning_effort,
        Some(agent_shim_core::request::ReasoningEffort::Xhigh),
        "mapping should rewrite Max→Xhigh"
    );

    // Encode through the OpenAI-Chat-shape (since Copilot upstream is OpenAI-Chat
    // shape on the chat path). For this assertion-only test, dive into the
    // canonical_to_chat helper directly.
    let chat_body =
        agent_shim_providers::oai_chat_wire::canonical_to_chat::build(&req, &target);
    // accepts_xhigh defaults to true for Copilot (Task 6).
    assert_eq!(chat_body.reasoning_effort.as_deref(), Some("xhigh"));
}
```

The `UpstreamConfig::github_copilot_test_stub()` helper may not exist — replace with whatever minimal stub the `model_index_integration.rs` test uses, or just construct `UpstreamConfig { ... }` inline with the bare fields the type requires. Inspect the existing struct.

If `build` for `canonical_to_chat` isn't pub, make it pub in Task 7. If the integration goes through `/v1/messages` rather than `/v1/chat/completions`, route through the Anthropic provider's `build()` (which is pub after Task 9) and assert on `output_config.effort: "xhigh"` in that body instead. Pick whichever shape better matches the route policy → Copilot path in production.

- [ ] **Step 3: Run, verify pass**

Run: `cargo nextest run -p agent-shim-protocol-tests claude_code_max_effort_becomes_copilot_xhigh`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/protocol-tests/tests/effort_mapping_end_to_end.rs
git commit -m "test(protocol-tests): e2e Claude Code max → Copilot xhigh via mapping"
```

---

## Task 17: Update documentation

**Files:**
- Modify: `CONTEXT.md` (add 3 domain terms)
- Modify: `README.md` (`## Reasoning / thinking effort` section, lines ~272-295)
- Modify: `docs/configuration.md` (route table)
- Modify: `config/gateway.example.yaml` (add worked example)

- [ ] **Step 1: Update `CONTEXT.md`**

Add to the "Entities" section (after the existing `Reasoning effort` entry around lines 34-40):

```markdown
**Reasoning mapping table** *(`ReasoningMapping`)*
Optional per-route ordered list of `{ match, set }` rules over canonical
effort. First rule whose `match` equals the post-default inbound effort
fires its `set`; unmatched passes through. Both fields are
canonical-vocabulary effort strings (`minimal/low/medium/high/xhigh/max`),
NOT raw inbound or outbound dialect strings. Lives in
`agent-shim-core::policy::RoutePolicy`.

**Mapping rule** *(`MappingRule`)*
One entry in a reasoning mapping table.

**Effort vocabulary**
Six canonical levels: `Minimal | Low | Medium | High | Xhigh | Max`.
Cross-dialect mapping is per-direction:
- Anthropic inbound `output_config.effort` ∈ {low, medium, high, xhigh, max}
- OpenAI Chat inbound `reasoning_effort` ∈ {minimal, low, medium, high}
- Copilot Claude inbound: same as Anthropic
- Outbound Anthropic: `Minimal → "low"`, else identity
- Outbound OpenAI Chat (not Copilot): `Xhigh|Max → "high"`, else identity
- Outbound Copilot Chat: `Xhigh|Max → "xhigh"`, else identity
- Outbound Responses API: same as OpenAI Chat
```

Update the existing `Reasoning effort` entry to remove the budget_tokens cross-dialect bullet and replace with the new 6-value note.

- [ ] **Step 2: Update `README.md`**

Replace `## Reasoning / thinking effort` (around line 272-295) with:

```markdown
## Reasoning / thinking effort

AgentShim translates "thinking effort" between dialects so any agent can drive any reasoning-capable backend.

| Frontend dialect | Field accepted on inbound request |
|---|---|
| Anthropic `/v1/messages` | `output_config: { effort: "low" \| "medium" \| "high" \| "xhigh" \| "max" }` (with `anthropic-beta: effort-2025-11-24`) |
| OpenAI `/v1/chat/completions` | `reasoning_effort: "minimal" \| "low" \| "medium" \| "high"` |
| OpenAI `/v1/responses` | `reasoning: { effort: "..." }` |

Inbound effort is normalised to the 6-value canonical enum
(`minimal/low/medium/high/xhigh/max`) and forwarded to the upstream in its
native shape. Compression at provider boundaries:

- OpenAI Chat (non-Copilot) and Responses API: `xhigh`/`max` → `"high"`.
- Anthropic: `minimal` → `"low"` (Anthropic vocabulary has no minimal).

**Per-route default.** Set `reasoning_effort` on a route to apply a default when the agent doesn't send one:

```yaml
routes:
  - frontend: anthropic_messages
    model: claude-sonnet-4-5
    upstream: copilot
    upstream_model: claude-sonnet-4-5
    reasoning_effort: high
```

**Per-route mapping table.** Rewrite inbound efforts before they reach the upstream:

```yaml
routes:
  - frontend: anthropic_messages
    model: claude-opus-4-7
    upstream: copilot
    upstream_model: claude-opus-4-7
    reasoning_mapping:
      - match: max     # Claude Code "ultrathink"
        set:   xhigh   # → Copilot's top tier
      - match: high
        set:   xhigh   # bump high to xhigh on this route
```

Rules are evaluated top-to-bottom; first match wins; unmatched passes through. The default fills inbound holes before mapping runs, so `default + mapping` chain works as expected.
```

- [ ] **Step 3: Update `docs/configuration.md`**

Find the routes section (around line 130-140), add a `reasoning_mapping` row to whatever table or example list exists. Mirror the format used by `reasoning_effort` and `anthropic_beta` adjacent entries.

- [ ] **Step 4: Update `config/gateway.example.yaml`**

Append after the existing `reasoning_effort: medium` example (around line 70):

```yaml
  # Per-route effort mapping table — rewrite inbound efforts before forward.
  # First match wins; unmatched passes through.
  # - frontend: anthropic_messages
  #   model: claude-opus-4-7
  #   upstream: copilot
  #   upstream_model: claude-opus-4-7
  #   reasoning_mapping:
  #     - match: max     # Claude Code "ultrathink"
  #       set:   xhigh   # Copilot's top tier
  #     - match: high
  #       set:   xhigh   # also lift high
```

- [ ] **Step 5: Verify markdown still renders**

Run: `cargo doc --workspace --no-deps 2>&1 | grep -i warning | head -20`
Expected: no new warnings.

- [ ] **Step 6: Commit**

```bash
git add CONTEXT.md README.md docs/configuration.md config/gateway.example.yaml
git commit -m "docs: document reasoning_mapping and 6-value effort vocabulary"
```

---

## Task 18: Full workspace test sweep

- [ ] **Step 1: Format check**

Run: `cargo fmt --all -- --check`
Expected: no diff.

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 3: Full test suite**

Run: `cargo nextest run --workspace`
Expected: all tests pass.

- [ ] **Step 4: Run the existing effort-related tests we care about**

Run: `cargo nextest run --workspace -E 'test(/.*effort.*|.*mapping.*|.*thinking.*|.*reasoning.*/)'`
Expected: PASS.

- [ ] **Step 5: Final commit if anything was tweaked**

```bash
git add -u
git commit -m "chore: workspace lint and format sweep" --allow-empty
```
