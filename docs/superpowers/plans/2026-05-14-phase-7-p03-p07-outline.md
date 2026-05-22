# Phase 7 — Plans P03 through P07 outline

> **Status:** P01 and P02 are fully detailed in their own plan files (sibling files in this directory). P03-P07 are outlined here at task-list granularity. Each will be promoted to its own full plan file when its predecessor lands and we've learned what worked.
>
> Rationale: writing 7 plans at full granularity up front risks high churn — every plan after P02 depends on details that will only crystallise once the trait surface is exercised in real integration. Full-detail planning happens just-in-time.

**Source design:** [`docs/superpowers/specs/2026-05-14-plugin-system-design.md`](../specs/2026-05-14-plugin-system-design.md) §12.

---

## Promotion rule

When the predecessor plan is merged and CI is green, promote the next outline to its own full plan file using P01/P02 as templates. Promote one plan at a time. The full plan file replaces the matching section in this overview.

Filename convention: `docs/superpowers/plans/2026-05-XX-phase-7-pNN-<short-slug>.md`.

---

## P03 — Config integration (Layer A validation)

**Status:** ✅ Promoted to full plan: [`2026-05-14-phase-7-p03-config-integration.md`](2026-05-14-phase-7-p03-config-integration.md)

(Outline below preserved for context but superseded by the full plan.)

**Source spec:** §5.1, §5.2, §5.3 Layer A, §5.4, §5.5, §5.6.

**Goal:** Add `plugins:` top-level and `routes[].plugins:` per-route blocks to `agent-shim-config`. Implement Layer A validation (undeclared plugin reference / `timeout_ms == 0` / duplicate plugin names). Env-var overlay verified.

**Architecture:** New module `crates/config/src/plugins.rs` housing `PluginEntry`, `RoutePluginsBlock`, `TimeoutMs` enum (untagged `u64 | map`). `GatewayConfig.plugins: BTreeMap<String, PluginEntry>` and `RouteEntry.plugins: Option<RoutePluginsBlock>` are the new fields. Validation extends `crate::validation::validate()` with the three rules.

**Frozen-core impact:** None.

**Task outline:**

1. Add `PluginEntry` struct (kind, config opaque `serde_json::Value`, on_error, timeout_ms, enabled) with `deny_unknown_fields`.
2. Add `TimeoutMs` untagged enum (`Single(u64)` | `PerHook { default, on_decoded_request, on_resolved, on_stream_event, on_response_complete }`).
3. Add `RoutePluginsBlock` struct (four ordered `Vec<String>` per hook) with `deny_unknown_fields`.
4. Wire fields into `GatewayConfig` (`plugins:`) and `RouteEntry` (`plugins:`).
5. Implement `validate_plugins(cfg) -> Result<(), ValidationError>` covering Layer A's three rules. Hook into `validate()`.
6. YAML round-trip tests: minimal config, full config from spec §5.2.
7. Env-var overlay tests for `AGENT_SHIM__PLUGINS__<NAME>__ENABLED/ON_ERROR/CONFIG__<FIELD>`.
8. Workspace check + clippy + fmt + commit per task.

**Tasks count estimate:** ~8 tasks. Plan length comparable to P02.

---

## P04 — Pipeline integration

**Status:** ✅ Promoted to full plan: [`2026-05-14-phase-7-p04-pipeline-integration.md`](2026-05-14-phase-7-p04-pipeline-integration.md)

(Outline below preserved for context but superseded by the full plan.)

**Source spec:** §6.2, §6.4, §6.5, §6.6, §6.7, §6.9.

**Goal:** Wire `PluginRegistry::run_on_decoded_request` / `run_on_resolved` / `wrap_stream` / `run_on_response_complete` into `pipeline.rs::dispatch_inner` at the four documented anchor points. Add universal `H7Guard` covering all three streaming frontends. Add `HandlerError::PluginFailed` and its 400/502 mapping.

**Architecture:** New methods on `PluginRegistry` (the `run_*` and `wrap_stream`) implement the chain walk with clone-then-swap (§6.4), the H5 stream wrapper (§6.5), and the H7 spawn (§6.8 — but JoinSet flush lands in P07). Pipeline modifications are at four code-anchor points, no line numbers. `HandlerError::PluginFailed { kind, plugin, hook, aborted }` added; `handler_error_status_hint` extended.

**Frozen-core impact:** None.

**Task outline:**

1. Implement `PluginRegistry::run_on_decoded_request` using `invoke()` template + clone-then-swap + protected-field check.
2. Implement `PluginRegistry::run_on_resolved` (same shape, plus `BackendTarget` arg).
3. Implement `PluginRegistry::wrap_stream` (1-in-N-out, fast path for empty subscription list, fail-mid-stream → SSE error frame).
4. Implement `PluginRegistry::run_on_response_complete` as `tokio::spawn` (JoinSet wiring + flush in P07; for P04 a bare spawn suffices).
5. Unit test the four methods individually with mock plugins.
6. Add `HandlerError::PluginFailed`; extend `from_resilience_error` and `handler_error_status_hint` (400 for `aborted: true`, 502 otherwise).
7. Extend frontend error envelope rendering to handle `PluginFailed` (one branch each in Anthropic / OpenAI Chat / OpenAI Responses).
8. Replace `StreamLogger` with a generalised `H7Guard` (still `Drop`-based) covering all three streaming frontends. The guard captures `Arc<PluginRegistry>` + route + ctx; its Drop calls `registry.run_on_response_complete(...)`.
9. Wire the four call points in `pipeline.rs::dispatch_inner` per spec §6.6's anchor table.
10. Integration tests with `PluginRegistry::empty()` — pipeline behaviour must be byte-identical to pre-plugin baseline (no overhead path).
11. Mockito-based integration test with a stub plugin verifying the four hooks fire in order.
12. Workspace check + clippy + fmt + commit per task.

**Tasks count estimate:** ~12 tasks. Largest single plan in this phase.

---

## P05 — Observability

**Source spec:** §7 (entire).

**Goal:** Add structured logs (§7.1, §7.2), OTel `plugin.invoke` child span (§7.3), Prometheus counter + histogram (§7.4), stream-hook noise control (§7.5), shutdown-drop counter (§7.4 + §6.8 JoinSet wiring).

**Architecture:** Logging emits `tracing` events with the §7.1 field set. Spans use the existing `tracing::info_span!("plugin.invoke", ...)` pattern from v0.5. Metrics use `metrics-rs` macros with the agreed label set; histograms use the default `metrics-exporter-prometheus` 12-bucket distribution. Stream-hook noise control = skip per-event log on `success` outcome.

**Frozen-core impact:** None.

**Task outline:**

1. Extend `invoke()` template with span + log emission (success → DEBUG, skipped → WARN, failed → ERROR, aborted → INFO, protected_field_mutated → ERROR, timed_out → WARN/ERROR per on_error). Add rustdoc on the log-field helper explicitly forbidding request content (§7.6 PII red lines).
2. Add Prometheus registration of `agent_shim_plugin_invocations_total` and `agent_shim_plugin_duration_seconds`. Use the existing observability crate's metric-registration helpers.
3. Add `agent_shim_plugin_h7_dropped_at_shutdown_total{plugin_name}` counter (zero-increment registration at registry boot).
4. Stream-hook noise control: in `wrap_stream`'s per-event invoke, suppress structured-log emission on success; metrics still update.
5. Wire `tokio::task::JoinSet` for H7 spawn tracking (delayed from P04). Implement `flush_pending_h7(deadline) -> drop_count`.
6. Hook flush into gateway shutdown (`crates/gateway/src/shutdown.rs`).
7. Unit tests: log field-set assertions via `tracing_subscriber::fmt::TestWriter`; metric counter assertions via `metrics-util::debugging::DebuggingRecorder`.
8. Workspace check + clippy + fmt + commit per task.

**Tasks count estimate:** ~8 tasks.

---

## P06 — Layer B validation + built-in plugin kinds

**Source spec:** §5.3 Layer B, §8.1, §8.2, §8.3.

**Goal:** Implement Layer B validation (unknown kind / config deserialisation / hook-subscription mismatch — `RegistryBuildError` variants from P02). Ship the three v1 built-in plugin kinds (`prompt_compressor`, `pii_scrubber`, `usage_recorder`) each behind its own Cargo feature.

**Architecture:** `PluginRegistry::build(specs: &[PluginSpec], factories: &[Arc<dyn PluginFactory>]) -> Result<Self, RegistryBuildError>` — the actual constructor that ties P02's pieces together with P03's config types. `register_builtin_plugins(factories: &mut Vec<Arc<dyn PluginFactory>>)` adds the three kinds. Each kind: ~50-100 lines, own file + tests.

**Frozen-core impact:** None.

**Task outline:**

1. Implement `PluginRegistry::build` constructor.
2. Implement `prompt_compressor` (H2 only): three strategies (`summarize_old_turns`, `drop_old_turns`, `truncate_to_tokens`) + factory + config struct + unit tests with synthetic CanonicalRequest fixtures. Uses `agent_shim_tokens::count_text`.
3. Implement `pii_scrubber` (H2 only, per Q9): pattern set (email / phone / ssn / credit_card) + custom-regex support + factory + tests. Default `on_error: fail`.
4. Implement `usage_recorder` (H7 only): two sinks (`prometheus` registers per-plugin custom counter; `log` emits an INFO line) + factory + tests.
5. Add Cargo feature flags for the three kinds in `crates/plugins/Cargo.toml`.
6. Implement `register_builtin_plugins` in `builtin/mod.rs` behind `#[cfg(feature = "...")]` blocks.
7. Wire `register_builtin_plugins` into `gateway::state::AppCore::build` after factories list is constructed.
8. Workspace check + clippy + fmt + commit per task.

**Tasks count estimate:** ~10 tasks.

---

## P07 — Hot-reload + integration tests + benchmark

**Status:** ✅ Promoted to full plan: [`2026-05-22-phase-7-p07-hot-reload-and-tests.md`](2026-05-22-phase-7-p07-hot-reload-and-tests.md) (full spec at [`../specs/2026-05-22-phase-7-p07-hot-reload-and-tests-design.md`](../specs/2026-05-22-phase-7-p07-hot-reload-and-tests-design.md)).

(Outline below preserved for context but superseded by the full plan.)

**Source spec:** §3.5 (hot reload), §6.8 (shutdown flush — completes the JoinSet wiring), §9 (testing strategy), §11 risks.

**Goal:** Make `PluginRegistry` part of the `AppSnapshot` arc-swap path. Ship `tests/plugins_integration.rs` covering the end-to-end spec §9 matrix. Add a benchmark proving the no-plugin fast path matches the v0.6 baseline.

**Architecture:** `AppSnapshot` gains `plugins: Arc<PluginRegistry>`. Reload path in `commands::serve::handle_reload` rebuilds the registry from the new config and atomically swaps via the existing arc-swap mechanism. New integration test file exercises the 12 scenarios from spec §9. New `crates/gateway/benches/no_plugins_overhead.rs` (criterion) compares with-empty-registry vs without-registry.

**Frozen-core impact:** None.

**Task outline:**

1. Add `plugins: Arc<PluginRegistry>` to `AppSnapshot`. Default = `PluginRegistry::empty()`.
2. Threading through reload: in `handle_reload`, after the new config is validated (Layer A) and the resilience structures are rebuilt, also rebuild the plugin registry (Layer B). On any error, the entire reload is rejected (existing arc-swap commit-or-rollback semantics).
3. Integration test: H2 plugin fires, modifies prompt → upstream sees modified prompt.
4. Integration test: H3 plugin fires per `BackendTarget`.
5. Integration test: H5 plugin drops every other event.
6. Integration test: H7 plugin sees usage summary.
7. Integration test: `on_error: skip` swallows panic-free Err; `on_error: fail` 502s.
8. Integration test: `Aborted` returns 400 with inbound-frontend envelope.
9. Integration test: H5 error mid-stream emits SSE error frame + closes.
10. Integration test: shutdown flush awaits in-flight H7 spawns within deadline.
11. Integration test: protected-field mutation returns 502 with `ProtectedFieldMutated`.
12. Integration test: reload swap is observed by new requests; in-flight requests retain prior registry.
13. Criterion benchmark: empty registry overhead vs `PluginRegistry` totally absent. Must show < 100 ns difference per request (zero-overhead claim).
14. Update top-level README + docs/architecture.md with a Phase 7 section.
15. CHANGELOG entry for v0.7.0.
16. Workspace check + clippy + fmt + commit per task.

**Tasks count estimate:** ~16 tasks. Largest plan in this phase by task count, but each task is small (single integration test or single doc edit).

---

## Phase exit criteria

All seven plans landed when:

- `cargo test --workspace` passes including the new integration suite
- `cargo clippy --workspace --all-targets -- -D warnings` passes
- `cargo fmt --all -- --check` passes
- `cargo bench --bench no_plugins_overhead -p agent-shim` shows zero-overhead claim met
- README, docs/architecture.md, and CHANGELOG reflect Phase 7
- `crates/core/` diff against the v0.6.1 baseline is empty (frozen-core invariant respected)
- No new external workspace dependencies introduced beyond what `regex` brings (used only by `pii_scrubber`, feature-gated)
