# AgentShim 0.8.0 — Release Notes

**Release date:** 2026-05-29
**Previous version:** [0.7.0](https://github.com/anthropics/agent-shim/releases/tag/v0.7.0)

## Summary

v0.8.0 ships two cross-cutting features and one critical observability
fix:

1. **Per-route reasoning effort mapping.** Six-value canonical effort
   vocabulary (`minimal/low/medium/high/xhigh/max`) plus a per-route
   `reasoning_mapping: [{ match, set }]` rewrite table. Claude Code's
   "max effort" can now land on Copilot's "xhigh" without the agent
   needing to know which dialect is downstream.
2. **Public `/v1/models` endpoint + admin catalog tooling.** OpenAI-shape
   public catalog endpoint enriched with upstream-discovered metadata
   (context window, family, capabilities). New `agent-shim show-catalog`
   CLI, admin `/admin/catalog`, optional `print_catalog_on_start`,
   optional `validation.strict_upstream_models` typo gate.
3. **Foreground stdout fix.** `agent-shim serve` no longer silently
   drops stdout logs when neither `logging.file` nor `otel.endpoint`
   is configured.

This release also enriches the per-request log line with inbound vs
outbound reasoning effort and the upstream model's catalog context
window so operators can see at a glance what AgentShim decided to
forward upstream.

## Upgrade notes

| Audience | Action |
|---|---|
| **Operators** | Drop in the new binary. Existing configs continue to load (every new field is optional with a backwards-compatible default). The default-permissive posture means upgrade carries no behavior change unless you opt into the new knobs. |
| **`BackendProvider` implementors (internal crate consumers)** | `list_models` signature changed from `Option<BTreeSet<String>>` to `Option<BTreeMap<String, ModelMetadata>>`. Existing impls that only know model names migrate trivially via `BTreeMap::from_iter(names.into_iter().map(|n| (n, ModelMetadata::default())))`. |
| **Plugin developers** | No plugin-API changes. The PluginRegistry and four hook anchors (H2/H3/H5/H7) are unchanged. |
| **Custom subscribers using `agent-shim-observability`** | No call-site changes. The `init()` signature and `TracingHandles` shape are unchanged. The fix is fully internal. |

## What's new

### Per-route reasoning effort mapping

Three building blocks land together:

#### 1. Six-value canonical effort enum

The `ReasoningEffort` enum now covers every level expressible by the
upstream dialects we support:

```rust
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}
```

Parse aliases: `none → Minimal`, `default → Medium`,
`x-high|extra_high → Xhigh`. Decode lands the canonical value in
`canonical.generation.reasoning.effort`; providers translate from there.

#### 2. Per-route mapping table

The `RoutePolicy` carries an ordered rewrite table:

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
        set:   xhigh   # also lift `high` on this route
```

Rules evaluate top-to-bottom; first match wins; unmatched passes
through. Both `match` and `set` are canonical-vocabulary values, not
dialect strings. Unknown values fail config load with a precise field
path.

The route default fills inbound holes before mapping runs, so
`reasoning_effort + reasoning_mapping` chain as expected:

```yaml
routes:
  - frontend: anthropic_messages
    model: claude-opus-4-7
    upstream: copilot
    upstream_model: claude-opus-4-7
    reasoning_effort: max           # default when inbound is silent
    reasoning_mapping:
      - match: max
        set:   xhigh
```

#### 3. Provider-side compression rules

Effort values that have no native dialect equivalent are compressed
at the encoder so the upstream sees something it understands:

| Upstream | `xhigh` / `max` behavior |
|---|---|
| Copilot Chat | passes through (`ProviderCapabilities::accepts_xhigh = true`) |
| Copilot — Anthropic family | passes through as `output_config.effort` |
| OpenAI Chat (non-Copilot) | compressed to `"high"` |
| OpenAI Responses API | compressed to `"high"` |
| Gemini | `Max` → `thinkingConfig.thinkingBudget = 24576` |
| Anthropic via `effort-2025-11-24` beta | emits `output_config.effort` + `thinking: { type: adaptive }` |
| Anthropic legacy `thinking.budget_tokens` | preserved (separate code path) |

Inbound `minimal` is mapped to Anthropic's `"low"` since Anthropic's
effort vocabulary has no `minimal`.

### Public `/v1/models` + catalog tooling

#### Public endpoint

```bash
$ curl http://localhost:8787/v1/models | jq '.data[] | {id, upstream}'
{
  "id": "claude-opus-4-7",
  "upstream": { "provider": "copilot", "model": "claude-opus-4.7" }
}
{
  "id": "gpt-5.5",
  "upstream": { "provider": "copilot", "model": "gpt-5.5" }
}
```

Each record carries the chain head and (where upstream surfaced it)
a `capabilities` block with `context_window_tokens`,
`max_output_tokens`, `family`, and supports-flags.

Filters: `?frontend=anthropic_messages|openai_chat|openai_responses`
and `?capability=vision|tool_calls|streaming|structured_outputs|reasoning_effort`.
`GET /v1/models/:id` returns one record or 404.

#### Admin endpoint

`GET /admin/catalog` returns the same data plus per-route policy
(default effort, mapping table) and full `ModelMetadata`. Reachable
only on the admin listener.

`POST /admin/discover` is currently a `501 Not Implemented` stub.
Use `POST /admin/reload` or SIGHUP for now — full reload re-runs
discovery.

#### CLI

```bash
agent-shim show-catalog --config gateway.yaml              # table
agent-shim show-catalog --config gateway.yaml --format json
agent-shim show-catalog --config gateway.yaml --strict     # CI gate
```

`--strict` exits non-zero when any route alias has no upstream
metadata — useful for catching typos in CI before deploy.

#### Startup options

```yaml
logging:
  print_catalog_on_start: true     # dump catalog to stderr after discovery

validation:
  strict_upstream_models: true     # hard-fail on typos at startup
```

### Request log enrichment

Every request now emits a `→` line with the full routing decision
and a `←` line with the outcome:

```text
→ /v1/messages | model: claude-opus-4-7 → claude-opus-4.7-1m-internal
  | bodyBytes: 167 | maxTokens: 30 | ctxWindow: 1000000
  | stream: false | reasoning: max → xhigh
← /v1/messages (unary) | model: claude-opus-4-7 → claude-opus-4.7-1m-internal
  | input: 17 | output: 6 | 1.1s
```

The `reasoning:` field shows the per-request decision in both
directions:

| Case | Renders as |
|---|---|
| Inbound silent + no policy | `none` |
| Inbound `x`, no rewrite | `x` |
| Inbound `x`, mapping → `y` | `x → y` |
| Inbound silent, default `y` | `(default) → y` |
| Inbound `x`, mapping drops it | `x → (dropped)` |

The `ctxWindow` field comes from the discovered model catalog. When
the upstream's `list_models` returned no metadata it renders as `?`.

## Critical fix: foreground stdout was silent without `logging.file`

In v0.7.0, the composite layer assembled by `tracing_setup::init`
became an empty `Vec<Box<dyn Layer<S>>>` whenever both
`otel_layer` and `file_layer` were absent. `Vec<Box<dyn Layer<S>>>`
implements `Layer<S>`, but an empty Vec's `Layer::enabled()`
returns `false` — which short-circuited the subscriber chain and
masked events from the downstream stdout `fmt` layer.

**Symptom:** running `agent-shim serve` foreground with no
`logging.file` and no `otel.endpoint` produced zero stdout. Adding
`logging.file:` made stdout work; removing it killed stdout again.
Setting `RUST_LOG=trace` surfaced reqwest's logs (which arrive via
`tracing-log` and bypass the per-layer enabled check) but never
`agent_shim`'s own events.

**Fix:** wrap the composite as `Option<Vec<_>>`. When both inner
layers are absent, the composite becomes `None` — a true no-op
layer — instead of an empty Vec that denies every event.

Also: `try_init()` errors are no longer silently swallowed via
`let _ = …`. Future regressions surface via `eprintln!` instead of
going invisible.

This affected every foreground deploy that didn't configure file
logging (a common operator pattern on Linux/macOS without systemd-journald
forwarding). It did **not** affect Windows Service deploys (which
typically configure `logging.file`) or any deploy with OTel enabled.

## Workspace stats

| | v0.7.0 | v0.8.0 | Δ |
|---|---|---|---|
| Crates | 11 | 11 | 0 |
| Source files | 269 | 281 | +12 |
| LoC (excluding tests) | ~22 k | ~24 k | +~2 k |
| Tests | 938 | 1001 | +63 |
| Clippy warnings | 0 | 0 | — |

## Documentation

- [`README.md`](../README.md) — top-level overview, capability matrix
  (v0.8), reasoning effort + mapping examples, model catalog usage,
  logging walkthrough
- [`docs/configuration.md`](configuration.md) — full YAML reference
- [`docs/observability.md`](observability.md) — metrics + tracing
  surface, including the new `→` / `←` request log fields
- [`CHANGELOG.md`](../CHANGELOG.md) — full per-feature changelog
- Specs:
  - [`docs/superpowers/specs/2026-05-28-reasoning-effort-mapping-design.md`](superpowers/specs/2026-05-28-reasoning-effort-mapping-design.md)
  - [`docs/superpowers/specs/2026-05-28-models-endpoint-and-catalog-design.md`](superpowers/specs/2026-05-28-models-endpoint-and-catalog-design.md)
- Plans:
  - [`docs/superpowers/plans/2026-05-28-reasoning-effort-mapping.md`](superpowers/plans/2026-05-28-reasoning-effort-mapping.md)
  - [`docs/superpowers/plans/2026-05-28-models-endpoint-and-catalog.md`](superpowers/plans/2026-05-28-models-endpoint-and-catalog.md)

## What's next

v0.9+ candidates (explicitly deferred from v0.8):

- **`POST /admin/discover` atomic catalog refresh.** Reserved
  endpoint shipped as a 501 stub; needs ArcSwap of ModelIndex to
  do safely under hot reload.
- **Learned realised-cost tracking (rolling EWMA).** Observed token
  counts feed back into the cost filter.
- **Distributed cost-filter state.** Multi-instance deployments
  share filter counts.
- **Distributed breaker / rate-limit state.**

No removals, no breaking config changes planned for v0.9 — the
frozen-core invariant continues to apply to `crates/core/`.
