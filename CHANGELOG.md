# Changelog

All notable changes to AgentShim are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

(no unreleased changes yet)

## [0.8.0] — 2026-05-29

Per-route reasoning effort mapping, public model catalog, and the
operator-side observability that lives around them.

This release introduces a six-value canonical reasoning-effort
vocabulary that lets agents address the entire range of upstream
"thinking" knobs through a single dialect-agnostic enum. Routes can
rewrite effort on the fly with a `reasoning_mapping` table — so
Claude Code's "max" can land on Copilot's "xhigh", Codex's "high"
on a DeepSeek route that prefers "medium", and so on, without the
agent needing to know which dialect is downstream.

The model catalog also gets a public face: `GET /v1/models` exposes
AgentShim's accepted aliases in OpenAI shape, each enriched with
upstream-discovered metadata (context window, family, capabilities).
A new `agent-shim show-catalog` CLI prints the same data from
configuration alone — useful for CI typo gates.

`crates/core/` is unchanged across the entire 0.7→0.8 cycle except
for additive vocabulary growth (`ReasoningEffort::{Xhigh, Max}`,
`ResolvedPolicy::reasoning_budget_tokens`, `RoutePolicy::reasoning_mapping`,
`catalog::{ModelRecord, ModelMetadata, ModelSupports, UpstreamRef, ModelCatalog}`).
The frozen-core invariant is honored — no field removals, no
behavioral changes on existing variants.

### Added (reasoning effort mapping)

- **Six-value canonical effort enum.** `ReasoningEffort::{Minimal,
  Low, Medium, High, Xhigh, Max}` covers every level expressible
  by Anthropic, OpenAI, Copilot, Gemini, and DeepSeek. Aliases:
  `none → Minimal`, `default → Medium`, `x-high|extra_high → Xhigh`.
- **`RoutePolicy::reasoning_mapping: Vec<MappingRule>`.** Per-route
  ordered rewrite table. First match wins; unmatched passes through.
  Both `match` and `set` are canonical-vocabulary effort values, not
  dialect strings. Route default fills inbound holes before mapping
  runs.
- **`ResolvedPolicy::reasoning_budget_tokens`.** Token-quantified
  reasoning budget carried alongside effort. v0.6 copied this from
  the inbound request directly; v0.8 keeps that behavior — the
  field exists so providers that need a number (Gemini, legacy
  Anthropic `thinking.budget_tokens` shape) read from one source.
- **Anthropic `effort-2025-11-24` beta plumbed end-to-end.** When the
  inbound request carries `anthropic-beta: effort-2025-11-24`, the
  Anthropic provider emits `output_config.effort` + `thinking: {
  type: adaptive }` instead of the legacy `thinking.budget_tokens`
  shape. Inbound decode of `output_config.effort` lands the value
  in `canonical.generation.reasoning.effort`.
- **`ProviderCapabilities::accepts_xhigh`.** Providers that natively
  understand "xhigh" (Copilot Chat) opt in via this capability;
  others (OpenAI Chat, Responses) compress `xhigh`/`max` to `"high"`
  at the encoder.
- **Gemini `Max` budget map.** `ReasoningEffort::Max` maps to
  `thinkingConfig.thinkingBudget = 24576`; matches the
  "thinking-strong" Gemini tier.
- **Effort string validation at config load.** `reasoning_mapping`
  entries with non-canonical effort values fail
  `validate_config` with a precise field path.
- **End-to-end protocol test.** New
  `crates/protocol-tests/tests/effort_mapping_end_to_end.rs`
  exercises the Claude Code `max` → Copilot `xhigh` rewrite through
  the full pipeline (decode → policy resolve → mapping → encode).

### Added (`/v1/models` + catalog)

- **Public `GET /v1/models`** OpenAI-shape endpoint. Returns every
  alias AgentShim accepts. `?frontend=anthropic_messages|openai_chat|openai_responses`
  filters by inbound dialect; `?capability=vision|tool_calls|streaming|structured_outputs|reasoning_effort`
  drops records whose metadata lacks the capability.
  `GET /v1/models/:id` returns one record or 404.
- **Each catalog record carries** the resolved upstream chain head
  and, where the upstream's own `list_models` surfaced it, a
  `capabilities` block (`context_window_tokens`, `max_output_tokens`,
  `family`, `supports.{vision, tool_calls, streaming, structured_outputs}`).
- **Admin `GET /admin/catalog`.** Operator-only view including
  per-route `reasoning_mapping`, `default_reasoning_effort`, and the
  full `ModelMetadata`. Reachable only on the admin listener.
- **Admin `POST /admin/discover` (stub, 501).** Reserved for atomic
  ModelIndex refresh; v0.8 ships a stub. Use `POST /admin/reload`
  or SIGHUP for now — the full reload re-runs discovery.
- **`agent-shim show-catalog` CLI.** Prints the catalog from disk
  config alone without starting the server. `--format table` (default)
  or `--format json`. `--strict` exits non-zero when any route alias
  has no upstream metadata — handy for CI.
- **`logging.print_catalog_on_start: true`.** Optional dump of the
  catalog to stderr after model discovery completes. Off by default
  so CI / automation stays silent.
- **`validation.strict_upstream_models: true`.** Hard startup error
  when any route's `upstream_model` isn't present in the discovered
  upstream catalog. Catches typos like `claude-opus-47` (missing
  dot). Off by default because not every upstream exposes `/models`.
- **Copilot `/models` enrichment.** `github_copilot::parse_models_response`
  now extracts the full capability blob — family, limits, supports —
  populating `ModelMetadata` for every Copilot model. Backwards
  compatible with providers whose `list_models` returns `None`.

### Added (request log enrichment)

- **Inbound vs outbound reasoning + ctxWindow in the request log.**
  The per-request `→` line now shows the effort decision in both
  directions and the upstream model's catalog context window:

  ```text
  → /v1/messages | model: claude-opus-4-7 → claude-opus-4.7-1m-internal
    | bodyBytes: 167 | maxTokens: 30 | ctxWindow: 1000000
    | stream: false | reasoning: max → xhigh
  ```

  Effort field compresses across five cases: identical inbound/
  outbound prints as `"high"`; rewrite as `"max → xhigh"`; route
  default filling silent inbound as `"(default) → high"`; a
  mapping that clears effort as `"high → (dropped)"`; absent on
  both as `"none"`. `ctxWindow` comes from the discovered model
  catalog; falls back to `"?"` when the upstream's `list_models`
  returned no metadata.

### Fixed

- **`agent-shim serve` stdout no longer silent without `logging.file`
  or `otel.endpoint`.** The composite layer assembled in
  `tracing_setup::init` had been built as an empty `Vec<Box<dyn
  Layer<S>>>` when both inner layers were absent, and the empty Vec's
  `Layer::enabled()` returned false — short-circuiting the
  subscriber and masking events from the downstream stdout `fmt`
  layer. Events arriving via `tracing-log` (reqwest, hyper) bypassed
  the per-layer check via the LogTracer's own dispatcher path, which
  is why `RUST_LOG=trace` surfaced those but never `agent_shim`'s
  own logs. Wrapping the composite as `Option<Vec<…>>` makes the
  empty case a true no-op layer.
- **`agent-shim serve` no longer swallows tracing subscriber init
  errors silently.** `try_init()` errors now surface via `eprintln!`
  so subsequent regressions don't go invisible.
- **Anthropic Messages requests no longer emit a duplicate `→` log
  line.** The proxy_raw branch's pre-decode log line was firing
  even when proxy_raw was not applicable to the inbound frontend
  (Copilot upstreams don't implement Anthropic raw passthrough).
  Moved the log into the `Ok(Some(...))` success arm so the
  fall-through canonical path emits only its (full structured)
  line.

### Internal

- **`BackendProvider::list_models` returns `Option<BTreeMap<String,
  ModelMetadata>>`.** Was `Option<BTreeSet<String>>` in v0.7.
  ModelIndex stores the full metadata blob; existing call sites
  that only wanted names migrate trivially.
- **`ModelIndex::with_metadata` / `::metadata` / `::provider_models`
  / `::providers` accessors.** The router stores discovered
  upstream metadata so the resolver and pipeline can answer
  catalog/capability questions without re-querying upstream.
- **`ModelResolver::list_catalog`.** Walks every route × ModelIndex
  combination, populates `ModelRecord` entries with chain heads
  and metadata. Used by both the public endpoint and the admin
  endpoint.
- **`format_effort_log(inbound, outbound) -> String`.** Pure helper
  in `crates/gateway/src/pipeline.rs` for the 5-way log
  compression. Five unit tests cover the matrix.
- **Workspace tests: 1001 passing.** +10 new tests against the v0.7
  baseline (5 reasoning effort, 1 effort_mapping_end_to_end, 5
  format_effort_log).

## [0.7.0] — 2026-05-22

Phase 7 complete. The plugin system goes live, and the cross-platform
operations work (Windows Service, file logging) that landed during the
Phase 7 cycle ships alongside it.

This is the first release where AgentShim has an extensible plugin
surface for cross-cutting request shaping. Three built-in plugins ship
in v0.7.0 (`pii_scrubber`, `prompt_compressor`, `usage_recorder`), each
behind its own Cargo feature (all default-on). Plugin configuration is
hot-reloadable via SIGHUP / `POST /admin/reload`; reload validation is
all-or-nothing (no partial commit). The empty-registry hot path has a
measured overhead of < 1 µs / hook on commodity hardware.

`crates/core/` is unchanged across the entire Phase 7 cycle (frozen-core
invariant honored — diff against the v0.6.1 baseline is empty).

### Added (plugin system — Phase 7)

- **Plugin trait + registry** (P01-P02): `Plugin` trait with four hooks
  (`on_decoded_request`, `on_resolved`, `on_stream_event`,
  `on_response_complete`). `PluginRegistry` with per-route subscription
  plans and per-hook timeouts. Built atop a new `agent-shim-tokens`
  crate (cl100k token counting via tiktoken-rs).
- **Config integration** (P03): `plugins:` top-level YAML block and
  `routes[].plugins:` per-hook lists. Layer-A validation (undeclared
  references, `timeout_ms == 0`, duplicate names).
- **Pipeline integration** (P04): four anchor points in
  `pipeline::dispatch` wire the hooks at the correct moments. Universal
  `H7Guard` covers all three streaming frontends. `HandlerError::PluginFailed`
  with 400/502 envelope mapping.
- **Observability** (P05): `plugin.invoke` tracing spans,
  `agent_shim_plugin_invocations_total` and
  `agent_shim_plugin_duration_seconds` metrics. Shutdown flush of
  in-flight H7 spawns with `agent_shim_plugin_h7_dropped_at_shutdown_total`
  counter.
- **Registry builder** (P06a): `PluginRegistry::build` constructor
  performing Layer-B validation (kind lookup, factory parse, hook
  subscription mismatch). `FactoryDependencies` lets factories validate
  upstream references at startup.
- **`pii_scrubber`** (P06b1): regex-based PII redaction on H2 (inbound)
  and H5 (outbound). Behind the `pii_scrubber` Cargo feature
  (default-on). Per-rule match counter.
- **`prompt_compressor`** (P06b2): token-aware conversation compression
  with three strategies — `drop_old_turns`, `truncate_to_tokens`,
  `summarize_old_turns` (with upstream call + timeout + fallback).
  Behind the `prompt_compressor` Cargo feature (default-on).
- **`usage_recorder`** (P06a): on-H7 Prometheus + log sinks for usage
  telemetry. Behind the `usage_recorder` Cargo feature (default-on).
- **Hot reload** (P07): `PluginRegistry` is bundled into `AppSnapshot`;
  reload-time rebuild via Layer-B validation. Failure rejects the entire
  reload atomically (no partial commit) → HTTP 400 from `/admin/reload`
  with `{"ok": false, "errors": ["plugin validation error: ..."]}`.
  In-flight requests observe the snapshot bound at the top of
  `dispatch`; subsequent reloads do not affect them. Protected by
  `crates/gateway/tests/plugins_reload.rs::reload_swap_isolates_in_flight_requests`.
- **Integration tests** (P07): H5 alternate-drop, H5 mid-stream error
  frame, `on_error: skip`, reload happy path, reload bad-config
  rejection, in-flight isolation under reload.
- **Microbench** (P07): `crates/plugins/benches/empty_registry_overhead.rs`
  protects the zero-overhead-empty-registry property. Observed median
  ~827 ns on commodity hardware.

### Added (operations)

- **Windows Service support.** New
  `agent-shim service install|uninstall|start|stop|restart|status`
  subcommands on Windows. Run agent-shim as a long-lived background
  service with SCM state visibility (port-bind-then-Running semantics)
  and graceful shutdown via the existing axum drain path. See
  `docs/deployment.md`.
- **Cross-platform file logging.** New optional `logging.file` config
  block (path, format, rotation, max_files). Backed by `tracing-appender`
  with daily/hourly rotation and async writes. See
  `docs/observability.md#file-logging`.
- **Linux systemd example** at `deploy/agent-shim.service`. Reuses the
  existing SIGHUP reload handler for `systemctl reload`.

### Changed

- `crates/gateway/src/state.rs`: `plugins` field moved from `AppCore`
  (immutable) to `AppSnapshot` (hot-swappable). All pipeline read-sites
  updated to read through the snapshot.
- `crates/gateway/src/reload_trigger.rs`: `ReloadOutcome` gains a
  `PluginValidation(String)` variant. `crates/gateway/src/admin/reload_handler.rs`
  maps it to HTTP 400.
- `agent_shim_config_reloads_total` counter gains a new `result` label
  value: `"plugin_validation_error"`.

### Internal

- New crates: `agent-shim-tokens`, `agent-shim-plugins`.
- `crates/core/` unchanged across all of Phase 7 (frozen-core invariant
  preserved against the v0.6.1 baseline).
- New dev-dependency: `eventsource-client` (P07 test-only).
- New dev-dependency: `criterion` for `agent-shim-plugins` bench-only.
- `clippy.toml` now pins `msrv = "1.80"` so clippy lints respect the
  workspace's MSRV.
- New plugin docs in `README.md` and `docs/architecture.md` (P07 T11).

### Migration notes

- v0.6.x → v0.7.0: no required config changes for deployments without
  plugins. The empty `PluginRegistry` is a zero-overhead identity path.
- To opt into plugins, add a top-level `plugins:` block and reference
  plugin names from `routes[].plugins.on_{hook}:` lists. See the README
  "Plugins" section.
- Plugin section reload semantics: a Layer-B failure (unknown kind,
  factory parse error, hook subscription mismatch) atomically rejects
  the entire reload; the previous configuration continues running.

## [0.6.1] — 2026-05-13

Patch release closing the five Minor items deferred from Phase 6's
P04 quality review (M-1, M-3, M-6, M-7, M-8). The headline change is
image-aware cost estimation: the per-request cost-cap filter now
accounts for image content blocks alongside text, dispatching by
inbound `FrontendKind` to vendor-specific token math. Other closures
are mechanical cleanup (`FilterReason::TiktokenFallback` removed,
`upstream_*` accessors canonicalised), the `#[derive(Metric)]`
proc-macro for single-site metric registration, and the `PolicyVec<T>`
length-invariant wrapper. v0.6.1 is the first release to add to
`crates/core/` since v0.5.0; the addition is governed by the four
discipline rules in [ADR-0007](docs/adr/0007-frozen-core-lift-discipline.md).
`crates/frontends/` and `crates/providers/src/` remain at zero diff
against the v0.6.0 baseline.

### Added
- **`ImageTokenEstimator` trait + `ImageSizeHint`/`ImageDetail` enums**
  in `crates/core/src/cost.rs`. Three implementations live in
  `crates/router/src/image_estimators/`: `AnthropicImageEstimator`,
  `OpenAiImageEstimator`, `ResponsesImageEstimator`. The gateway
  selects per inbound `FrontendKind`. Closes M-8.
- **`#[derive(Metric)]` proc-macro** in the new
  `crates/observability-derive/` crate. Adding a new Prometheus
  metric is now a one-site edit in
  `crates/observability/src/metrics/catalog.rs`. Closes M-6.
- **`PolicyVec<T>` length-invariant wrapper** in
  `crates/router/src/policy_vec.rs`. Makes "one policy per chain
  element" a type-level invariant; removes the runtime
  `debug_assert_eq!` checks in `resilient_caller.rs`. Closes M-7.
- **Structural-parity test** for metric registration
  (`crates/observability/tests/derive_metric_parity.rs`) anchored
  to the v0.6.0 baseline fixture.
- **ADR-0007** — disciplined lift of the frozen-core invariant
  (`docs/adr/0007-frozen-core-lift-discipline.md`).

### Changed
- **Cost estimation is now image-aware.** `estimate_request_cost`
  takes an `&dyn ImageTokenEstimator`; the cost-cap filter's
  per-request estimate accounts for image content blocks alongside
  text. The estimator dispatches by inbound `FrontendKind` via a
  small selector in `crates/gateway/src/image_estimator_selector.rs`.
  Closes M-8.
- **Frozen-core invariant lifted under discipline.** v0.6.1 is the
  first release to add to `crates/core/` since v0.5.0. The four
  discipline rules are documented in
  [ADR-0007](docs/adr/0007-frozen-core-lift-discipline.md): trait-only
  additions, category-tag-per-hunk, re-state-in-next-spec,
  other-crates-stay-frozen. Phase 7 will diff against the v0.6.1
  baseline, not v0.5.0.
- **`upstream_cost`/`upstream_tier`/`upstream_latency_budget`**
  accessors live in a single canonical location
  (`crates/config/src/upstream_accessors.rs`). Closes M-3.
- **`FilterReason` enum** lost its `TiktokenFallback` variant + its
  `"tiktoken_fallback"` label string. Both were `#[allow(dead_code)]`
  and never produced. Closes M-1.

### Removed
- **`#[allow(dead_code)] FilterReason::TiktokenFallback`** — the
  dead variant + its string label in `FilterReason::as_str` (see
  Changed above).
- **Duplicated `upstream_*` accessor functions** previously hosted
  in `crates/config/src/validation.rs` and
  `crates/router/src/cost_filter.rs`. Both now import from
  `agent_shim_config::upstream_accessors`.
- **Runtime `debug_assert_eq!` length checks** on `retry_policies`
  and `breaker_policies` in `resilient_caller.rs`. Replaced by
  the `PolicyVec<T>` type-level invariant.

### Internal
- New crate `agent-shim-observability-derive` (proc-macro). Workspace
  dependency edge: observability → observability-derive.
- New module `crates/router/src/image_estimators/` with three impls.
- New module `crates/gateway/src/image_estimator_selector.rs` with
  one selector function keyed on `FrontendKind`.
- New module `crates/router/src/policy_vec.rs` (`pub(crate)` only).
- New module `crates/config/src/upstream_accessors.rs` (canonical
  per-axis match-on-variant helpers).

### Known limitations (still v0.7+)
- Realised-cost feedback loop (rolling EWMA over observed token
  counts) — see [ADR-0006](docs/adr/0006-cost-aware-routing.md)
  Open Questions.
- Distributed cost-filter state across multiple gateway instances.
- Agent-driven routing preferences (`agent-shim-budget` header).
- Image dimension extraction from `BinarySource` (estimator always
  receives `ImageSizeHint::Unknown` in v0.6.1).
- Audio / file content end-to-end.
- Redacted request/response capture & replay.
- k8s manifests / Helm chart.

## [0.6.0] — 2026-05-12

Phase 6 release: the gateway becomes **cost-aware**. Three subsystems
close at once — outbound `traceparent` propagation, hot-reloadable
rate-limit policy, and four-axis cost-aware routing (tier · per-token
cost · p95 latency budget · per-route cost cap). The two v0.5 "Known
limitations" are resolved. Operators upgrading from v0.5 must add a
single required field per upstream (`tier:`); everything else is
additive.

### Added

#### Outbound `traceparent` propagation (Plan 01)

- New `agent_shim_providers::ProviderHttpClient` wraps every
  provider's `reqwest::Client`. Auto-injects W3C `traceparent` (and
  `tracestate` when present) from the current `Span`'s OTel context
  on every outgoing request. End-to-end distributed traces through
  the gateway work without operator action.
- New `agent_shim_observability::inject_context_into_headers` helper
  drives the injection from a single point so all five providers share
  the same propagator config.
- Mockito-backed integration tests in
  `crates/gateway/tests/outbound_traceparent.rs` verify the header
  shape (`^00-[0-9a-f]{32}-[0-9a-f]{16}-(00|01)$`) and that the
  outbound trace-ID matches the inbound `traceparent` (single trace
  end-to-end).
- Closes the v0.5 "Outbound `traceparent` propagation" Known
  Limitation.

#### Rate-limit reload (Plan 02)

- `AppCore::limiter_registry` moves from `Arc<LimiterRegistry>` to
  `Arc<ArcSwap<LimiterRegistry>>`. The reload-applying task rebuilds
  the registry from the new `rate_limit.*` policy and atomically swaps
  it after the `AppSnapshot` swap.
- A reload that changes any `rate_limit.*` field takes effect on the
  very next request — no process restart required.
- Existing in-flight buckets are **replaced** (not migrated) — the
  `governor` crate that backs the buckets does not expose retuning,
  so the operator-visible behaviour is "swap to new policy with a
  fresh token allowance" rather than "preserve token counts across
  the swap". Documented in
  [`docs/observability.md`](docs/observability.md#rate-limit-buckets-across-reload).
- Covered by `crates/gateway/tests/reload_rate_limit_buckets_reset.rs`.
- Closes the v0.5 "Rate-limit policy effect on reload" Known
  Limitation.

#### Cost-aware routing (Plan 03 + Plan 04)

- Four orthogonal routing axes per route:
  - `tier` filter (`economy` / `standard` / `premium`)
  - `cost` per token (USD-denominated)
  - `p95_latency_budget_ms` per upstream
  - `max_cost_usd` per route
- New `crates/router/src/cost_filter.rs` runs as a pre-chain-walk
  pass between the rate-limit gate and the v0.4 `ResilientCaller`
  chain walk. Pure function over `(chain, route, request, config, probe)`
  — no I/O, no metrics emission inside the function. Survivors retain
  original chain order; the operator's "preferred upstream" intent
  from v0.4 is preserved.
- New `crates/router/src/cost_estimate.rs` computes an upper-bound
  per-request USD estimate using `tiktoken-rs`'s `cl100k_base` encoder
  (lazily initialised via `OnceLock`) and the request's `max_tokens`
  ceiling (default 4096). Only `ContentBlock::Text` blocks contribute
  to the input-token estimate today; ToolCall / ToolResult / Image /
  Reasoning blocks are skipped (deferred to v0.7 per ADR-0006).
- New `LatencyProbe` trait in `crates/router/src/latency_probe.rs`;
  Prometheus-backed `PrometheusLatencyProbe` implementation in
  `crates/gateway/src/latency_probe.rs` reads the
  `agent_shim_upstream_duration_seconds` histogram via the
  `metrics-exporter-prometheus` text scrape.
- When the filter empties the chain, the gateway short-circuits with
  HTTP 503 `NoEligibleUpstream`. The response body lists each skipped
  upstream and the per-axis reason, rendered through the inbound
  frontend's existing error envelope (Anthropic, OpenAI Chat, OpenAI
  Responses).
- New metric `agent_shim_cost_filtered_total{reason, upstream, route}`
  with reasons `tier / latency / cap / latency_unknown / tiktoken_fallback`.
  `latency_unknown` and `tiktoken_fallback` are **notes**, not skips —
  the upstream still survives but the operator sees the warm-up /
  encoder-fallback in the counter.
- Five new schema fields:
  - upstream-level: `tier`, `cost.input_per_million_usd`,
    `cost.output_per_million_usd`, `p95_latency_budget_ms`
  - route-level: `min_tier`, `max_cost_usd`
- Validation rules 15-18 added (see
  [`docs/configuration.md`](docs/configuration.md#validation-rules-15-18)).

### Changed

- **`AppCore::limiter_registry` field type** — `Arc<LimiterRegistry>` →
  `Arc<ArcSwap<LimiterRegistry>>`. Integration tests that construct
  `AppCore` directly need to wrap their `LimiterRegistry::disabled()`
  in `ArcSwap::from_pointee(...)`.
- **`ResilientCaller::new` signature** — now takes an extra
  `Arc<dyn LatencyProbe>` argument. Tests that construct it directly
  should pass `Arc::new(DisabledLatencyProbe)` (when latency-axis
  evaluation is not desired) or a `MockLatencyProbe::with([...])` for
  scenario-specific p95 stubs.

### Breaking

⚠ **`tier` is required on every upstream.** Configs without a
`tier:` line on every upstream will fail to parse at startup.

**Upgrade action:** add one line per upstream in `gateway.yaml`:

```yaml
upstreams:
  my_upstream:
    type: open_ai_compatible
    # existing fields...
    tier: standard           # one of: economy | standard | premium
```

This is the **only** breaking change in v0.6. All other v0.5 config
shapes continue to parse unchanged.

### Frozen-core invariant changes

The Phase 5 invariant covered `crates/core/`, `crates/frontends/`,
**and** `crates/providers/src/`. Phase 6 narrows the scope:

- **Phase 6 invariant** — `git diff 44749de -- crates/core/ crates/frontends/`
  is empty at the v0.6.0 release tag. The core canonical model and
  every frontend adapter remain untouched, as in every prior phase.
- **`crates/providers/src/` is unfrozen for Phase 6.** Plan 01 modifies
  `crates/providers/src/http_client.rs` (promotes `pub(crate)` →
  `pub`, returns `ProviderHttpClient` instead of bare `reqwest::Client`)
  and renames the `client: reqwest::Client` field to
  `client: ProviderHttpClient` on five provider modules (OpenAI
  compatible, GitHub Copilot, Anthropic, DeepSeek, Gemini). No other
  diff hunks are introduced; the change is mechanical and bounded.
- This unfreeze is the v0.6 sibling of v0.3's ADR-0003 one-time
  exception. See [ADR-0006](docs/adr/0006-cost-aware-routing.md) §2
  for the discipline rule, the audit instructions, and the v0.6
  classification of every `providers/src/` hunk.

### Deprecated

None.

### Fixed

None — this release is additive against the v0.5.0 baseline.

### Documentation

- New [`docs/adr/0006-cost-aware-routing.md`](docs/adr/0006-cost-aware-routing.md)
  records the four-axis decision + rejected alternatives + the v0.6.1 /
  v0.7 deferred-findings register.
- [`docs/observability.md`](docs/observability.md) gains a "Cost-aware
  routing (v0.6)" operator section and a "What's NOT in v0.6" running
  list; removes the two v0.5 deferrals from Known Limitations.
- [`docs/configuration.md`](docs/configuration.md) documents the five
  new schema fields (tier, cost, p95_latency_budget_ms, min_tier,
  max_cost_usd) and validation rules 15-18.
- [`docs/architecture.md`](docs/architecture.md) adds a "Cost-aware
  routing (v0.6)" subsection with the request-flow diagram and the
  cross-link to ADR-0006.

### Known limitations

- **Estimate is not realised cost.** The cost filter uses an upper
  bound from `tiktoken-rs` + `max_tokens`. Realised costs are usually
  lower; the operator-set `max_cost_usd` should be tuned with the
  semantic "reject if it COULD cost more than $X". Rolling-EWMA
  realised-cost tracking is a v0.7+ candidate (ADR-0006 Open
  Questions).
- **Single-instance state still applies.** Cost-filter counts are
  per-process; distributed cost-aware behaviour across N gateway
  replicas remains a v0.7+ candidate.
- **Tokeniser mismatch.** `tiktoken-rs`'s `cl100k_base` encoder is a
  frontier-model tokeniser and won't perfectly match every backend.
  Estimates are "good enough for filtering", not "good enough for
  billing reconciliation".

## [0.5.0] — 2026-05-09

Phase 5 release: the gateway becomes **observable and operable**.
Three subsystems — Prometheus metrics on a separate admin listener,
OpenTelemetry tracing with first-class spans (export optional via
OTLP/gRPC), and hot-reload of routing & policy config via SIGHUP or
`POST /admin/reload` — compose alongside the v0.4 resilience layer.
All three are independently configurable and absent → disabled.

### Added

#### Admin listener (Plan 01)

- New top-level `admin:` config block. When present, binds a separate
  `axum::Router` on the configured address (default
  `127.0.0.1:9100`). Hosts `/metrics`, `/healthz`, `/readyz`, and
  `/admin/reload`. When absent, no admin listener is bound — zero
  observability surface exposed.
- `/healthz` moved from the public port to the admin port. The public
  port retains `/` (returns "ok") for basic up/down probes.
- `/readyz` checks that providers are initialized and the `AppSnapshot`
  is populated.
- `AppState` pivoted to `Arc<AppCore> + Arc<ArcSwap<AppSnapshot>>`.
  Every Phase 4 field migrated unchanged into one of the two halves.
  `AppCore` is `#[derive(Clone)]` so the reload-applying task can fork
  a handle alongside the gateway server
  ([ADR-0005](docs/adr/0005-hot-reload-snapshot-model.md)).

#### Prometheus metrics (Plan 02)

- New `crates/observability/src/metrics/` module. `metrics-rs` facade
  + `metrics-exporter-prometheus`. Full metric set described at startup
  so /metrics returns text even for unhit counters.
- Request-lifecycle metrics: `agent_shim_requests_total`,
  `agent_shim_request_duration_seconds`,
  `agent_shim_request_body_bytes`, `agent_shim_in_flight_requests`.
- Resilience-layer metrics mirroring the v0.4 tracing taxonomy:
  `agent_shim_retry_attempts_total`,
  `agent_shim_retry_exhausted_total`,
  `agent_shim_fallback_transitions_total`,
  `agent_shim_breaker_state_changes_total`,
  `agent_shim_rate_limit_rejected_total`.
- Upstream call metrics:
  `agent_shim_upstream_duration_seconds`,
  `agent_shim_upstream_errors_total`.
- Token accounting: `agent_shim_tokens_input_total`,
  `agent_shim_tokens_output_total`.
- Reload metric: `agent_shim_config_reloads_total{result}`.
- Cardinality budgets enforced at the type level — high-cardinality
  values like `request_id`, `api_key_hash`, and unrouted model aliases
  flow through tracing only, never as metric labels.

#### OpenTelemetry tracing (Plan 03)

- `crates/observability/src/otel/` module. `tracing-opentelemetry`
  bridge attached to the global subscriber. When `otel.endpoint` is
  configured, exports via OTLP/gRPC; otherwise the OTel layer is `None`
  and spans flow only to the existing fmt layer.
- Span tree per request: `gateway.request` → `route.resolve`,
  `auth.verify`, `rate_limit.check`, `provider.complete` (one per
  chain element) → `retry.attempt #N` → `stream.encode`.
- Span attributes follow OTel HTTP semconv (`http.request.method`,
  `http.route`, `http.response.status_code`) plus `agent_shim.*`
  custom attrs (`frontend`, `route`, `identity`, `request_id`,
  `upstream`, `model`, `attempts`, `fallback_position`).
- **Inbound only** W3C `traceparent` extraction via a small
  `tower::Layer` — agent traces continue through the gateway as a
  parented span. **Outbound `traceparent` propagation onto upstream
  HTTP calls is deferred to v0.6** (see Known limitations).
- Parent-based sampling with operator-tuned `otel.sample_ratio`.

#### Hot-reload (Plan 04)

- Reload triggers: SIGHUP (Unix) and `POST /admin/reload`
  (cross-platform). Both feed a single mpsc channel; one task
  (`commands::serve::handle_reload`) drains it and applies the swap.
  Idempotent.
- POST body is optional — without one, the gateway re-reads the file
  at the original `--config` path; with `Content-Type: application/yaml`,
  the supplied YAML is validated and applied directly.
- Validation rules 11-14 (spec §5.5) reject reload-time changes to
  `upstreams.*` set, `server.*`/`admin.*` fields, or `otel.endpoint`.
  Returns HTTP 403 with `ImmutableFieldChanged` / `UpstreamSetChanged`.
- Other validation failures return HTTP 400 with the same JSON shape
  startup uses.
- **Breaker state survives reload, breaker policy doesn't.** The
  empirical signal the breaker has accumulated about an unhealthy
  upstream is preserved across policy changes. Covered by
  `crates/gateway/tests/reload_in_flight.rs`.
- **Rate-limit bucket reset on policy change is deferred to v0.6.**
  `LimiterRegistry` lives on the immutable `AppCore`; the reload-
  applying task only swaps `AppSnapshot`. A v0.5 reload that changes
  `rate_limit.*` therefore has no effect on running buckets until the
  process restarts. See Known limitations.

### Changed

- **`/healthz` no longer on the public port.** Liveness checks move to
  either `http://<host>:8787/` (returns "ok" if listener up) or
  `http://<host>:9100/healthz` (with admin enabled).
- `AppState::new` returns `(AppState, mpsc::Receiver<ReloadRequest>)`.
  Tests that called `AppState::new(cfg).await` need to bind both
  values.
- New deps in the workspace: `arc-swap`, `metrics`,
  `metrics-exporter-prometheus`, `opentelemetry`, `opentelemetry_sdk`,
  `opentelemetry-otlp`, `tracing-opentelemetry`.

### Deprecated

None. Both v0.4 and v0.5 config shapes are supported indefinitely. v0.4
configs (no `admin:`, no `otel:`) keep behaving exactly as in v0.4.

### Fixed

None — this release is additive against v0.4.1.

### Documentation

- New [`docs/observability.md`](docs/observability.md) operator guide
  with quick-start, /metrics scrape config, OTel collector setup,
  hot-reload examples (kubectl, SIGHUP), the metric set, and
  v0.5 known gaps.
- New [`docs/adr/0005-hot-reload-snapshot-model.md`](docs/adr/0005-hot-reload-snapshot-model.md)
  records the snapshot/policy boundary, the rate-limit deferral, and
  the rejected alternatives.
- `docs/architecture.md` gains an "Observability layer" section + the
  perf overhead targets table.
- `docs/contributing.md` gains "How to add a metric" + "How to add a
  span attribute" recipes.
- `docs/configuration.md` documents the new admin/otel blocks and
  reload-validation rules 11-14.

### Known limitations

- **No outbound `traceparent` propagation.** Inbound continues; the
  gateway does NOT inject `traceparent` onto upstream HTTP calls.
  Requires a provider-side hook the v0.4 frozen surface doesn't
  expose. Documented as a v0.6 candidate.
- **Rate-limit policy changes are inert until restart in v0.5.**
  `LimiterRegistry` lives on the immutable `AppCore`; the reload-
  applying task only swaps `AppSnapshot`. Tracked as a v0.6 follow-up
  alongside either a `governor` replacement that supports retuning or
  a token-preservation rebuild path. See ADR-0005 §3 and
  `docs/observability.md`.
- **Single-instance state still applies.** Distributed breaker and
  rate-limit state remains a v0.6+ candidate.

### Frozen-core invariant continues

`git diff v0.4.1..master -- crates/core/ crates/frontends/ crates/providers/src/`
is empty for the v0.5.0 release tag.

## [0.4.1] — 2026-05-09

Patch release: fixes a streaming termination bug on the OpenAI Chat
frontend that caused clients to hang indefinitely after the model had
finished responding.

### Fixed

- **OpenAI Chat `/v1/chat/completions` streaming response never
  terminated.** When `server.keepalive_secs > 0` (the default, 15s),
  the SSE encoder merged the canonical event stream with an infinite
  `IntervalStream` of keepalive pings via `futures::stream::select`,
  which only ends when *both* sides end. The ping side never ended,
  so even after `data: [DONE]\n\n` was emitted the HTTP body stayed
  open and clients (Codex, Cursor, Claude Code, etc.) sat on
  "waiting for reply" indefinitely. The fix mirrors the pattern
  already in use by the Anthropic Messages and OpenAI Responses
  encoders: an `AtomicBool` `done` flag flipped on `ResponseStop`
  (and on stream-level errors) gates the ping stream via
  `take_while` and trips a `terminate_on_sentinel` `scan` over the
  merged output, closing the body cleanly after `[DONE]`. Two
  regression tests with a 2-second `tokio::time::timeout` guard the
  termination contract for both `keepalive=Some(_)` and
  `keepalive=None` paths
  (`crates/frontends/src/openai_chat/encode_stream.rs`).

## [0.4.0] — 2026-05-08

Phase 4 release: the gateway becomes **resilient** as well as
protocol-translating. Four subsystems — fallback chains, per-route
retries, per-(upstream, model) circuit breakers, and four-dimensional
token-bucket rate limiting — compose under a new `ResilientCaller`
orchestrator (see [ADR-0004](docs/adr/0004-resilient-caller.md)). All
four subsystems are independently configurable and default to behavior
that preserves v0.3 wire output for healthy upstreams.

### Added

#### Per-route fallback chains (Plan 02)

- Routes can list primary + backup upstreams in an ordered
  `upstreams: [...]` array. Fallback fires on retry exhaustion against
  the current upstream when the error is fallback-eligible (network /
  upstream 5xx / upstream 429).
- Existing v0.3 singular `upstream` / `upstream_model` shape continues
  to work — internally it deserializes to a 1-element vec. Mixed
  shapes (both singular and array on the same route) are rejected at
  startup.
- Three new `HandlerError` variants: `NoUpstreamSucceeded` (HTTP 503),
  `AllBreakersOpen` (HTTP 503), and `RateLimited` (HTTP 429 +
  `Retry-After`) with dialect-correct envelopes for OpenAI Chat,
  OpenAI Responses, and Anthropic Messages.

#### Per-route retries (Plan 01)

- Exponential backoff + jitter + total-time budget within a single
  upstream. Defaults: 2 attempts, 100ms initial, 2.0× multiplier,
  ±25% jitter, 5000ms total budget.
- Default fallback eligibility: `network`, `upstream_5xx`,
  `upstream_429`. Operators override per route via
  `retry.retry_on: [...]`.

#### Per-(upstream, model) circuit breakers (Plan 03)

- Sliding-window failure-rate breaker. Trips when the failure rate
  over `window_secs` (default 60s) exceeds `failure_threshold_pct`
  (default 50%) across at least `min_requests` (default 20). Open
  state holds for `open_cooldown_secs` (default 30s) before
  transitioning to half-open.
- Half-open state allows exactly one probe via
  `AtomicBool::compare_exchange`. Probe success → Closed; probe
  failure → Open with a fresh cooldown.
- Per-(provider_name, model) keying — a misconfigured model on an
  otherwise healthy provider trips its own breaker without affecting
  siblings.

#### Four-dimensional token-bucket rate limiting (Plan 04)

- Independent buckets per: API key, route, upstream, source IP. A
  request must satisfy all applicable buckets; the first to reject
  names the dimension in the error.
- Built on the [`governor`](https://crates.io/crates/governor) crate
  for atomic, lock-free per-bucket math.
- HTTP 429 responses include `Retry-After` derived from `governor`'s
  `wait_time_from(now)`.

#### API-key auth (Plan 04)

- Keys come in via `Authorization: Bearer <key>` or
  `x-api-key: <key>`. The gateway hashes the key with SHA-256 and
  looks up the hash in `auth.keys.<sha256:hex>`. Plaintext is never
  stored or logged.
- `auth.enabled=false` (default) skips header inspection entirely
  (zero overhead).
- `auth.enabled=true, required=false`: unknown keys → tagged
  `Anonymous` (uses anonymous bucket).
- `auth.required=true`: unknown keys → HTTP 401 before any upstream
  contact.

#### Structured resilience tracing (Plan 05)

- Every retry attempt, fallback transition, breaker state change,
  rate-limit rejection, and request completion emits a structured
  `tracing` event under `target = "agent_shim::resilience"` with a
  fixed field set (see `crates/router/src/resilient_caller.rs`
  module-level doc). Identity in logs is the SHA-256 hash form
  (`sha256:<hex>`) or `anonymous` — plaintext keys never appear in
  logs.
- Per-request summary log at request end with the full chain walk.

### Changed

- **Default `retry.max_attempts: 2`** — small behavior change on
  upgrade (one extra round-trip on transient errors). Operators
  wanting strict v0.3 behavior (no retries) set
  `retry: {max_attempts: 1}` per route.
- `BackendTarget` resolution now returns `Vec<BackendTarget>` instead
  of `Option<BackendTarget>`. The first element is the primary; the
  rest are fallback candidates. v0.3 single-upstream routes produce a
  1-element vec.
- `ResilientCaller::complete` replaces the direct
  `provider.complete()` call in `pipeline.rs`. All four subsystems
  compose at this single point; see
  [ADR-0004](docs/adr/0004-resilient-caller.md).

### Deprecated

None. Both v0.3 and v0.4 config shapes are supported indefinitely.

### Fixed

None — this release is additive against the v0.3 baseline.

### Documentation

- New [`docs/resilience.md`](docs/resilience.md) operator-facing guide
  with quick-start config, SHA-256 key generation recipe, retry
  tuning, layering walkthrough, log-line reference, and the
  multi-instance caveat.
- New [`docs/adr/0004-resilient-caller.md`](docs/adr/0004-resilient-caller.md)
  records the orchestrator-pattern choice over middleware-per-subsystem
  and inline-in-pipeline alternatives.
- `docs/architecture.md` capability matrix gains a Resilience row;
  performance overhead targets paragraph from the design spec.
- `docs/contributing.md` gains a "How to add a resilience subsystem"
  subsection.
- All five provider docs gain a "Resilience behavior" subsection
  documenting fallback eligibility per provider.

### Known limitations

- **Single-instance state.** Breaker and rate-limit state lives in
  process memory. Multi-instance deployments behind a load balancer
  lose strict enforcement until the breaker actually trips at the
  upstream. Distributed state (Redis backend) is a Phase 5 candidate.
- **Pre-stream fallback only.** Once `provider.complete()` returns
  `Ok(stream)` and bytes flow to the client, fallback is no longer
  possible. Mid-stream failures surface as stream-level errors. This
  matches v0.3 behavior; v0.4 does not introduce buffering.

### Frozen-core invariant resumes

ADR-0003 (Phase 3) was a bounded one-time exception. All five
Phase 4 plan files declared `core changes: NONE`, and
`git diff v0.3.0..master -- crates/core/` is empty for the v0.4.0
release tag.

## [0.3.0] — 2026-05-08

Phase 3 release: the OpenAI Responses frontend now routes to every
backend, vision Tier-1 covers the Responses row of the capability
matrix, and the first new core field since v0.1 lands as a typed
`Usage.safety_ratings` (per [ADR-0003](docs/adr/0003-promote-safety-ratings.md)).

### Added

#### OpenAI Responses frontend × all backends

- **Responses → Anthropic** via the canonical translation path
  (Plan 02). Hybrid passthrough still runs only when both inbound and
  outbound are Anthropic Messages; Responses-shaped requests go
  through `anthropic::request::build` / `anthropic::response::parse`.
- **Responses → Gemini** via the existing canonical Generate Content
  client (Plan 03). Gemini's `thoughts: true` reasoning parts surface
  as Responses `response.reasoning.delta` / `.done` events, and
  `safetyRatings` reach the `response.completed` payload.
- **Responses event-model audit** (Plan 01): golden SSE captures pin
  `response.output_item.{added,done}`,
  `response.content_part.{added,done}`,
  `response.output_text.{delta,done}`,
  `response.function_call_arguments.{delta,done}`,
  `response.reasoning.{delta,done}`, `response.completed`,
  `response.error` against real OpenAI traces.
- **Responses tool round-trip**: `function_call` /
  `function_call_output` items in `input` decode to canonical
  `ToolCallBlock` / `ToolResultBlock`, and outbound canonical tool
  calls re-emit as streaming `function_call` output items.
- **Responses reasoning round-trip**: inbound `reasoning` items in
  `input` decode to `ContentBlock::Reasoning`; outbound
  `StreamEvent::ReasoningDelta` surfaces as `response.reasoning.delta`
  + `response.reasoning.done` (previously dropped silently).

#### Vision (Tier-1) for Responses

End-to-end image support for the Responses row of the capability
matrix:

| Frontend → Provider | OAI-compat | Copilot | Anthropic | DeepSeek | Gemini |
|---|---|---|---|---|---|
| OpenAI Responses | ✅ | ✅ | ✅ (canonical) | ❌ gate | ✅ |

- The DeepSeek negative cell goes through the same capability gate as
  the existing Anthropic Messages and OpenAI Chat rows.
- Vision smoke tests cover all four active cells via mockito plus the
  capability-mismatch test using a panicking text-only stub provider.

#### `Usage.safety_ratings` — first new canonical field since v0.1

- New typed canonical field `Usage::safety_ratings: Option<Vec<SafetyRating>>`
  in `crates/core/src/safety.rs`, with open-enum
  `SafetyCategory::Other(String)` and `SafetyLevel::Other(String)`
  variants so unknown wire values round-trip losslessly.
- ADR-0003 records the promotion:
  [`docs/adr/0003-promote-safety-ratings.md`](docs/adr/0003-promote-safety-ratings.md).
  This is the v0.3 frozen-core exception that ADR-0002 reserved.
- Cross-protocol smoke tests (Responses → Anthropic and
  Responses → Gemini): text, tools, reasoning, safety, vision —
  15+ new tests under `crates/protocol-tests/tests/`.

#### Documentation

- ADR-0003 published.
- Per-provider docs gain a "Responses frontend" section.
- README capability matrix bumped to v0.3 with the Responses row
  fully populated.

### Changed

- Gemini provider now **double-writes** safety ratings: both the new
  typed `Usage.safety_ratings` field and the legacy
  `extensions["gemini.safety_ratings"]` key. The legacy write is
  removed in v0.3.1, marked at the call sites with
  `// REMOVE in v0.3.1` per ADR-0003.
- `EDITOR_VERSION` / `EDITOR_PLUGIN_VERSION` headers sent to GitHub
  Copilot bumped to `AgentShim/0.3.0`.

### Fixed

- **Anthropic streaming parser** now emits `ToolCallStop` for
  `tool_use` blocks (latent v0.2 bug surfaced during Plan 02 T2 —
  the parser previously emitted `ContentBlockStop` only, leaving
  the canonical event sequence missing the matching tool-call stop
  event for Responses-bound consumers).
- **OpenAI Responses decoder** now maps `input_image` content parts
  to `ContentBlock::Image` (latent bug surfaced during Plan 04 T4 —
  the decoder previously routed image parts to
  `ContentBlock::Unsupported`, silently dropping vision payloads
  before they reached any provider).
- **OpenAI Responses outbound encoder** now threads images through
  to the upstream wire shape (latent bug surfaced during Plan 04 T4 —
  the encoder previously called `extract_text` only, dropping image
  blocks on the way out).

### Deprecated

- `extensions["gemini.safety_ratings"]` — read from
  `Usage.safety_ratings` instead. The extension write is retained for
  the v0.3.0 transitional minor and removed in v0.3.1 per ADR-0003.

### Deferred (Phase 4+)

- Fallback chains, circuit breakers, per-route retries with backoff
  (Phase 4).
- Per-key rate limiting, per-agent API keys, request budget caps
  (Phase 4).
- Prometheus metrics, hot-reload config, OpenTelemetry (Phase 5).
- Audio / file content end-to-end (Phase 6+).
- Multi-account Copilot (Phase 6+).
- OAuth Anthropic / "Workbench" tokens (Phase 6+).

## [0.2.0] — 2026-05-07

Phase 2 release: provider breadth + vision Tier-1 + a token-count endpoint.
The canonical core is unchanged from v0.1 (frozen per
[ADR-0002](docs/adr/0002-frozen-core-v0.2.md)); all v0.2 work lands as new
providers, encoders, capability flags, and extension-map keys.

### Added

#### Native providers

- **Anthropic-as-backend** (`upstreams.<name>.type: anthropic`). Hybrid
  outbound path: byte-passthrough when the inbound frontend is also
  Anthropic Messages (round-trips `cache_control`, signature deltas on
  thinking blocks, and Anthropic-only metadata losslessly), canonical
  translation otherwise.
- **DeepSeek native** (`type: deepseek`). Adds:
  - `reasoning_content` interleaving for `deepseek-reasoner` (R1) — visible
    thinking blocks driven by the `ReasoningInterleaver` state machine.
  - `cache_hit_tokens` / `cache_miss_tokens` mapping into the canonical
    `Usage` shape.
  - Defense-in-depth `cache_control` strip on outbound bodies.
- **Gemini native** (`type: gemini`, AI Studio /
  generativelanguage.googleapis.com). Adds:
  - JSON-array streaming on `:streamGenerateContent` (NOT SSE) via a
    custom byte-level scanner that handles chunk boundaries at any byte
    position (mid-string, mid-escape, between objects). Exhaustively
    chunk-fuzzed.
  - `inlineData` (base64) and `fileData` (URL) image inputs.
  - `functionDeclarations` / `functionCall` / `functionResponse` for tools.
  - `thinkingConfig` (budget tokens) for reasoning-capable models.
  - `safetyRatings` / `citationMetadata` round-trip via the canonical
    extensions map (`gemini.safety_ratings`, `gemini.citation_metadata`).

#### Vision (Tier-1)

End-to-end image support across the (frontend × provider) matrix:

| Frontend → Provider | OAI-compat | Copilot | Anthropic | DeepSeek | Gemini |
|---|---|---|---|---|---|
| Anthropic Messages | ✅ | ✅ | ✅ (passthrough = lossless) | ❌ gate | ✅ |
| OpenAI Chat | ✅ | ✅ | ✅ (canonical) | ❌ gate | ✅ |
| OpenAI Responses | ✅ | ✅ | n/a | ❌ gate | n/a |

- **Capability gate** at the gateway boundary: when an inbound request
  contains an `Image` block but the routed provider's
  `capabilities.vision == false` (today: DeepSeek), the request is
  rejected with HTTP 400 and a frontend-shaped error envelope BEFORE any
  upstream call. See [`docs/architecture.md#capability-gate`](docs/architecture.md).
- `BinarySource::{Url, Base64, Bytes, ProviderFileId}` wired through every
  encoder. `Bytes` re-encodes to base64; `ProviderFileId` is dropped with
  a `tracing::warn!` when the target wire format doesn't model it.
- Vision smoke tests (`crates/providers/tests/vision_smoke.rs`,
  `crates/gateway/tests/vision_capability_mismatch.rs`) cover the active
  cells via mockito + a panicking stub provider.

#### `/v1/messages/count_tokens` endpoint

- New POST endpoint that mirrors Anthropic's `messages/count_tokens` API.
- Local token counting via `tiktoken-rs` — no upstream call required.
- Counts text, tool_use, tool_result, reasoning, redacted, image, and
  system instruction blocks; counts tool definitions and `tool_choice`.
- Per-message overhead applied per role.
- Smoke tests at `crates/gateway/tests/count_tokens_smoke.rs`.

#### Shared infrastructure

- `agent-shim-providers::oai_chat_wire` shared crate-internal lib:
  `canonical_to_chat::build`, `chat_sse_parser`, `chat_unary_parser`,
  `interleaved_reasoning`. Composed by the OpenAI-compatible, GitHub
  Copilot, and DeepSeek providers — no circular imports.
- `agent-shim-providers::http_client::build(read_timeout)` — shared TLS
  config, connect timeout, and read-gap timeout that every provider uses.
- `validate_oai_style_upstream` config helper, applied by all
  OpenAI-shaped upstreams.

#### Documentation

- Per-provider guides under `docs/providers/`: `anthropic.md`,
  `deepseek.md`, `gemini.md`.
- [ADR-0002](docs/adr/0002-frozen-core-v0.2.md) — the frozen-core policy
  that drove putting provider-specific data into namespaced
  `extensions` keys instead of new canonical variants.
- Refreshed `README.md`, `docs/architecture.md`, `docs/contributing.md`,
  `docs/configuration.md` for the v0.2 surface.
- `scripts/regen-fixtures.sh` for ongoing fixture maintenance.
- Opt-in nightly live-e2e GitHub Actions workflow
  (`.github/workflows/nightly-live.yaml`).

### Changed

- `EDITOR_VERSION` / `EDITOR_PLUGIN_VERSION` headers sent to GitHub
  Copilot bumped to `AgentShim/0.2.0`.
- `OpenAI Chat` decoder: `image_url` parts now decode into
  `ContentBlock::Image` with `BinarySource::{Url, Base64}` based on URL
  scheme (https / data: URI). Garbage URLs fall back to `Unsupported`.
- `Anthropic Messages` decoder: image source deserialized directly via
  `BinarySource`'s `serde` impl. `cache_control` preserved on the
  block's extensions map.
- GitHub Copilot provider declares `capabilities.vision = true` (it
  serves Claude and GPT-4o under the hood, both vision-capable).
- DeepSeek provider explicitly declares `capabilities.vision = false`
  so the gateway's capability gate rejects image requests with
  HTTP 400 before any upstream call.

### Deferred (Phase 3+)

- OpenAI `/v1/responses` frontend → Anthropic / Gemini backends
  (Phase 3). Today works for OAI-compat / Copilot / DeepSeek.
- Fallback chains, circuit breakers, retries with backoff (Phase 4).
- Rate limiting, per-agent API keys, request budget caps (Phase 4).
- Prometheus metrics, hot-reload config, OpenTelemetry (Phase 5).
- Audio / file content end-to-end.
- Multi-account Copilot.

## [0.1.1] — 2026-04-29

Fuzzy model discovery, polish.

## [0.1.0] — 2026-04-28

Initial MVP. Anthropic + OpenAI Chat frontends, OpenAI-compatible +
GitHub Copilot backends, tool calling, streaming, static config,
structured logging.
