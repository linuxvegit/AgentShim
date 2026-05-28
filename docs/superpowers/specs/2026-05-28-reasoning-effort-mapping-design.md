# Cross-Dialect Reasoning Effort Translation — Design Spec

**Date:** 2026-05-28
**Status:** Draft (pending implementation plan)
**Scope:** Per-route translation table for reasoning effort across the
modern effort-string dialects spoken by Anthropic, OpenAI, and Copilot.

## Motivation

As of `anthropic-beta: effort-2025-11-24`, Claude Code / Claude Desktop
emit reasoning intent as **a string effort enum**, the same shape OpenAI
and Copilot use — Anthropic no longer sends `thinking.budget_tokens` for
new clients on that beta. Captured payload:

```
POST /v1/messages?beta=true
anthropic-beta: claude-code-20250219,
                context-1m-2025-08-07,
                interleaved-thinking-2025-05-14,
                effort-2025-11-24

{
  "model": "claude-opus-4-7",
  "messages": [...],
  "max_tokens": 64000,
  "thinking": { "type": "adaptive" },
  "output_config": { "effort": "max" },
  "stream": true
}
```

This convergence means the cross-dialect problem reduces to **name
translation**:

| Source | Effort vocabulary |
|---|---|
| Anthropic (`output_config.effort`) | `low`, `medium`, `high`, `xhigh`, `max` |
| OpenAI Chat (`reasoning_effort`) | `minimal`, `low`, `medium`, `high` |
| OpenAI Responses (`reasoning.effort`) | `minimal`, `low`, `medium`, `high` |
| Copilot Claude | `minimal`, `low`, `medium`, `high`, `xhigh` |

A Claude Code agent set to "max effort" wants its intent honored when the
gateway routes it to Copilot Claude (`xhigh`) or to OpenAI (`high`). A
Copilot user choosing `xhigh` may want it lifted to `max` on Anthropic
native. Today AgentShim has no per-route knob for either rewrite.

This spec introduces a single mechanism — a per-route **reasoning
mapping table** that operates exclusively in the effort-string axis — and
brings the canonical model into alignment with the new vocabularies.

## Domain language

Three entries are added to `CONTEXT.md`:

- **Reasoning mapping table** (`ReasoningMapping`): An optional per-route
  ordered list of effort-string rewrite rules. The first rule whose
  `match` equals the inbound effort fires its `set` and stops the scan.
  Unmatched: passthrough. **Both `match` and `set` are values drawn from
  the canonical `ReasoningEffort` vocabulary, NOT raw inbound or outbound
  dialect strings.** The mapping runs after decode (inbound is already
  canonical) and before encode (provider re-translates to its dialect),
  so the mapping author works in canonical terms only.

- **Adaptive thinking**: Anthropic's new `thinking: { type: "adaptive" }`
  shape, used together with `output_config.effort`. Replaces the old
  `thinking: { type: "enabled", budget_tokens: N }` shape on clients with
  the `effort-2025-11-24` beta header.

- **Effort vocabulary**: The canonical 6-value union covering both
  dialects: `Minimal | Low | Medium | High | Xhigh | Max`. OpenAI is the
  source of `Minimal`; Anthropic is the source of `Max`; `Xhigh` is the
  Copilot extension to OpenAI Chat.

The existing terms **Reasoning effort**, **Route policy**, and **Resolved
policy** are unchanged but adopt the 6-value enum.

The previously-canonical concept **`thinking.budget_tokens` as
reasoning intent** is retired from canonical. Quantitative budget
control still exists for **Gemini** internally and for **legacy Anthropic
clients** on the decode path, but it no longer participates in the
mapping pipeline.

## Configuration surface (YAML)

```yaml
routes:
  - frontend: anthropic_messages
    model: claude-opus-4-7
    upstream: copilot
    upstream_model: claude-opus-4-7

    # Existing: route-level default when inbound supplies nothing.
    reasoning_effort: medium

    # New: rewrite table. First matching rule wins.
    reasoning_mapping:
      # Claude Code "max" → Copilot's xhigh (Copilot doesn't recognise max).
      - match: max
        set:   xhigh

      # OpenAI client "high" → lift to xhigh on this route.
      - match: high
        set:   xhigh

      # Wildcard last resort that pins minimums (commented; example only).
      # - match: minimal
      #   set:   low
```

Each entry is `{ match: <effort>, set: <effort> }`. Both fields are
required effort strings drawn from the 6-value **canonical** vocabulary
(`minimal`, `low`, `medium`, `high`, `xhigh`, `max`). They are NOT
inbound or outbound dialect strings — the frontend has already decoded
the inbound to canonical before mapping runs, and the provider will
re-encode canonical to its own dialect after mapping runs. A mapping
rule like `match: max → set: xhigh` therefore fires whenever the
canonical inbound effort is `Max`, regardless of which dialect the
agent originally spoke.

### Rule semantics

- **Order**: Rules are evaluated top-to-bottom. The first match fires;
  subsequent rules are skipped.
- **No match**: If no rule matches, the inbound (after Default fallback)
  passes through unchanged.
- **Wildcard**: There is no `*` wildcard. To force every inbound effort
  to a single value, enumerate each variant explicitly. The justification
  is YAGNI — six rules to cover all variants is short and self-documenting,
  and we avoid the configuration ambiguity that comes with implicit
  fall-through.
- **Unknown effort strings** in YAML (`set: superhigh`) are rejected at
  config-load time with a clear error.

### Interaction with `default_reasoning_effort`

```
inbound effort  ──┬─→ if Some: pass through to step [2]
                  └─→ if None: substitute route.reasoning_effort default

[2] Mapping table ──→ if rule matches: rewrite; else: passthrough

[3] ResolvedPolicy.reasoning_effort
        │
        ▼
provider encode
```

- `default_reasoning_effort` and `reasoning_mapping` are independent. The
  default fills in before mapping runs, so a route may pair them
  (`default: medium`, `mapping: [medium → high]` ⇒ "at least high").
- This sequence preserves the v0.x contract for routes without a mapping.

## Canonical model changes

### `crates/core/src/request.rs`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,   // NEW — Anthropic effort-2025-11-24 vocabulary
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low     => "low",
            ReasoningEffort::Medium  => "medium",
            ReasoningEffort::High    => "high",
            ReasoningEffort::Xhigh   => "xhigh",
            ReasoningEffort::Max     => "max",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "minimal" | "none"               => Some(Self::Minimal),
            "low"                            => Some(Self::Low),
            "medium" | "default"             => Some(Self::Medium),
            "high"                           => Some(Self::High),
            "xhigh" | "x-high" | "extra_high"=> Some(Self::Xhigh),
            "max"                            => Some(Self::Max),
            _                                => None,
        }
    }
}
```

`ReasoningOptions.budget_tokens` is retained — Gemini still uses it
internally, and the Anthropic provider still consumes it for non-effort
beta routes (see Risks).

### `crates/core/src/policy.rs`

```rust
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoutePolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_anthropic_beta: Option<String>,
    // NEW
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_mapping: Vec<MappingRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingRule {
    pub r#match: ReasoningEffort,
    pub set:     ReasoningEffort,
}

// ResolvedPolicy unchanged — only its `reasoning_effort` field is the
// output of the mapping pipeline.
```

### `RoutePolicy::resolve` — updated algorithm

```rust
pub fn resolve(&self, req: &CanonicalRequest) -> ResolvedPolicy {
    // Step 1: gather inbound effort, falling back to route default.
    let inbound_effort = req
        .generation
        .reasoning
        .as_ref()
        .and_then(|r| r.effort);
    let post_default = inbound_effort.or(self.default_reasoning_effort);

    // Step 2: apply mapping table.
    let final_effort = match post_default {
        Some(e) => self.reasoning_mapping
            .iter()
            .find(|rule| rule.r#match == e)
            .map(|rule| rule.set)
            .or(Some(e)),
        None => None,
    };

    // Step 3: anthropic headers (unchanged).
    let anthropic_headers = build_anthropic_headers(req, self);

    ResolvedPolicy {
        reasoning_effort: final_effort,
        anthropic_headers,
    }
}
```

Notes:

- `inbound_effort` is **never quantized** from a budget anymore. The
  mapping table only acts on effort strings.
- The same inbound-wins semantics apply to `budget_tokens` (untouched by
  `RoutePolicy::resolve`; providers read it directly from
  `req.generation.reasoning.budget_tokens` for the small set of cases
  where it still matters — see Provider notes below).

## Frontend decode changes

### `frontends/anthropic_messages/decode.rs`

Replace the existing `thinking.budget_tokens` → `ReasoningOptions`
mapping with an `output_config.effort`-only read. The wire structs gain
an `output_config` field; the `thinking` block becomes informational only
on this path.

```rust
// Current (removed):
//   thinking.budget_tokens  →  ReasoningOptions.budget_tokens
//
// New:
//   output_config.effort    →  ReasoningOptions.effort
//
// Notes:
// - thinking: { type: "adaptive" }   — recognized; no effort signal extracted.
// - thinking: { type: "enabled", budget_tokens: N }
//                                    — older clients; we no longer set
//                                      ReasoningOptions.budget_tokens
//                                      from this path.
//
// Behavioural change: agents on the OLD client (no effort-2025-11-24
// beta) will lose the ability to express reasoning intent unless they
// supply effort via `output_config.effort`. See Risks.
```

The Anthropic `thinking` content-block decode for assistant messages
(parsing `thinking`/`signature` blocks out of message content) is
unchanged — that path is about reading back the model's reasoning blob,
not the inbound budget signal.

### `frontends/openai_chat/decode.rs` and `openai_responses/decode.rs`

Add `"xhigh"` and `"max"` to `ReasoningEffort::parse` accepted aliases
(it already accepts `xhigh` — `max` is new). No other change.

## Provider encode changes

### `providers/anthropic/request.rs` — Anthropic native

**Always** emit:

```json
{
  "thinking":      { "type": "adaptive" },
  "output_config": { "effort": "<canonical effort as string>" }
}
```

when `resolved_policy.reasoning_effort` is `Some`. The mapping from
`ReasoningEffort` to Anthropic string is the identity for all six values
**except** `Minimal`, which has no equivalent in Anthropic's vocabulary
and is mapped to `"low"`.

The existing `effort_to_budget` table and the `thinking.budget_tokens`
encoder are removed for the effort-driven path. They remain only as a
defensive fallback for non-effort-beta routes (gated by inspecting
whether `output_config` would be accepted — practically, by whether
`anthropic-beta: effort-2025-11-24` is present in resolved headers).

```rust
fn build_reasoning(req: &CanonicalRequest) -> ReasoningBlocks {
    let effort = req.resolved_policy.reasoning_effort;
    if effort.is_none() {
        return ReasoningBlocks::default();
    }
    let want_effort_beta = req
        .resolved_policy
        .anthropic_header("anthropic-beta")
        .map(|v| v.contains("effort-2025-11-24"))
        .unwrap_or(false);

    if want_effort_beta {
        ReasoningBlocks {
            thinking: Some(json!({ "type": "adaptive" })),
            output_config: Some(json!({
                "effort": anthropic_effort_str(effort.unwrap())
            })),
        }
    } else {
        // Legacy path: derive a budget from effort like today.
        ReasoningBlocks {
            thinking: Some(json!({
                "type": "enabled",
                "budget_tokens": effort_to_budget(effort.unwrap())
            })),
            output_config: None,
        }
    }
}

fn anthropic_effort_str(e: ReasoningEffort) -> &'static str {
    match e {
        ReasoningEffort::Minimal => "low",  // no Minimal in Anthropic
        ReasoningEffort::Low     => "low",
        ReasoningEffort::Medium  => "medium",
        ReasoningEffort::High    => "high",
        ReasoningEffort::Xhigh   => "xhigh",
        ReasoningEffort::Max     => "max",
    }
}
```

### `providers/oai_chat_wire/canonical_to_chat.rs` — OpenAI Chat / Copilot

`reasoning_effort` serialisation rewrites `Xhigh` and `Max` based on
whether the target accepts `xhigh`:

```rust
fn effort_for_chat(e: ReasoningEffort, accepts_xhigh: bool) -> &'static str {
    match (e, accepts_xhigh) {
        (ReasoningEffort::Max, true)   => "xhigh",   // Copilot Claude
        (ReasoningEffort::Max, false)  => "high",    // pure OpenAI
        (ReasoningEffort::Xhigh, true) => "xhigh",
        (ReasoningEffort::Xhigh, false) => "high",
        (ReasoningEffort::High, _)     => "high",
        (ReasoningEffort::Medium, _)   => "medium",
        (ReasoningEffort::Low, _)      => "low",
        (ReasoningEffort::Minimal, _)  => "minimal",
    }
}
```

`accepts_xhigh` is a capability flag on `BackendTarget` (Copilot Claude
sets it; pure OpenAI and Codex do not). Adding the flag is a small
extension to existing `ProviderCapabilities`.

### `providers/openai_compatible/responses_api/encode_request.rs`

Same compression as OAI Chat — Responses API does not accept `xhigh` or
`max`, so both compress to `high`.

### `providers/gemini/request.rs`

Add `Max` to `effort_to_budget`. Suggested value (consistent with
existing scale): `Max => 24576`. Otherwise unchanged.

### `providers/anthropic` — header passthrough

Unchanged. The route's `default_anthropic_beta` may be used by operators
to *force* `effort-2025-11-24` on a route whose agents don't send it,
but the spec does **not** make the provider auto-inject that beta —
inbound-or-route-default only, per existing policy.

## Config schema changes

```rust
// agent-shim-config::schema
pub struct RouteEntry {
    // ... existing fields ...
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

`StaticRouter::from_config` parses both strings through
`ReasoningEffort::parse`; unknown values cause a config-load **error**
(not a warning — the previous warning-only behaviour for
`reasoning_effort` is unchanged, but unknown rules in a mapping table
are too dangerous to silently drop).

## Testing matrix

| Case | Where |
|---|---|
| `ReasoningEffort` enum gains `Max`, `parse("max")` works | `core::request` |
| Anthropic decode reads `output_config.effort: "max"` | `frontends::anthropic_messages::decode` |
| Anthropic decode ignores `thinking.budget_tokens` when present (legacy) | `frontends::anthropic_messages::decode` |
| Anthropic decode handles `thinking: { type: "adaptive" }` without panicking | `frontends::anthropic_messages::decode` |
| OAI Chat decode accepts `"max"` and parses to `Max` | `frontends::openai_chat::decode` |
| `RoutePolicy::resolve` with no mapping → unchanged behaviour | `core::policy` |
| Mapping `[max → xhigh]` rewrites inbound Max | `core::policy` |
| First rule wins | `core::policy` |
| No rule matches → passthrough | `core::policy` |
| Default-then-mapping ordering | `core::policy` |
| Anthropic provider emits adaptive+effort when beta present | `providers::anthropic::request` |
| Anthropic provider emits legacy budget when beta absent | `providers::anthropic::request` |
| Anthropic provider maps `Minimal` → `"low"` on effort beta | `providers::anthropic::request` |
| OAI Chat provider emits `"xhigh"` only when target accepts it | `providers::oai_chat_wire` |
| OAI Chat provider compresses `Max` → `"high"` on pure OpenAI | `providers::oai_chat_wire` |
| Responses API provider compresses `Xhigh`/`Max` → `"high"` | `providers::openai_compatible::responses_api` |
| Gemini `effort_to_budget(Max)` returns the expected number | `providers::gemini` |
| Config: malformed effort in `reasoning_mapping` rejects load | `config::schema` / `router::static_routes` |
| End-to-end: Anthropic frontend `output_config.effort: max` → Copilot `reasoning_effort: xhigh` via mapping `[max → xhigh]` | integration test |

## Wire evidence — Copilot Claude

Captured 2026-05-28 from VS Code Copilot Chat hitting
`api.enterprise.githubcopilot.com/v1/messages`:

```json
{
  "model": "claude-opus-4.6-1m",
  "thinking":      { "type": "adaptive", "display": "summarized" },
  "output_config": { "effort": "high" },
  "messages": [...]
}
```

A second batch on the same date with a newer Copilot client + 4.7
models captured the following payload combinations:

| Copilot UI selection | `model` | `output_config` |
|---|---|---|
| 4.7 / 1M ctx / xhigh effort  | `claude-opus-4.7-1m-internal` | **absent** |
| 4.7 / default ctx / high effort | `claude-opus-4.7-1m-internal` | `{ effort: "high" }` |
| 4.6 / 1M ctx / high effort   | `claude-opus-4.6-1m`          | `{ effort: "high" }` |
| 4.6 / default ctx / high effort | `claude-opus-4.6-1m`       | `{ effort: "high" }` |

This confirms five things relevant to this spec:

1. **Context window is selected via `model` id, not a request field.**
   `-1m` (and `-1m-internal`) suffixes pick the 1M variant; the bare
   family name is 200K. AgentShim already supports this through
   per-route `upstream_model` — no new mechanism is needed. The
   `/v1/models` catalog spec surfaces the same-family long-context
   sibling via `long_context_variant` for client discoverability, but
   routing remains operator-explicit (no dynamic SKU swap on a 1M beta
   header).

2. **The Copilot UI "max effort" selection appears to be wire-encoded
   as `output_config` *omitted*.** No `effort` field, no
   `reasoning_effort`, no header — the server defaults the effort to
   the highest tier for that account/SKU.

   We **do not mirror that behaviour**. AgentShim's
   Anthropic-as-provider encoder ALWAYS emits an explicit
   `output_config: { effort: "..." }` when
   `resolved_policy.reasoning_effort` is `Some`, including for
   `Xhigh` and `Max`. Reasoning:

   - Anthropic's documented protocol treats `output_config.effort` as
     an explicit selector. Relying on server-side defaults to mean
     "xhigh" is undocumented behaviour we can't depend on across SKUs,
     models, or future Copilot/Anthropic-native parity changes.
   - Round-tripping `Max → omit → "is this xhigh or something
     else?"` makes the resolved-policy snapshot non-reproducible.
   - The lossy compression contract for OpenAI Chat (`Xhigh → "high"`)
     becomes harder to reason about if some Anthropic targets accept
     `xhigh` while others want field omission.

   The unverified risk is that some Copilot SKU might 400 on
   `output_config: { effort: "xhigh" }`. If that surfaces in
   production, the operator escape hatch is a `reasoning_mapping`
   entry `{ match: xhigh → set: high }` on the affected route — no
   spec change required.

3. **Copilot Claude accepts the standard Anthropic effort field**
   (`output_config.effort`) when sent. The effort-suffixed model SKUs
   (`claude-opus-4.7-high`, `claude-opus-4.7-xhigh`) seen in the
   `/models` catalog are conveniences for clients that don't send
   the effort field, NOT a required encoding. We do not use them.

4. The Anthropic-as-provider encoder in this spec therefore emits the
   same `{ thinking: adaptive, output_config: { effort: ... } }`
   payload for both Anthropic-native and Copilot-Claude targets — no
   `accepts_xhigh` capability is needed on the `/v1/messages` path.
   `accepts_xhigh` remains relevant only on the OpenAI-Chat path
   (where it gates serialising `reasoning_effort: "xhigh"` vs
   compressing to `"high"`).

5. Copilot's `thinking.display: "summarized"` is an additional field
   that toggles whether reasoning summary blocks stream back. It's
   orthogonal to effort selection and is out of scope here; the
   provider passes it through verbatim from inbound or omits it
   (defaults are upstream-decided).

## Out of scope (deferred)

- **Budget-axis matching / quantization**: removed from the spec. With
  Anthropic now emitting effort strings, the old "Claude Code sends 31999
  budget" case is the legacy path only. If it becomes relevant again, we
  can add `match: { budget_tokens: {...} }` later — the YAML shape is
  intentionally extensible (`match` could become an object).
- **Match by model name / inbound headers / capability**: not in v1.
- **Per-route override of `effort_to_budget`** (Gemini's table): YAGNI.
- **`thinking.type` modes beyond `adaptive`**: Anthropic ships
  `enabled`/`disabled`/`adaptive`. v1 only learns to *read* `adaptive`
  (don't crash) and *write* `adaptive`. Other modes remain on the legacy
  path.

## Implementation order

1. Extend `ReasoningEffort` to 6 variants in `core::request`. Update
   `parse`/`as_str` and all enumerations. Unit-test boundary.
2. Add `MappingRule` and `reasoning_mapping` to `RoutePolicy`.
3. Update `RoutePolicy::resolve` to apply mapping post-default.
4. Update `frontends::anthropic_messages::decode` to read
   `output_config.effort`; tolerate `thinking: {type: "adaptive"}`.
5. Update `frontends::openai_chat::decode` /
   `openai_responses::decode` to accept `"max"`.
6. Add `accepts_xhigh` flag to `BackendTarget` (or per-provider).
7. Update `providers/oai_chat_wire/canonical_to_chat` to compress on
   `accepts_xhigh`.
8. Update `providers/openai_compatible/responses_api` similarly.
9. Update `providers/anthropic/request::build_thinking` to branch on
   `effort-2025-11-24` beta presence; emit new shape when present.
10. Extend `providers/gemini::effort_to_budget` with `Max`.
11. Extend `config::schema::RouteEntry`; update `StaticRouter`
    translation; reject malformed effort strings.
12. End-to-end integration test (`tests/integration/effort_mapping.rs`).
13. Update `README.md` (`## Reasoning / thinking effort`),
    `CONTEXT.md` (new terms), `config/gateway.example.yaml`,
    `docs/providers/anthropic.md`.

## Risks and trade-offs

- **Lossy compression at the OpenAI boundary.** Canonical `Xhigh` and
  `Max` both serialise to OpenAI `"high"`. Operators wanting distinction
  must route Anthropic-max traffic to a Copilot/Anthropic backend, not
  to pure OpenAI. README will note this explicitly.

- **`Minimal` has no Anthropic equivalent.** We map `Minimal → "low"` on
  the Anthropic effort path. This loses the "absolutely no thinking"
  signal that OpenAI's `minimal` carries. An operator who needs strict
  no-thinking on Anthropic should route to a non-thinking model or set
  the route default to skip effort entirely.

- **Legacy Anthropic clients regress on the new decode path.** A client
  that still sends `thinking.budget_tokens` and **no** `output_config`
  loses its ability to express reasoning intent — `RoutePolicy::resolve`
  will see `None` and fall back to default. This is acceptable because:
  (a) Claude Code rolled out `effort-2025-11-24` widely in late 2025,
  (b) the legacy path was already serving a shrinking population, and
  (c) the route-level `reasoning_effort: medium` default catches the
  common case. We accept the trade-off explicitly rather than maintaining
  two decode paths in canonical.

- **Effort-beta not auto-injected.** When Anthropic native is the
  upstream but the agent doesn't send `effort-2025-11-24` header,
  forwarding `output_config` may be rejected. The operator must add
  `anthropic_beta: effort-2025-11-24` on the route. Documented in
  README.

- **Future Anthropic vocabulary changes.** If Anthropic adds yet another
  level (e.g. `"god-mode"`), we need a 7th canonical variant or an
  unknown-passthrough mode. The 6-variant enum is the right point on
  the simplicity/flexibility curve as of 2026-05-28; we accept the
  maintenance cost of a future enum bump rather than going to a free
  string today.
