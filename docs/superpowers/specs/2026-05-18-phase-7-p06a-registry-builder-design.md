# Phase 7 P06a — PluginRegistry::build + usage_recorder built-in (design)

> **Status:** Design spec for P06a. Implementation plan will be promoted from this document via `superpowers:writing-plans`.
>
> **Source spec:** [`2026-05-14-plugin-system-design.md`](2026-05-14-plugin-system-design.md) §5.3 (Layer B), §6.1 (Registry internals), §8.3 (usage_recorder).

## 1. Goal

Wire the **production** `PluginRegistry::build(&GatewayConfig, factories)` constructor that turns YAML + factory list into a populated registry, plus the first built-in plugin (`usage_recorder` with the `log` sink) to validate the end-to-end YAML → AppCore → live request → H7 hook fires path.

P06a deliberately ships **only one** built-in (the simplest). The other two built-ins (`prompt_compressor`, `pii_scrubber`) plus the `prometheus` sink for `usage_recorder` are deferred to P06b — each introduces its own design questions (token caching, regex library choice, runtime-registered Prometheus metrics) that warrant separate brainstorming.

## 2. Scope

**In scope:**

1. `PluginRegistry::build(cfg: &GatewayConfig, factories: Vec<Arc<dyn PluginFactory>>) -> Result<Self, RegistryBuildError>` — the real production constructor. Two-phase fail-fast.
2. `builtin_plugins() -> Vec<Arc<dyn PluginFactory>>` in `agent_shim_plugins::builtin`, feature-gated.
3. `usage_recorder` built-in: H7-only, `sink: log` only, configurable level (info/debug/warn), field-name whitelist (validation-only — runtime always emits all 6 fields).
4. `AppState::new` returns `Result<(Self, Receiver), anyhow::Error>` so registry build failure surfaces as gateway startup failure.
5. Cargo feature flag `usage_recorder` (default-on) in `crates/plugins/Cargo.toml`.
6. One gateway integration test driving the full path.

**Out of scope (deferred to P06b or later):**

- `prompt_compressor` plugin (needs token caching, 3 strategies, `agent_shim_tokens::count_text` integration)
- `pii_scrubber` plugin (needs `regex` crate, 4 built-in patterns, custom regex support)
- `usage_recorder` with `sink: prometheus` (needs runtime-registered counters with operator-defined labels)
- Plugin hot-reload (P07)
- Factory storage on `PluginRegistry` (factories are dropped after `build` — YAGNI)
- Per-route disabled-plugin opt-out (YAGNI; disabled flag at top-level is enough)

## 3. Architecture

### 3.1 Crate boundary change

`agent-shim-plugins` adds a new dependency on `agent-shim-config`. The plugins crate already depends on `agent-shim-core`, `agent-shim-observability` (P05), and `agent-shim-tokens` (P01), so adding `agent-shim-config` is the fourth upstream dependency.

The reverse dependency is verified safe: `agent-shim-config` does **not** import any `agent-shim-plugins` types — `config` defines `OnErrorYaml` / `TimeoutMs` / `PluginEntry` (in `crates/config/src/plugins.rs`) as its own mirror types specifically to avoid the cycle. P06a converts those YAML mirrors into the runtime types (`OnError`, `HookTimeouts`) inside `PluginRegistry::build`.

### 3.2 Frozen-core invariant

`crates/core/` is **not modified**. P06a uses only existing core types (`FrontendKind`, `RequestId`, `Usage`). `ResponseSummary` is **not** extended with a `model` field — `PluginContext.route_label` already carries `<frontend>/<model>` so no information is lost.

### 3.3 Component diagram

```
GatewayConfig (config crate)
   │
   │ &cfg
   ▼
PluginRegistry::build(cfg, factories) ─── Vec<Arc<dyn PluginFactory>>
   │                                          ▲
   │                                          │ builtin_plugins()
   │                                          │ (feature-gated)
   │                                          │
   ▼                                       UsageRecorderFactory
PluginRegistry { plugins, plans, supervisor }
   │
   │ stored in AppCore.plugins: Arc<PluginRegistry>
   ▼
pipeline.rs::dispatch_inner
   ├── run_on_decoded_request   ─── H2 plugins (P04 wiring)
   ├── run_on_resolved          ─── H3 plugins (P04 wiring)
   ├── wrap_stream              ─── H5 plugins (P04 wiring)
   └── H7Guard.drop → run_on_response_complete ─── H7 plugins (P04 wiring)
                                                     ▲
                                              UsageRecorder fires here
```

## 4. `PluginRegistry::build` design

### 4.1 Signature

```rust
impl PluginRegistry {
    pub fn build(
        cfg: &agent_shim_config::GatewayConfig,
        factories: Vec<Arc<dyn PluginFactory>>,
    ) -> Result<Self, RegistryBuildError>;
}
```

- Sync function — `instantiate` is sync per `PluginFactory` trait.
- `factories` is consumed (not borrowed) — caller no longer needs the Vec after build.
- Returns `Self` on success (no `Arc` wrap — caller wraps).

### 4.2 Algorithm

**Phase 1 — Factory index**: Build `HashMap<&'static str, Arc<dyn PluginFactory>>` by `kind_name()`. Duplicate kind names would silently overwrite — but factories are compile-time-registered (built-in + future user-supplied), so duplicate kinds are a programmer error caught in code review, not a runtime concern. (If P06b ever exposes user-loaded factories, we add a duplicate check then.)

**Phase 2 — Instantiate every plugin** (in `cfg.plugins` BTreeMap key order, for predictable error attribution):

For each `(name, entry_cfg)` in `cfg.plugins`:

1. **Layer B rule 4** (unknown kind): lookup `factory_index.get(entry_cfg.kind.as_str())` — if `None`, return `Err(RegistryBuildError::UnknownKind { plugin: name, kind: entry_cfg.kind, known: <sorted list of factory_index keys> })`.
2. **Layer B rule 5** (config deserialisation): call `factory.instantiate(&name, entry_cfg.config.clone())`. On `Err(PluginConfigError)`, wrap as `RegistryBuildError::Instantiation(err, name.clone())`.
3. **Convert YAML mirror types to plugins-crate types**:
   - `entry_cfg.on_error: OnErrorYaml` → `OnError` (1:1 match)
   - `entry_cfg.timeout_ms: Option<TimeoutMs>` → `HookTimeouts`. `None` = `HookTimeouts::default()`. `Some(Uniform(n))` = uniform across all hooks. `Some(PerHook { default, per_hook... })` = per-hook overrides, missing fields fall back to `default` then `HookTimeouts::default()`.
4. **Cache `kind` as `&'static str`**: `factory.kind_name()` is the source of truth (rustdoc on `Plugin::kind_name` already mandates string-literal return; we use `factory.kind_name()` rather than the constructed `plugin.kind_name()` so the cached value comes from a single canonical site).
5. **Build the `PluginEntry`**:
   ```rust
   Arc::new(PluginEntry {
       name: name.clone(),
       kind,
       plugin: Arc::from(plugin),  // Box<dyn Plugin> → Arc<dyn Plugin>
       on_error,
       timeouts,
       enabled: entry_cfg.enabled,
   })
   ```
6. Insert into `plugins: HashMap<String, Arc<PluginEntry>>`.

**Disabled plugins still complete phase 2.** This matches spec §5.3 rule 7 — config bugs surface at startup regardless of `enabled` flag. Phase 3 skips disabled entries when populating `plans`.

**Phase 3 — Scan routes, validate hook subscription, build plans**:

For each `route` in `cfg.routes`:

1. Skip if `route.plugins.is_none()` or all four hook lists are empty (fast path stays in `lookup()`).
2. Parse `route.frontend: String` → `FrontendKind` via a 6-arm private `match` (same alias set as `StaticRouter::from_config`: `"anthropic_messages"|"anthropic" => AnthropicMessages`, `"openai_chat"|"openai" => OpenAiChat`, `"openai_responses"|"responses" => OpenAiResponses`). On unknown string, return `Err(RegistryBuildError::UnknownFrontend { frontend: route.frontend.clone() })`. (Production: Layer A's `VALID_FRONTENDS` check guarantees a match; build() is defensive so test/in-memory callers fail with a typed error instead of panicking.)
3. For each `(hook_str, plugin_name)` in `block.iter_references()`:
   - **Undeclared reference defense**: if `plugins.get(plugin_name)` is `None`, return `Err(RegistryBuildError::UndeclaredPluginReference { route: format!("{frontend}/{model}"), hook: hook_str, plugin_name: plugin_name.to_string() })`. (Production: Layer A catches this earlier; build() is defensive against direct in-memory callers that skip Layer A.)
   - **Layer B rule 6** (hook subscription mismatch): if `entry.plugin.hooks().contains(hook)` is false, return `Err(RegistryBuildError::HookSubscriptionMismatch { frontend, model, plugin, hook, subscribed: <Plugin::hooks() rendered as strs> })`.
   - **Skip disabled** (Layer B rule 7): if `!entry.enabled`, continue (do not push to plan).
   - Push `Arc::clone(entry)` onto the appropriate hook list in `RouteHookPlan`.
4. Insert `(model.clone(), plan)` into `plans[frontend].specific`. Set `plans[frontend].is_empty = false`.

**Phase 1 defense — factory duplicate kind**: when populating `factory_index`, if a `factory.kind_name()` is already present in the map, return `Err(RegistryBuildError::DuplicateFactoryKind { kind: kind.to_string() })`. P06a built-ins are unique by construction; the check exists to catch test-author footgun (e.g. pushing the same factory twice).

**Wildcard routes** (currently P03 only supports specific routes; wildcard support is implicit `FrontendRoutePlans::wildcard: None`). P06a does not introduce wildcard semantics.

### 4.3 Error types

`RegistryBuildError` already exists from P02 with three Layer B variants (`UnknownKind`, `Instantiation`, `HookSubscriptionMismatch`). P06a adds **three new variants** for defense-in-depth against in-memory callers that bypass Layer A:

```rust
pub enum RegistryBuildError {
    UnknownKind { plugin: String, kind: String, known: Vec<String> },
    Instantiation(#[source] PluginConfigError, /* plugin name */ String),
    HookSubscriptionMismatch {
        frontend: FrontendKind,
        model: String,
        plugin: String,
        hook: &'static str,
        subscribed: Vec<&'static str>,
    },
    // P06a additions:
    /// Two factories registered the same `kind_name()`. Catches test-author
    /// footgun (pushing the same factory twice) and future P06c user-supplied
    /// factory collisions.
    DuplicateFactoryKind { kind: String },
    /// Layer A normally rejects this; build() returns it when called from an
    /// in-memory test path that skipped Layer A.
    UndeclaredPluginReference { route: String, hook: &'static str, plugin_name: String },
    /// `route.frontend` was not one of the six accepted aliases. Layer A's
    /// `VALID_FRONTENDS` whitelist normally catches this earlier.
    UnknownFrontend { frontend: String },
}
```

`#[derive(thiserror::Error)]` impl strings are mechanical; defer to the implementation PR.

### 4.4 HookTimeouts conversion helper

Add a private associated function on `HookTimeouts`:

```rust
impl HookTimeouts {
    pub(crate) fn from_yaml(yaml: &agent_shim_config::plugins::TimeoutMs) -> Self {
        use agent_shim_config::plugins::TimeoutMs;
        let defaults = Self::default(); // 50/50/5/50 per spec §5.4
        match yaml {
            TimeoutMs::Uniform(n) => Self::uniform(*n),
            TimeoutMs::PerHook {
                default,
                on_decoded_request,
                on_resolved,
                on_stream_event,
                on_response_complete,
            } => Self {
                on_decoded_request: on_decoded_request
                    .or(*default)
                    .unwrap_or(defaults.on_decoded_request),
                on_resolved: on_resolved
                    .or(*default)
                    .unwrap_or(defaults.on_resolved),
                on_stream_event: on_stream_event
                    .or(*default)
                    .unwrap_or(defaults.on_stream_event),
                on_response_complete: on_response_complete
                    .or(*default)
                    .unwrap_or(defaults.on_response_complete),
            },
        }
    }
}
```

**Fallback chain**: per-hook override → `default` field → per-hook system default (50/50/5/50).

This is `pub(crate)` because Layer A validation in `agent-shim-config` doesn't need it — it validates the YAML shape only.

## 5. `usage_recorder` built-in design

### 5.1 Files

- `crates/plugins/src/builtin/usage_recorder.rs` — new file, ~150 LOC + ~120 LOC tests.
- `crates/plugins/src/builtin/mod.rs` — add `#[cfg(feature = "usage_recorder")] pub mod usage_recorder;` and the `builtin_plugins()` fn.

### 5.2 YAML schema

```yaml
plugins:
  usage_logger:
    type: usage_recorder
    config:
      sink: log              # required — only "log" in v1
      level: info            # optional — default "info"; one of info|debug|warn
      fields:                # optional — default = all 6
        - request_id
        - route
        - input_tokens
        - output_tokens
        - elapsed_ms
        - upstream_status
```

### 5.3 Config types

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageRecorderConfig {
    sink: Sink,
    #[serde(default)]
    level: LogLevel,
    #[serde(default = "default_fields")]
    fields: Vec<Field>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
enum Sink { Log }

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum LogLevel {
    #[default]
    Info,
    Debug,
    Warn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Field {
    RequestId,
    Route,
    InputTokens,
    OutputTokens,
    ElapsedMs,
    UpstreamStatus,
}

fn default_fields() -> Vec<Field> {
    vec![
        Field::RequestId,
        Field::Route,
        Field::InputTokens,
        Field::OutputTokens,
        Field::ElapsedMs,
        Field::UpstreamStatus,
    ]
}
```

Serde auto-rejects unknown field names (`fields: ["bogus"]` → `Field` deserialise error) and unknown levels — those surface as `PluginConfigError::Deserialize` from the factory.

### 5.4 fields semantics — validation only

The `fields` config exists for two purposes:

1. **Spelling validation**: typos in `fields` cause Layer B startup failure (Q14 — fail-closed).
2. **Documenting operator intent**: future readers of the YAML see what the operator expected to capture.

**At runtime, all 6 fields are always emitted.** Tracing macros require compile-time-static field names, so dynamic selection is not feasible without 64 (= 2^6) emit branches. Spec/rustdoc on `UsageRecorderConfig.fields` makes this explicit.

**Empty list (`fields: []`)** is accepted and treated identically to the default — `fields` does not gate runtime emission, so empty is a no-op same as full. The factory does NOT reject `fields: []`. Rustdoc on `UsageRecorderConfig.fields` calls this out.

If future operators need per-field opt-out (P07 use case?), a follow-up plan can revisit. For v1, the 6 fields are small and predictable enough that always-emit is acceptable.

### 5.5 Plugin trait impl

```rust
pub struct UsageRecorder {
    level: LogLevel,
    // fields not stored — validation already happened at factory.instantiate
}

#[async_trait]
impl Plugin for UsageRecorder {
    fn kind_name(&self) -> &'static str { "usage_recorder" }
    fn hooks(&self) -> HookSet { HookSet::RESPONSE_COMPLETE }

    async fn on_response_complete(
        &self,
        ctx: &PluginContext,
        summary: &ResponseSummary,
    ) -> PluginResult<()> {
        // P06a grill Q13: emit tokens as Option<u64>. None → tracing renders
        // "None" (via Debug) rather than the misleading "0" sentinel; Some(n)
        // → "Some(n)". Operators distinguishing "unknown" from "real zero"
        // can grep accordingly.
        let input_tokens = summary.usage.as_ref().map(|u| u.input_tokens);
        let output_tokens = summary.usage.as_ref().map(|u| u.output_tokens);
        let status_str = match summary.upstream_status {
            UpstreamStatus::Success => "success",
            UpstreamStatus::Error => "error",
            UpstreamStatus::Cancelled => "cancelled",
        };
        emit_usage!(
            self.level,
            "agent_shim.request_id" = ctx.request_id.0.as_str(),
            "agent_shim.route" = ctx.route_label.as_str(),
            "usage.input_tokens" = ?input_tokens,
            "usage.output_tokens" = ?output_tokens,
            "usage.elapsed_ms" = summary.elapsed_ms,
            "usage.upstream_status" = status_str,
        );
        Ok(())
    }
}
```

Field naming follows the P05 dotted-tracing convention:
- `agent_shim.*` for gateway-shared metadata (request_id, route).
- `usage.*` for plugin-emitted business data.
- `plugin.kind = "usage_recorder"` is implicit in the macro's invariant fields (see below).

### 5.6 `emit_usage!` macro

```rust
// File-local: 3-branch level dispatch. Tracing event! requires
// compile-time level paths, hence the match.
macro_rules! emit_usage {
    ($level:expr, $($field:tt = $val:expr),* $(,)?) => {
        match $level {
            LogLevel::Info => tracing::info!(
                target: "agent_shim::usage_recorder",
                "plugin.kind" = "usage_recorder",
                $($field = $val),*,
                "usage recorded",
            ),
            LogLevel::Warn => tracing::warn!(
                target: "agent_shim::usage_recorder",
                "plugin.kind" = "usage_recorder",
                $($field = $val),*,
                "usage recorded",
            ),
            LogLevel::Debug => tracing::debug!(
                target: "agent_shim::usage_recorder",
                "plugin.kind" = "usage_recorder",
                $($field = $val),*,
                "usage recorded",
            ),
        }
    };
}
```

The `target: "agent_shim::usage_recorder"` enables operator-level filtering via `RUST_LOG=agent_shim::usage_recorder=debug` without interfering with other plugin logs.

### 5.7 Factory

```rust
pub struct UsageRecorderFactory;

impl PluginFactory for UsageRecorderFactory {
    fn kind_name(&self) -> &'static str { "usage_recorder" }

    fn instantiate(
        &self,
        plugin_name: &str,
        config: serde_json::Value,
    ) -> Result<Box<dyn Plugin>, PluginConfigError> {
        let cfg: UsageRecorderConfig = serde_json::from_value(config)
            .map_err(|e| PluginConfigError::Deserialize(e, plugin_name.to_string()))?;
        Ok(Box::new(UsageRecorder { level: cfg.level }))
    }
}
```

## 6. `builtin_plugins` design

Lives in `crates/plugins/src/builtin/mod.rs`:

```rust
#[cfg(feature = "usage_recorder")]
pub mod usage_recorder;

use std::sync::Arc;
use crate::PluginFactory;

/// Return every built-in plugin factory compiled into this binary.
/// Operators opt out via Cargo features at compile time.
///
/// Order is alphabetical for predictable diagnostic output (e.g. the
/// `known` list in `RegistryBuildError::UnknownKind`).
pub fn builtin_plugins() -> Vec<Arc<dyn PluginFactory>> {
    let mut factories: Vec<Arc<dyn PluginFactory>> = Vec::new();
    #[cfg(feature = "usage_recorder")]
    factories.push(Arc::new(usage_recorder::UsageRecorderFactory));
    // P06b will push prompt_compressor, pii_scrubber here.
    factories
}
```

Caller pattern (in `AppState::build`):

```rust
let mut factories: Vec<Arc<dyn PluginFactory>> = agent_shim_plugins::builtin::builtin_plugins();
// (Future P06c could push user-supplied factories here.)
let plugins = Arc::new(
    agent_shim_plugins::PluginRegistry::build(&config, factories)?,
);
```

## 7. Cargo feature flags

`crates/plugins/Cargo.toml`:

```toml
[features]
default = ["usage_recorder"]
usage_recorder = []
# P06b adds: prompt_compressor, pii_scrubber, usage_recorder_prometheus
```

`usage_recorder = []` (no transitive dep activation) because the built-in uses only what plugins crate already imports (`tracing`, `serde`, `serde_json`).

`default-features = false` builds of `agent-shim-plugins` still compile cleanly — `builtin_plugins()` returns an empty Vec.

## 8. `AppState::new` signature change

### 8.1 New signature

```rust
pub async fn new(
    config: GatewayConfig,
) -> anyhow::Result<(Self, tokio::sync::mpsc::Receiver<ReloadRequest>)>
```

(Was: `(Self, Receiver)`.)

Same change for `new_with_clock` (test-only) and `new_with_plugins` (test-only injection of pre-built registry).

### 8.2 `AppState::build` change

Prepend (before the existing `let keepalive = ...`):

```rust
// P06a: build plugin registry from config + built-in factories.
let plugins = {
    let factories = agent_shim_plugins::builtin::builtin_plugins();
    Arc::new(
        agent_shim_plugins::PluginRegistry::build(&config, factories)
            .map_err(|e| anyhow::anyhow!("plugin registry build failed: {e}"))?,
    )
};
```

Remove the existing `Arc::new(PluginRegistry::empty())` placeholder.

`new_with_plugins` bypasses `builtin_plugins()` (caller supplies the `plugins: Arc<PluginRegistry>` directly). It still converts to `Result` for signature parity — the build returns `Ok(...)` wrapped because no registry-build path is exercised. The Result preserves the option for future P07 hot-reload to surface additional errors through the same channel.

### 8.3 `run_core` change

```rust
let (mut state, mut reload_rx) = crate::state::AppState::new(cfg).await?;
```

The existing `anyhow::Result` return of `run_core` absorbs the `?`. No new error variant needed.

### 8.4 Test-call-site fix-up

`AppState::new(cfg).await` → `AppState::new(cfg).await.expect("test config builds")` (or `.unwrap()` for one-off shapes). Verified ~35 call sites across `crates/gateway/src/admin/handlers.rs`, `crates/gateway/src/state.rs`, and ~30 files in `crates/gateway/tests/`. Mechanical change, no test logic touched.

## 9. Testing strategy

### 9.1 plugins crate unit tests (~11 new in `crates/plugins/src/registry.rs`)

| Test | Verifies |
|---|---|
| `build_empty_config_returns_empty_registry` | Empty `cfg.plugins` and `cfg.routes[].plugins` produces a registry with empty plans. |
| `build_unknown_kind_returns_error` | `cfg.plugins.foo.type = "nonexistent"` returns `RegistryBuildError::UnknownKind` listing alphabetically-sorted known kinds. |
| `build_factory_instantiation_failure_returns_error` | A mock factory returning `Err` produces `RegistryBuildError::Instantiation`. |
| `build_hook_subscription_mismatch_returns_error` | A plugin that subscribes only to H2 referenced from `on_response_complete` returns `RegistryBuildError::HookSubscriptionMismatch` with `subscribed` listing actual hooks. |
| `build_happy_path_populates_plans` | Single plugin on one route surfaces in `lookup(frontend, model).on_decoded_request[0]`. |
| `build_disabled_plugin_instantiated_but_not_in_plan` | `enabled: false` plugin appears in `registry.plugins` HashMap but NOT in any `RouteHookPlan`. |
| `build_timeout_yaml_uniform_converts_correctly` | YAML `timeout_ms: 50` produces `HookTimeouts::uniform(50)`. |
| `build_timeout_yaml_per_hook_converts_correctly` | YAML `timeout_ms: { default: 100, on_stream_event: 5 }` produces `HookTimeouts { default-fallback, stream_event = 5 }`. |
| `build_duplicate_factory_kind_returns_error` | Pushing two factories with same `kind_name()` returns `RegistryBuildError::DuplicateFactoryKind`. |
| `build_undeclared_plugin_ref_returns_error` | `route.plugins` references a name not in `cfg.plugins` returns `RegistryBuildError::UndeclaredPluginReference`. (Layer A would normally catch this — test constructs `GatewayConfig` literal to bypass.) |
| `build_unknown_frontend_returns_error` | `route.frontend = "weird_dialect"` returns `RegistryBuildError::UnknownFrontend`. |

### 9.2 usage_recorder unit tests (~8 new in `crates/plugins/src/builtin/usage_recorder.rs`)

| Test | Verifies |
|---|---|
| `factory_kind_name_is_usage_recorder` | `UsageRecorderFactory::kind_name()` returns `"usage_recorder"`. |
| `factory_rejects_missing_sink` | `config: {}` returns `PluginConfigError::Deserialize`. |
| `factory_rejects_unknown_field_name` | `config: { sink: log, fields: [bogus] }` returns Deserialize error. |
| `factory_rejects_unknown_level` | `config: { sink: log, level: spam }` returns Deserialize error. |
| `factory_accepts_minimal_config` | `config: { sink: log }` succeeds; `level = info`, all 6 fields default. |
| `factory_accepts_full_config` | All explicit fields parse identically. |
| `plugin_hooks_returns_response_complete_only` | `Plugin::hooks()` is `HookSet::RESPONSE_COMPLETE` exactly. |
| `on_response_complete_emits_at_configured_level` | Using `#[tracing_test::traced_test]`, verify INFO/WARN/DEBUG levels emit the expected target + plugin.kind field. |

### 9.3 Gateway integration test (~1 new in `crates/gateway/tests/usage_recorder_integration.rs`)

End-to-end through the **real** build path (no `make_app_state` shortcut). The test parses a YAML literal into `GatewayConfig`, calls `AppState::new(cfg).await?` (which exercises `builtin_plugins()` + `PluginRegistry::build`), drives one HTTP request, and uses `#[tracing_test::traced_test]` to assert the H7 emission carries the expected fields.

YAML uses an upstream pointing at an unreachable `base_url: "http://127.0.0.1:1"`. The provider call fails (`UpstreamStatus::Error`) but the H7Guard (P04 T7) still fires `run_on_response_complete` on drop — so `usage_recorder` emits its line regardless. This sidesteps the need for a mockito-driven upstream and proves H7-on-error path works end-to-end.

Assertions:
- HTTP response status (likely 502 from the resilience layer — exact code informational only)
- `traced_test` log buffer contains a line at `target = "agent_shim::usage_recorder"` with `plugin.kind = "usage_recorder"` and `agent_shim.request_id = req_...` fields populated.
- `usage.upstream_status = "error"` to confirm the H7-on-error path was hit.

### 9.4 Existing test fix-ups

Every `crates/gateway/tests/*.rs` call to `AppState::new(cfg).await` or `new_with_plugins(...).await` adds `.expect(...)` / `.unwrap()`. Estimated ~20 sites. Mechanical batch edit.

### 9.5 Expected test count

- Baseline (after P05): 824 passed
- registry::build tests: +11
- usage_recorder unit tests: +8
- gateway integration: +1
- **Expected total: ~844**

## 10. Risks & mitigations

| Risk | Mitigation |
|---|---|
| `agent-shim-plugins` depending on `agent-shim-config` reverses the historical "config is leaf" convention. | Verified no cycle (config does not import plugins). `OnErrorYaml` / `TimeoutMs` mirror types in config exist precisely to support this direction. Same pattern as router crate already uses. |
| `fields` config doing nothing at runtime is confusing. | Rustdoc on `UsageRecorderConfig.fields` + spec §5.4 explicitly state validation-only semantics. Spec §5.4 also documents the `fields: []` no-op behaviour. |
| `AppState::new` signature change ripples across ~35 test files. | Mechanical fix-up — all call sites add `.unwrap()` / `.expect(...)`. Single commit. Verified count by grep. |
| `builtin_plugins()` returns Vec and the caller may forget to use it. | Compile-time: `PluginRegistry::build(&cfg, vec![])` succeeds when `cfg.plugins` is empty, fails as `RegistryBuildError::UnknownKind` otherwise — so an unused or empty `builtin_plugins()` surface as a clear Layer B error, not silent skip. |
| Test author pushes the same factory twice. | `RegistryBuildError::DuplicateFactoryKind` defends. Unit-tested. |
| In-memory test path bypasses Layer A and produces a config with an undeclared plugin ref or unknown frontend. | `RegistryBuildError::UndeclaredPluginReference` and `RegistryBuildError::UnknownFrontend` defend. Unit-tested. |
| `Option<u64>` token emit renders as "None"/"Some(1234)" via Debug, not as a plain integer. | Acceptable — operators distinguishing "unknown" from "zero" need exactly this distinction. Documented in §5.5. |
| Integration test depends on the H7Guard firing in `UpstreamStatus::Error` path. | Already exercised by P05 T12 + P04 T11 wiring — H7Guard is a `Drop`-based guard and always fires regardless of upstream outcome. |
| Spec § 5.4 default for per-hook `on_stream_event = 5` vs others = 50 — easy to mis-derive in `HookTimeouts::from_yaml`. | Unit test `build_timeout_yaml_per_hook_converts_correctly` checks the fallback chain explicitly. |

## 11. Spec acceptance criteria

P06a is complete when:

1. `PluginRegistry::build(&cfg, factories)` exists with the documented signature.
2. Build returns first encountered `RegistryBuildError` on Layer B failures.
3. `RegistryBuildError` has 6 variants: `UnknownKind`, `Instantiation`, `HookSubscriptionMismatch`, `DuplicateFactoryKind`, `UndeclaredPluginReference`, `UnknownFrontend`.
4. Disabled plugins are instantiated but excluded from plans (§5.3 rule 7).
5. `HookTimeouts::from_yaml` correctly applies the per-hook fallback chain.
6. `builtin_plugins() -> Vec<Arc<dyn PluginFactory>>` exists in `agent_shim_plugins::builtin`.
7. `usage_recorder` ships behind feature `usage_recorder`, default-on.
8. `usage_recorder` accepts `sink: log`, `level: info|debug|warn`, `fields: [...]` (6-element whitelist, empty list accepted as no-op).
9. `usage_recorder` runtime always emits all 6 fields at the configured level on H7; tokens emit as `Option<u64>` (None when `summary.usage` is None).
10. Invalid `fields` (e.g. `[bogus]`), invalid `level`, or missing `sink` fail Layer B with `PluginConfigError::Deserialize`.
11. `AppState::new` returns `Result<(Self, Receiver), anyhow::Error>` and surfaces registry-build errors as gateway-start errors.
12. `crates/core/` diff is empty (frozen-core preserved).
13. Test count climbs ~20 (registry + plugin + integration).
14. `cargo clippy --workspace --all-targets -- -D warnings` is clean.
15. `cargo fmt --all -- --check` is clean.

## 12. YAGNI watch

- **No factory storage on `PluginRegistry`** — P07 reload can re-call `builtin_plugins()` + `build`. Storing factories adds memory + complexity for zero current value.
- **No wildcard route plugins** — P03 schema doesn't expose them; P06a doesn't introduce them.
- **No per-route plugin disable** — `enabled: false` at top-level disables globally. Per-route opt-out is fine to add later when a real use case emerges.
- **No runtime-dynamic fields** — fields config is validation-only; 64-branch emit is over-engineering.
- **No prometheus sink** — deferred to P06b where runtime-registered counters belong.
- **No prompt_compressor / pii_scrubber** — both deferred to P06b for separate design.
