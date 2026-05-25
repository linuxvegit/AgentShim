# Phase 7 P07 — Hot-Reload + Integration Tests + Benchmark Design

> **Status:** Design / spec. Promotes outline §P07 from
> [`2026-05-14-phase-7-p03-p07-outline.md`](../plans/2026-05-14-phase-7-p03-p07-outline.md)
> to a full design after P06b1 (`pii_scrubber`) and P06b2 (`prompt_compressor`)
> landed. This is the **final** sub-project of Phase 7 — when this merges,
> v0.7.0 ships.

**Source umbrella spec:** [`2026-05-14-plugin-system-design.md`](2026-05-14-plugin-system-design.md) §3.5 (hot reload), §6.8 (shutdown flush), §9 (testing matrix), §11 (risks).

---

## 1. Goal

Close out Phase 7 by:

1. Making `PluginRegistry` part of the hot-reloadable `AppSnapshot` (today
   it lives on the immutable `AppCore`).
2. Closing the integration-test coverage gaps that P04 deferred (H5 stream
   wrapping, `on_error: skip`, mid-stream error frame, in-flight reload
   isolation).
3. Replacing the placeholder benchmark file with a real, useful micro-bench
   that protects the **zero-overhead empty-registry** property.
4. Shipping v0.7.0: README + architecture.md + CHANGELOG entries; version
   bump on the workspace.

---

## 2. Non-goals

- **Re-doing tests P04/P05 already wrote.** §3 below is an exhaustive
  inventory of existing coverage; P07 only adds what is missing.
- **End-to-end "with-plugins vs without-plugins" benchmarking.** The
  outline's "<100 ns/request" target is unmeasurable in an HTTP-shaped
  e2e bench (measurement noise > actual delta). §7 documents the
  methodology change.
- **New plugin kinds.** All three v1 built-ins (`usage_recorder`,
  `pii_scrubber`, `prompt_compressor`) shipped in earlier sub-projects.
- **Modifying `crates/core/`.** Frozen-core invariant (§10).

---

## 3. Existing coverage inventory

Before listing what to add, this is what is *already* in tree at master
HEAD (commit `1f2bfdf`):

### 3.1 Pipeline integration (`crates/gateway/tests/plugins_pipeline.rs`)

| § from umbrella spec | Test name | Covers |
|---|---|---|
| §9.T2 H2 mutates prompt | `h2_plugin_fires_and_modifies_prompt` | ✓ |
| §9.T3 H3 sees BackendTarget | `h3_plugin_fires_with_backend_target` | ✓ |
| §9.T4 H7 sees usage summary | `h7_plugin_fires_after_unary_response` | ✓ |
| §9.T6 `Aborted` → 400 | `plugin_aborted_returns_400_anthropic_envelope` | ✓ |
| §9.T5 `Failed` + `fail` → 502 | `plugin_failed_returns_502_when_on_error_fail` | ✓ |
| §9.T11 protected-field 502 | `plugin_protected_field_mutation_returns_502` | ✓ |
| Zero-overhead empty registry | `empty_registry_request_returns_200` | ✓ |

### 3.2 Observability (`crates/gateway/tests/plugins_observability.rs`)

| § | Test name | Covers |
|---|---|---|
| §9.T10 H7 shutdown drop | `slow_h7_plugin_dropped_at_shutdown_increments_dropped_counter` | ✓ |
| §9 ancillary Prometheus | `h2_plugin_invocation_emits_prometheus_counter` | ✓ |
| §9 ancillary empty-registry no-noise | `empty_registry_no_nonzero_plugin_observation` | ✓ |

### 3.3 Per-kind end-to-end (mockito)

- `pii_scrubber_integration.rs` — H2 inbound scrub + H5 outbound scrub
- `prompt_compressor_integration.rs` — summarize-path with second upstream
- `usage_recorder_integration.rs` — Prometheus + log sink emission

### 3.4 Coverage gaps P07 must close

| § | Scenario | Current | P07 action |
|---|---|---|---|
| §9.T5 H5 drops events | none | new test `h5_drops_alternate_stream_events` |
| §9.T9 H5 mid-stream error → SSE error frame | none | new test `h5_failure_emits_sse_error_frame_and_closes` |
| §9.T8 `on_error: skip` swallows error | none | new test `plugin_skip_on_error_returns_200` |
| §9.T12 reload swap, in-flight isolation | none | new test `reload_swap_isolates_in_flight_requests` |
| Plugin section reload error path | none | new test `reload_with_bad_plugin_config_rejected` |
| Plugin section reload happy path | none | new test `reload_swaps_plugins_atomically` |

---

## 4. Hot-reload architecture (the headline change)

### 4.1 Today (P04 placement)

`plugins: Arc<PluginRegistry>` is a field of `AppCore` — *immutable across
reload*. Built once in `AppState::build` and never replaced.

```rust
pub struct AppCore {
    pub plugins: Arc<agent_shim_plugins::PluginRegistry>,
    // ... immutable handles
}
```

This was the "placeholder" placement from P04 — explicitly documented as
"hot-swapping lands in P07".

### 4.2 P07: move to `AppSnapshot`

```rust
pub struct AppSnapshot {
    pub config: Arc<agent_shim_config::GatewayConfig>,
    pub auth_enabled: bool,
    pub auth_required: bool,
    pub configured_key_hashes: Arc<HashSet<String>>,
    /// Plan 07 P07: plugins now hot-swappable. In-flight requests
    /// capture the snapshot at the top of `pipeline::dispatch`; subsequent
    /// reloads do not perturb their plugin view.
    pub plugins: Arc<agent_shim_plugins::PluginRegistry>,
}
```

`AppCore.plugins` is **removed entirely**. All 7 pipeline call-sites
(`pipeline.rs` lines 580, 605, 838, 854, 916, 1054, 1078 at this commit)
change from `state.core.plugins.*` to `snapshot.plugins.*`. `snapshot` is
already captured once at the top of `dispatch` (line 256), so all 7 sites
are guaranteed to see the same registry for a given request.

### 4.3 Reload commit semantics

`handle_reload` in `crates/gateway/src/commands/serve.rs` adds plugin
rebuild **before** the snapshot store, all-or-nothing:

```
parse YAML ─┬─ validate_for_reload (Layer A: config schema, immutable fields)
            │   └─ failure → ReloadOutcome::{Parse, ImmutableField, ValidationError}
            │
            ├─ NEW: PluginRegistry::build (Layer B: kind lookup, factory parse,
            │       hook-subscription match)
            │   └─ failure → ReloadOutcome::PluginValidation(String)
            │
            ├─ snapshot.store(new_snap) ◄─── commit point 1 (plugins included)
            └─ limiter.store(new_limiter)  ◄─ commit point 2 (unchanged from P02)
```

**Failure semantics:**
- Any failure before the snapshot.store → entire reload rejected, old
  config / old plugins / old limiter all keep running.
- The new `ReloadOutcome::PluginValidation(String)` variant carries the
  formatted `RegistryBuildError` from `agent_shim_plugins`.
- Maps to **HTTP 400** in `admin/reload_handler.rs` (config-shape problem,
  caller's fault) with body `{"ok": false, "errors": ["<formatted>"]}`.
- Metric: existing `agent_shim_config_reloads_total{result=...}` counter
  gains a new label value `"plugin_validation_error"`.

**Note on the two-step commit:** the existing window between
`snapshot.store` and `limiter.store` (where a request can see "new
snapshot + old limiter") is **unchanged**. The plugin registry is bundled
into the snapshot, so the snapshot.store is now a *single atomic flip*
covering config + auth + plugins. The follow-on limiter store retains the
P02 documented behavior verbatim.

### 4.4 In-flight isolation

The arc-swap discipline already in place gives this for free:

1. Request R1 enters `pipeline::dispatch` → `snapshot.load_full()` at line
   256 → R1 binds an `Arc<AppSnapshot>` to its local `snapshot`.
2. Reload happens → `snapshot.store(new_snap)` replaces the `ArcSwap`'s
   pointer.
3. R1 still owns its captured `Arc<AppSnapshot>` (refcount stays > 0). All
   7 plugin reads on its path resolve against the *old* registry. R1
   continues to completion with consistent plugin behavior.
4. Subsequent request R2 enters `dispatch` → loads the *new* `Arc`.

This is the property §6.T12 must protect with a regression test.

---

## 5. Test plan additions

### 5.1 H5 stream wrapping (`crates/gateway/tests/plugins_h5_stream.rs`, new file)

**Stack:** uses `eventsource-client` (dev-dependency) to parse the SSE
response body into a sequence of `(event, data)` pairs. No manual SSE
parsing.

```toml
# crates/gateway/Cargo.toml
[dev-dependencies]
eventsource-client = "0.13"  # or whatever the current stable is
```

Two tests:

**`h5_drops_alternate_stream_events`** — Plugin's `on_stream_event` returns
`Skip` for every odd-indexed event; client should see only the
even-indexed events. Asserts:
- 200 status code (success path).
- Final SSE stream contains a strict subset of the original events.
- Dropped events are absent (no `text_delta` for skipped indices).

**`h5_failure_emits_sse_error_frame_and_closes`** — Plugin's
`on_stream_event` returns `Err(PluginError { aborted: false })` after the
3rd event. `on_error: fail`. Asserts:
- 200 status code (headers were already flushed by then).
- SSE stream contains at least the first 3 successful events.
- Stream terminates with an Anthropic-style `event: error` frame whose
  data payload contains a `"plugin_failed"` error type.
- Connection closes after the error frame.

### 5.2 `on_error: skip` (`crates/gateway/tests/plugins_pipeline.rs`, append)

**`plugin_skip_on_error_returns_200`** — H2 plugin returns
`Err(PluginError { aborted: false })`. Plugin config has `on_error: skip`.
Asserts:
- 200 status code (error swallowed).
- Upstream receives the *original* (un-mutated) canonical request.
- Metric `agent_shim_plugin_invocations_total{outcome="skipped"}` incremented.

### 5.3 Hot-reload tests (`crates/gateway/tests/plugins_reload.rs`, new file)

**`reload_swaps_plugins_atomically`** — Start gateway with plugin config A
(injects "BUILD-A" into prompt). Send a request — upstream sees "BUILD-A".
Trigger reload with plugin config B (injects "BUILD-B"). Send another
request — upstream sees "BUILD-B". The new request must observe the new
plugin behavior.

**`reload_with_bad_plugin_config_rejected`** — Start gateway with valid
plugin config. Trigger reload with config that references an unknown
plugin kind. Assert:
- `handle_reload` returns `ReloadOutcome::PluginValidation(...)`.
- HTTP 400 from `/admin/reload` with `{"ok": false, "errors": [...]}`.
- `agent_shim_config_reloads_total{result="plugin_validation_error"}`
  incremented exactly once.
- A subsequent request still observes the *old* plugin behavior (no
  partial commit).

**`reload_swap_isolates_in_flight_requests`** — The deterministic
in-flight isolation test.
1. Plugin A's `on_decoded_request` injects `MARKER=ORIGINAL` and then
   awaits an `Arc<Barrier>` of size 2 (plugin + main thread).
2. Spawn request R1 → it enters dispatch, takes snapshot A, plugin A is
   called, hits the barrier, waits.
3. Main thread:
   - Triggers reload to plugin B (`MARKER=RELOADED`); awaits reload OK.
   - Sends a second blocking request R2; assert R2 saw `MARKER=RELOADED`.
   - Releases the barrier.
4. R1 resumes, plugin A completes, request finishes; assert R1's upstream
   body contained `MARKER=ORIGINAL`.

Synchronization is done with `tokio::sync::Barrier`, not with `sleep`. The
test is deterministic.

---

## 6. Frozen-core invariant

`git diff master..HEAD -- crates/core/` must remain empty across all P07
commits. The hot-reload work is entirely in `crates/gateway/`. Plugin
registry rebuild logic uses existing `agent_shim_plugins` API surface; no
trait additions to core types are needed.

---

## 7. Benchmark methodology

### 7.1 What we are NOT measuring

The outline asked for "<100 ns/request difference between empty-registry
and no-PluginRegistry". This is unmeasurable in practice:
- An e2e bench through Axum + serde + provider mocks has noise floors in
  the *hundreds of nanoseconds* per iteration even on a quiet machine.
- "No PluginRegistry" would require a git revert of P04's pipeline edits
  — i.e. maintaining a parallel pipeline definition forever. Cost not
  justified.

### 7.2 What we ARE measuring

A criterion micro-benchmark at the registry-call layer:

```rust
// crates/plugins/benches/empty_registry_overhead.rs (new file)
fn bench_empty_registry_h2(c: &mut Criterion) {
    let registry = PluginRegistry::empty();
    let ctx = test_ctx();
    let req = synthetic_canonical_request();
    let rt = tokio::runtime::Runtime::new().unwrap();
    c.bench_function("empty_registry::run_on_decoded_request", |b| {
        b.iter(|| {
            rt.block_on(async {
                let r = req.clone();
                registry.run_on_decoded_request(black_box(("anthropic", "test-model")),
                                                black_box(&ctx), r).await
            })
        });
    });
}
```

**Target:** < 1 µs / call on a 2024-era developer laptop. Documented in
`bench_results.md` checked in alongside.

### 7.3 Code-level evidence

Spec / CHANGELOG include the explicit zero-overhead claim with the
relevant lines from `crates/plugins/src/registry.rs`:

> Empty registries early-return via `if self.subscriptions(...).is_empty()
> { return Ok(req); }` in `run_on_decoded_request`, `run_on_resolved`,
> `wrap_stream`, and `run_on_response_complete`. The 7-line dispatch in
> `pipeline.rs` therefore degenerates to 7 cheap dispatches each running
> the early-return path when no plugins are configured.

This claim is verified by the existing
`empty_registry_request_returns_200` test (parity) plus the new
micro-bench (latency floor).

---

## 8. Release surface

P07 ships v0.7.0. The final commit batch includes:

| File | Action |
|---|---|
| `Cargo.toml` (workspace root) | `version = "0.7.0"` |
| `CHANGELOG.md` | full v0.7.0 section: P01 tokens, P02 trait/registry, P03 config, P04 pipeline, P05 observability, P06a registry builder, P06b1 pii_scrubber, P06b2 prompt_compressor, **P07 hot-reload + tests + bench** |
| `README.md` | new top-level "Plugins" section with config example, supported hooks, supported built-ins, link to per-plugin docs (if any) |
| `docs/architecture.md` | new "Phase 7: Plugin system" section explaining the H2/H3/H5/H7 hooks, `AppSnapshot` hot-reload integration, and the zero-overhead design |
| `docs/superpowers/plans/2026-05-14-phase-7-p03-p07-outline.md` | mark §P07 as "✅ Promoted to full plan: [...]" once the plan file lands |

**Not** included in P07 (handled by user after merge):

- `git tag v0.7.0`
- `cargo publish`-style release actions
- crates.io publication (if/when that pathway opens up)

---

## 9. Task decomposition outline

The full plan will break this into ~13 tasks. Sketch:

| # | Task | Notes |
|---|---|---|
| T1 | Move `plugins` field from `AppCore` to `AppSnapshot`; update all 7 pipeline call-sites and `AppState::new_with_plugins` test helper | atomic refactor; tests still pass |
| T2 | Extend `ReloadOutcome` with `PluginValidation(String)` + admin handler 400 branch + metric label | adds new failure path |
| T3 | Wire `PluginRegistry::build` into `handle_reload` before the snapshot store | core hot-reload change |
| T4 | Add `eventsource-client` as a dev-dependency of `crates/gateway`; add `h5_drops_alternate_stream_events` | first H5 test |
| T5 | Add `h5_failure_emits_sse_error_frame_and_closes` | second H5 test |
| T6 | Add `plugin_skip_on_error_returns_200` | on_error: skip |
| T7 | Add `reload_swaps_plugins_atomically` | hot-reload happy path |
| T8 | Add `reload_with_bad_plugin_config_rejected` | hot-reload failure path |
| T9 | Add `reload_swap_isolates_in_flight_requests` (barrier-synchronized) | in-flight isolation |
| T10 | Add `crates/plugins/benches/empty_registry_overhead.rs` criterion bench + check-in `bench_results.md` snapshot | micro-bench |
| T11 | Write `docs/architecture.md` Phase 7 section + top-level `README.md` Plugins section | docs |
| T12 | Bump workspace version to `0.7.0`; write `CHANGELOG.md` v0.7.0 section | release prep |
| T13 | `cargo fmt --all` + `cargo clippy --workspace --all-targets -- -D warnings` + frozen-core check + workspace nextest run | acceptance gate |

---

## 10. Acceptance criteria

After T13, the following must all hold:

1. `cargo nextest run --workspace` shows ≥ 920 passing tests (baseline 912 + ≥ 6 new + ≥ 2 reload tests + bench compile check).
2. `cargo fmt --all -- --check` is clean.
3. `cargo clippy --workspace --all-targets -- -D warnings` is clean.
4. `git diff master..HEAD -- crates/core/` is empty.
5. `cargo bench --bench empty_registry_overhead -p agent-shim-plugins` runs; results recorded.
6. `AppCore` no longer has a `plugins` field; `AppSnapshot` does.
7. `ReloadOutcome::PluginValidation` exists; `/admin/reload` returns 400 on plugin validation failure.
8. `Cargo.toml` workspace `version = "0.7.0"`.
9. `CHANGELOG.md` v0.7.0 section is present and accurate.
10. `README.md` has a Plugins section.
11. `docs/architecture.md` has a Phase 7 section.
12. P07 outline in `2026-05-14-phase-7-p03-p07-outline.md` is marked as
    promoted with a link to the new plan.

When all 12 are satisfied, the branch is ready for `git merge --no-ff
worktree-phase-7-p07-hot-reload -m "Merge Phase 7 P07: hot-reload +
integration tests + bench → v0.7.0"` into master.

---

## 11. Open risks / mitigations

| Risk | Mitigation |
|---|---|
| `eventsource-client` dev-dep adds non-trivial compile time | dev-only; production binary unaffected; license audit clean (Apache-2.0 / MIT) |
| Barrier-sync test causes deadlock on assertion failure | `tokio::time::timeout(Duration::from_secs(5), ...)` wraps the barrier wait; assertion failure inside the plugin is captured via Arc<Mutex<Result>> shared with the main thread |
| Bench result drift on CI | `bench_results.md` is *not* checked into CI gates — it is a record snapshot; CI only verifies the bench compiles, not the timing |
| Documentation rot if plugin behavior changes after v0.7.0 | accepted; future phases handle their own doc updates per existing convention |
