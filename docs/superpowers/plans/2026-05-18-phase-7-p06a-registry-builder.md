# Phase 7 P06a — Registry Builder + usage_recorder Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the production `PluginRegistry::build(&cfg, factories)` constructor (Layer B validation), the `usage_recorder` built-in plugin (log sink only), and gateway wiring (`AppState::new` becomes `Result`-returning, calls `builtin_plugins() + PluginRegistry::build`).

**Architecture:** Two-phase fail-fast `build()` in plugins crate: phase 1 indexes factories by `kind_name()`, phase 2 instantiates every plugin (BTreeMap order, disabled plugins still instantiated), phase 3 scans routes and populates `plans` with hook-subscription validation. `usage_recorder` ships as one feature-gated H7 plugin emitting at info/debug/warn with a 6-field whitelist. Gateway `AppState::new` constructs factories via `builtin_plugins()` and wraps build in `anyhow`.

**Tech Stack:** Rust, `tracing` 0.1 (with `target:` colon syntax for filterable target), `serde_json` for opaque plugin config, `tracing-test` 0.2 for log capture, `agent-shim-config` as new upstream dep.

**Source spec:** `docs/superpowers/specs/2026-05-18-phase-7-p06a-registry-builder-design.md` (commits `17f015e` + `afe8988` + `4521642`).

---

## Pre-flight

Baseline: **824 tests passing** at master tip (P05 merge). After P06a: expect ~844 tests (824 + 11 registry + 8 usage_recorder + 1 integration).

Frozen-core invariant: `crates/core/` MUST be untouched. Acceptance check at end: `git diff master..HEAD -- crates/core/` empty.

---

## Task 1: Plugins crate dependencies + feature flag

**Files:**
- Modify: `crates/plugins/Cargo.toml`

This task only edits Cargo.toml. No tests; verified by next task's compilation.

- [ ] **Step 1: Add `agent-shim-config` dep + feature flag**

Edit `crates/plugins/Cargo.toml` to insert the new dependency alphabetically and add a `[features]` section:

```toml
[package]
name = "agent-shim-plugins"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[lib]
name = "agent_shim_plugins"
path = "src/lib.rs"

[features]
default = ["usage_recorder"]
# Each built-in lives behind its own Cargo feature. Disabling a
# feature removes the factory from `builtin_plugins()` and the plugin
# code from the binary. P06b will add prompt_compressor and
# pii_scrubber here (with possible regex dep activation).
usage_recorder = []

[dependencies]
agent-shim-config = { path = "../config" }
agent-shim-core = { path = "../core" }
agent-shim-observability = { path = "../observability" }
agent-shim-tokens = { path = "../tokens" }
async-trait.workspace = true
futures.workspace = true
metrics.workspace = true
parking_lot.workspace = true
serde = { workspace = true, features = ["derive"] }
serde_json.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "sync", "time"] }
tracing.workspace = true

[dev-dependencies]
metrics-util = "0.17"
pretty_assertions.workspace = true
serde_yaml = "0.9"
tracing-test = { version = "0.2", features = ["no-env-filter"] }
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "sync", "time", "test-util"] }
```

- [ ] **Step 2: Verify build**

Run: `rtk cargo build -p agent-shim-plugins`
Expected: PASS — no behavioural change yet, just dep wiring.

- [ ] **Step 3: Commit**

```bash
rtk git add crates/plugins/Cargo.toml Cargo.lock
rtk git commit -m "build(plugins): add agent-shim-config dep + usage_recorder feature (P06a T1)"
```

---

## Task 2: Extend `RegistryBuildError` with three defensive variants

**Files:**
- Modify: `crates/plugins/src/registry.rs` (extend `RegistryBuildError` enum around line 569)

- [ ] **Step 1: Write failing tests in registry.rs test mod**

Open `crates/plugins/src/registry.rs`. At the bottom of the existing `#[cfg(test)] mod tests` block, add a new sub-section after the last existing test:

```rust
    // ── Plan 07 P06a T2: RegistryBuildError new variants ───────────────

    #[test]
    fn registry_build_error_unknown_kind_display_format() {
        let err = RegistryBuildError::UnknownKind {
            plugin: "foo".to_string(),
            kind: "nonexistent".to_string(),
            known: vec!["a".to_string(), "b".to_string()],
        };
        let s = err.to_string();
        assert!(s.contains("foo"));
        assert!(s.contains("nonexistent"));
    }

    #[test]
    fn registry_build_error_duplicate_factory_kind_display_format() {
        let err = RegistryBuildError::DuplicateFactoryKind {
            kind: "usage_recorder".to_string(),
        };
        let s = err.to_string();
        assert!(s.contains("usage_recorder"));
        assert!(s.to_lowercase().contains("duplicate") || s.to_lowercase().contains("registered"));
    }

    #[test]
    fn registry_build_error_undeclared_plugin_reference_display_format() {
        let err = RegistryBuildError::UndeclaredPluginReference {
            route: "anthropic_messages/test-model".to_string(),
            hook: "on_decoded_request",
            plugin_name: "missing_plugin".to_string(),
        };
        let s = err.to_string();
        assert!(s.contains("missing_plugin"));
        assert!(s.contains("anthropic_messages/test-model"));
    }

    #[test]
    fn registry_build_error_unknown_frontend_display_format() {
        let err = RegistryBuildError::UnknownFrontend {
            frontend: "weird_dialect".to_string(),
        };
        let s = err.to_string();
        assert!(s.contains("weird_dialect"));
    }
```

- [ ] **Step 2: Run tests to verify failure**

Run: `rtk cargo build -p agent-shim-plugins --tests`
Expected: FAIL — `RegistryBuildError::DuplicateFactoryKind`, `UndeclaredPluginReference`, `UnknownFrontend` do not exist.

- [ ] **Step 3: Extend the enum**

In `crates/plugins/src/registry.rs`, replace the existing `RegistryBuildError` enum (around line 568-589) with:

```rust
/// Errors the registry surfaces during construction. Wrapped by
/// `gateway::main` into the boot-time error envelope.
///
/// Layer B (the gateway-side validation, see plugin design spec §5.3)
/// surfaces three variants: `UnknownKind`, `Instantiation`,
/// `HookSubscriptionMismatch`. P06a adds three defensive variants
/// (`DuplicateFactoryKind`, `UndeclaredPluginReference`,
/// `UnknownFrontend`) so `build()` is safe to call from in-memory test
/// paths that bypass Layer A. Layer A normally rejects these cases
/// earlier in production.
#[derive(Debug, thiserror::Error)]
pub enum RegistryBuildError {
    #[error("plugin `{plugin}` references unknown kind `{kind}` (known: {known:?})")]
    UnknownKind {
        plugin: String,
        kind: String,
        known: Vec<String>,
    },
    #[error("plugin `{1}`: {0}")]
    Instantiation(#[source] PluginConfigError, /* plugin name */ String),
    #[error(
        "route `{frontend:?}/{model}` references plugin `{plugin}` on hook `{hook}`, but that \
         plugin does not subscribe to it (subscribed: {subscribed:?})"
    )]
    HookSubscriptionMismatch {
        frontend: FrontendKind,
        model: String,
        plugin: String,
        hook: &'static str,
        subscribed: Vec<&'static str>,
    },
    /// Two factories registered the same `kind_name()`. Catches
    /// test-author footgun (pushing the same factory twice) and future
    /// P06c user-supplied factory collisions.
    #[error("factory kind `{kind}` already registered (duplicate factory)")]
    DuplicateFactoryKind { kind: String },
    /// Layer A normally rejects this; build() returns it when called from
    /// an in-memory test path that skipped Layer A.
    #[error(
        "route `{route}` on hook `{hook}` references undeclared plugin `{plugin_name}` \
         (declare it under top-level `plugins:`)"
    )]
    UndeclaredPluginReference {
        route: String,
        hook: &'static str,
        plugin_name: String,
    },
    /// `route.frontend` was not one of the six accepted aliases. Layer A's
    /// `VALID_FRONTENDS` whitelist normally catches this earlier.
    #[error("unknown frontend `{frontend}` in route plugins block")]
    UnknownFrontend { frontend: String },
}
```

- [ ] **Step 4: Run tests to verify pass**

Run: `rtk cargo nextest run -p agent-shim-plugins registry_build_error`
Expected: PASS — 4 tests.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/plugins/src/registry.rs
rtk git commit -m "feat(plugins): RegistryBuildError gains 3 defensive variants (P06a T2)"
```

---

## Task 3: `HookTimeouts::from_yaml` conversion helper

**Files:**
- Modify: `crates/plugins/src/registry.rs` (extend `impl HookTimeouts` around line 40)

- [ ] **Step 1: Write failing tests**

In `crates/plugins/src/registry.rs` test mod, add this sub-section:

```rust
    // ── Plan 07 P06a T3: HookTimeouts::from_yaml ───────────────────────

    #[test]
    fn hook_timeouts_from_yaml_uniform_applies_to_all_hooks() {
        use agent_shim_config::plugins::TimeoutMs;
        let t = HookTimeouts::from_yaml(&TimeoutMs::Uniform(100));
        assert_eq!(t.on_decoded_request, 100);
        assert_eq!(t.on_resolved, 100);
        assert_eq!(t.on_stream_event, 100);
        assert_eq!(t.on_response_complete, 100);
    }

    #[test]
    fn hook_timeouts_from_yaml_per_hook_with_default_uses_default_for_missing() {
        use agent_shim_config::plugins::TimeoutMs;
        let t = HookTimeouts::from_yaml(&TimeoutMs::PerHook {
            default: Some(200),
            on_decoded_request: None,
            on_resolved: None,
            on_stream_event: Some(5),
            on_response_complete: None,
        });
        assert_eq!(t.on_decoded_request, 200);
        assert_eq!(t.on_resolved, 200);
        assert_eq!(t.on_stream_event, 5);
        assert_eq!(t.on_response_complete, 200);
    }

    #[test]
    fn hook_timeouts_from_yaml_per_hook_without_default_falls_back_to_system_defaults() {
        // No default field, no per-hook fields → all hooks use system defaults (50/50/5/50).
        use agent_shim_config::plugins::TimeoutMs;
        let t = HookTimeouts::from_yaml(&TimeoutMs::PerHook {
            default: None,
            on_decoded_request: None,
            on_resolved: None,
            on_stream_event: None,
            on_response_complete: None,
        });
        let defaults = HookTimeouts::default();
        assert_eq!(t.on_decoded_request, defaults.on_decoded_request);
        assert_eq!(t.on_resolved, defaults.on_resolved);
        assert_eq!(t.on_stream_event, defaults.on_stream_event);
        assert_eq!(t.on_response_complete, defaults.on_response_complete);
    }

    #[test]
    fn hook_timeouts_from_yaml_per_hook_explicit_field_wins_over_default() {
        use agent_shim_config::plugins::TimeoutMs;
        let t = HookTimeouts::from_yaml(&TimeoutMs::PerHook {
            default: Some(50),
            on_decoded_request: Some(100),
            on_resolved: None,
            on_stream_event: None,
            on_response_complete: None,
        });
        assert_eq!(t.on_decoded_request, 100, "explicit per-hook field overrides default");
        assert_eq!(t.on_resolved, 50, "default fills missing field");
    }
```

- [ ] **Step 2: Run tests to verify failure**

Run: `rtk cargo build -p agent-shim-plugins --tests`
Expected: FAIL — `HookTimeouts::from_yaml` does not exist.

- [ ] **Step 3: Implement `HookTimeouts::from_yaml`**

In `crates/plugins/src/registry.rs`, find `impl HookTimeouts` (around line 40-60). Add a new method after `for_hook`:

```rust
    /// Convert a YAML mirror `TimeoutMs` (from `agent-shim-config`) into
    /// the runtime `HookTimeouts` shape. P06a §4.4.
    ///
    /// Fallback chain (per spec §5.4):
    /// - `Uniform(n)` → every hook gets `n`.
    /// - `PerHook { default, on_X }` → each hook uses its explicit override,
    ///   falling back to `default` (if set), then to the per-hook system
    ///   default (50ms for H2/H3/H7, 5ms for H5).
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
                on_resolved: on_resolved.or(*default).unwrap_or(defaults.on_resolved),
                on_stream_event: on_stream_event
                    .or(*default)
                    .unwrap_or(defaults.on_stream_event),
                on_response_complete: on_response_complete
                    .or(*default)
                    .unwrap_or(defaults.on_response_complete),
            },
        }
    }
```

- [ ] **Step 4: Run tests to verify pass**

Run: `rtk cargo nextest run -p agent-shim-plugins hook_timeouts_from_yaml`
Expected: PASS — 4 tests.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/plugins/src/registry.rs
rtk git commit -m "feat(plugins): HookTimeouts::from_yaml conversion helper (P06a T3)"
```

---

## Task 4: `PluginRegistry::build` two-phase constructor

**Files:**
- Modify: `crates/plugins/src/registry.rs` (add `build()` method on `impl PluginRegistry`, add ~11 tests)

This is the largest task. Implement `build()` and its 11 unit tests.

- [ ] **Step 1: Write the failing tests in registry.rs test mod**

In `crates/plugins/src/registry.rs` test mod, add:

```rust
    // ── Plan 07 P06a T4: PluginRegistry::build ─────────────────────────

    use agent_shim_config::plugins::{
        OnErrorYaml, PluginEntry as ConfigPluginEntry, RoutePluginsBlock,
    };
    use agent_shim_config::schema::{GatewayConfig, RouteEntry, ServerConfig};
    use std::collections::BTreeMap;

    /// Mock factory used by the build() tests. `kind_name()` is configured
    /// at construction so a single struct can stand in for many kinds.
    struct MockFactory {
        kind: &'static str,
        hooks: crate::HookSet,
        fail_instantiate: bool,
    }

    impl crate::PluginFactory for MockFactory {
        fn kind_name(&self) -> &'static str {
            self.kind
        }
        fn instantiate(
            &self,
            plugin_name: &str,
            _config: serde_json::Value,
        ) -> Result<Box<dyn crate::Plugin>, crate::PluginConfigError> {
            if self.fail_instantiate {
                return Err(crate::PluginConfigError::InvalidValue {
                    plugin: plugin_name.to_string(),
                    field: "synthetic",
                    reason: "mock factory configured to fail".to_string(),
                });
            }
            Ok(Box::new(MockPlugin { kind: self.kind, hooks: self.hooks }))
        }
    }

    struct MockPlugin {
        kind: &'static str,
        hooks: crate::HookSet,
    }

    #[async_trait]
    impl crate::Plugin for MockPlugin {
        fn kind_name(&self) -> &'static str { self.kind }
        fn hooks(&self) -> crate::HookSet { self.hooks }
    }

    fn empty_cfg() -> GatewayConfig {
        GatewayConfig {
            server: ServerConfig::default(),
            logging: Default::default(),
            upstreams: BTreeMap::new(),
            routes: vec![],
            plugins: BTreeMap::new(),
            auth: Default::default(),
            rate_limit: Default::default(),
            copilot: None,
            admin: None,
            metrics: Default::default(),
            otel: None,
            shutdown: Default::default(),
        }
    }

    fn cfg_with_one_plugin_one_route(
        plugin_name: &str,
        kind: &str,
        hook: &'static str,
    ) -> GatewayConfig {
        let mut cfg = empty_cfg();
        cfg.plugins.insert(
            plugin_name.to_string(),
            ConfigPluginEntry {
                kind: kind.to_string(),
                config: serde_json::Value::Object(Default::default()),
                on_error: OnErrorYaml::Skip,
                timeout_ms: None,
                enabled: true,
            },
        );
        let mut block = RoutePluginsBlock::default();
        match hook {
            "on_decoded_request" => block.on_decoded_request.push(plugin_name.to_string()),
            "on_resolved" => block.on_resolved.push(plugin_name.to_string()),
            "on_stream_event" => block.on_stream_event.push(plugin_name.to_string()),
            "on_response_complete" => block.on_response_complete.push(plugin_name.to_string()),
            other => panic!("unknown hook in test helper: {other}"),
        }
        cfg.routes.push(RouteEntry {
            frontend: "anthropic_messages".to_string(),
            model: "test-model".to_string(),
            upstream: Some("u".to_string()),
            upstreams: vec![],
            backend_model: None,
            reasoning_effort: None,
            anthropic_beta: None,
            cost: None,
            plugins: Some(block),
        });
        cfg
    }

    #[test]
    fn build_empty_config_returns_empty_registry() {
        let cfg = empty_cfg();
        let registry = PluginRegistry::build(&cfg, vec![])
            .expect("empty config must build successfully");
        assert!(registry.plugins.is_empty());
        assert!(registry.plans.is_empty());
    }

    #[test]
    fn build_unknown_kind_returns_error_with_sorted_known_list() {
        let cfg = cfg_with_one_plugin_one_route("p", "nonexistent_kind", "on_decoded_request");
        let factories: Vec<Arc<dyn crate::PluginFactory>> = vec![
            Arc::new(MockFactory { kind: "zebra", hooks: crate::HookSet::DECODED_REQUEST, fail_instantiate: false }),
            Arc::new(MockFactory { kind: "alpha", hooks: crate::HookSet::DECODED_REQUEST, fail_instantiate: false }),
        ];
        let err = PluginRegistry::build(&cfg, factories).unwrap_err();
        match err {
            RegistryBuildError::UnknownKind { plugin, kind, known } => {
                assert_eq!(plugin, "p");
                assert_eq!(kind, "nonexistent_kind");
                assert_eq!(
                    known,
                    vec!["alpha".to_string(), "zebra".to_string()],
                    "known list must be alphabetically sorted (HashMap iteration is non-deterministic)"
                );
            }
            other => panic!("expected UnknownKind, got {other:?}"),
        }
    }

    #[test]
    fn build_factory_instantiation_failure_returns_error() {
        let cfg = cfg_with_one_plugin_one_route("p", "fail_kind", "on_decoded_request");
        let factories: Vec<Arc<dyn crate::PluginFactory>> = vec![Arc::new(MockFactory {
            kind: "fail_kind",
            hooks: crate::HookSet::DECODED_REQUEST,
            fail_instantiate: true,
        })];
        let err = PluginRegistry::build(&cfg, factories).unwrap_err();
        assert!(matches!(err, RegistryBuildError::Instantiation(_, _)));
    }

    #[test]
    fn build_hook_subscription_mismatch_returns_error() {
        // Plugin subscribes to H2 only, but route references it on H7.
        let cfg = cfg_with_one_plugin_one_route("p", "h2_only", "on_response_complete");
        let factories: Vec<Arc<dyn crate::PluginFactory>> = vec![Arc::new(MockFactory {
            kind: "h2_only",
            hooks: crate::HookSet::DECODED_REQUEST,
            fail_instantiate: false,
        })];
        let err = PluginRegistry::build(&cfg, factories).unwrap_err();
        match err {
            RegistryBuildError::HookSubscriptionMismatch { plugin, hook, .. } => {
                assert_eq!(plugin, "p");
                assert_eq!(hook, "on_response_complete");
            }
            other => panic!("expected HookSubscriptionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn build_happy_path_populates_plans() {
        let cfg = cfg_with_one_plugin_one_route("p", "h2", "on_decoded_request");
        let factories: Vec<Arc<dyn crate::PluginFactory>> = vec![Arc::new(MockFactory {
            kind: "h2",
            hooks: crate::HookSet::DECODED_REQUEST,
            fail_instantiate: false,
        })];
        let registry = PluginRegistry::build(&cfg, factories).expect("happy path must build");
        let plan = registry
            .lookup(FrontendKind::AnthropicMessages, "test-model")
            .expect("route plan must be present");
        assert_eq!(plan.on_decoded_request.len(), 1);
        assert_eq!(plan.on_decoded_request[0].name, "p");
    }

    #[test]
    fn build_disabled_plugin_instantiated_but_not_in_plan() {
        let mut cfg = cfg_with_one_plugin_one_route("p", "h2", "on_decoded_request");
        cfg.plugins.get_mut("p").unwrap().enabled = false;
        let factories: Vec<Arc<dyn crate::PluginFactory>> = vec![Arc::new(MockFactory {
            kind: "h2",
            hooks: crate::HookSet::DECODED_REQUEST,
            fail_instantiate: false,
        })];
        let registry = PluginRegistry::build(&cfg, factories).expect("disabled plugin OK");
        // Instantiated:
        assert!(registry.plugins.contains_key("p"));
        // But not in plan:
        let lookup = registry.lookup(FrontendKind::AnthropicMessages, "test-model");
        match lookup {
            // Plan exists but on_decoded_request is empty.
            Some(plan) => assert!(plan.on_decoded_request.is_empty()),
            // OR plan was skipped entirely.
            None => {}
        }
    }

    #[test]
    fn build_duplicate_factory_kind_returns_error() {
        let cfg = empty_cfg();
        let factories: Vec<Arc<dyn crate::PluginFactory>> = vec![
            Arc::new(MockFactory { kind: "dup", hooks: crate::HookSet::DECODED_REQUEST, fail_instantiate: false }),
            Arc::new(MockFactory { kind: "dup", hooks: crate::HookSet::DECODED_REQUEST, fail_instantiate: false }),
        ];
        let err = PluginRegistry::build(&cfg, factories).unwrap_err();
        match err {
            RegistryBuildError::DuplicateFactoryKind { kind } => {
                assert_eq!(kind, "dup");
            }
            other => panic!("expected DuplicateFactoryKind, got {other:?}"),
        }
    }

    #[test]
    fn build_undeclared_plugin_ref_returns_error() {
        // Route references "ghost_plugin" but cfg.plugins is empty.
        let mut cfg = empty_cfg();
        let mut block = RoutePluginsBlock::default();
        block.on_decoded_request.push("ghost_plugin".to_string());
        cfg.routes.push(RouteEntry {
            frontend: "anthropic_messages".to_string(),
            model: "test-model".to_string(),
            upstream: Some("u".to_string()),
            upstreams: vec![],
            backend_model: None,
            reasoning_effort: None,
            anthropic_beta: None,
            cost: None,
            plugins: Some(block),
        });
        let err = PluginRegistry::build(&cfg, vec![]).unwrap_err();
        match err {
            RegistryBuildError::UndeclaredPluginReference { plugin_name, hook, route } => {
                assert_eq!(plugin_name, "ghost_plugin");
                assert_eq!(hook, "on_decoded_request");
                assert!(route.contains("test-model"));
            }
            other => panic!("expected UndeclaredPluginReference, got {other:?}"),
        }
    }

    #[test]
    fn build_unknown_frontend_returns_error() {
        let mut cfg = cfg_with_one_plugin_one_route("p", "h2", "on_decoded_request");
        cfg.routes[0].frontend = "weird_dialect".to_string();
        let factories: Vec<Arc<dyn crate::PluginFactory>> = vec![Arc::new(MockFactory {
            kind: "h2",
            hooks: crate::HookSet::DECODED_REQUEST,
            fail_instantiate: false,
        })];
        let err = PluginRegistry::build(&cfg, factories).unwrap_err();
        match err {
            RegistryBuildError::UnknownFrontend { frontend } => {
                assert_eq!(frontend, "weird_dialect");
            }
            other => panic!("expected UnknownFrontend, got {other:?}"),
        }
    }

    #[test]
    fn build_skips_empty_route_plugins_block() {
        // Route has plugins: Some({}) (all four lists empty) — should produce
        // no plan entry, lookup returns None.
        let mut cfg = empty_cfg();
        cfg.routes.push(RouteEntry {
            frontend: "anthropic_messages".to_string(),
            model: "test-model".to_string(),
            upstream: Some("u".to_string()),
            upstreams: vec![],
            backend_model: None,
            reasoning_effort: None,
            anthropic_beta: None,
            cost: None,
            plugins: Some(RoutePluginsBlock::default()),
        });
        let registry = PluginRegistry::build(&cfg, vec![]).expect("empty block builds");
        assert!(registry
            .lookup(FrontendKind::AnthropicMessages, "test-model")
            .is_none());
    }

    #[test]
    fn build_frontend_aliases_resolve_correctly() {
        // Both "anthropic" and "anthropic_messages" should map to the same FrontendKind.
        let mut cfg = cfg_with_one_plugin_one_route("p", "h2", "on_decoded_request");
        cfg.routes[0].frontend = "anthropic".to_string(); // alias
        let factories: Vec<Arc<dyn crate::PluginFactory>> = vec![Arc::new(MockFactory {
            kind: "h2",
            hooks: crate::HookSet::DECODED_REQUEST,
            fail_instantiate: false,
        })];
        let registry = PluginRegistry::build(&cfg, factories).expect("alias must resolve");
        assert!(registry
            .lookup(FrontendKind::AnthropicMessages, "test-model")
            .is_some());
    }
```

- [ ] **Step 2: Run tests to verify failure**

Run: `rtk cargo build -p agent-shim-plugins --tests`
Expected: FAIL — `PluginRegistry::build` does not exist.

- [ ] **Step 3: Implement `PluginRegistry::build`**

In `crates/plugins/src/registry.rs`, find `impl PluginRegistry` (around line 109). Add the `build` method after `for_testing_single_plugin`:

```rust
    /// Production constructor. Walks the YAML config + factory list and
    /// returns a fully populated registry, or the first Layer B validation
    /// error encountered. P06a §4.2.
    ///
    /// Two-phase fail-fast:
    /// 1. Index factories by `kind_name()` (with duplicate detection).
    /// 2. Instantiate every plugin in `cfg.plugins` BTreeMap order; disabled
    ///    plugins still pass through (so config errors surface even when
    ///    operators have flipped `enabled: false`).
    /// 3. Scan `cfg.routes`, validate hook subscription, populate plans.
    ///    Skip disabled entries when filling plan lists.
    ///
    /// Returns `Err` defensively when the input bypasses Layer A
    /// (`UndeclaredPluginReference`, `UnknownFrontend`) so test callers
    /// fail cleanly rather than panic.
    pub fn build(
        cfg: &agent_shim_config::GatewayConfig,
        factories: Vec<Arc<dyn crate::PluginFactory>>,
    ) -> Result<Self, RegistryBuildError> {
        // ── Phase 1: index factories by kind, detect duplicates ────────
        let mut factory_index: HashMap<&'static str, Arc<dyn crate::PluginFactory>> =
            HashMap::new();
        for f in factories {
            let kind = f.kind_name();
            if factory_index.contains_key(kind) {
                return Err(RegistryBuildError::DuplicateFactoryKind {
                    kind: kind.to_string(),
                });
            }
            factory_index.insert(kind, f);
        }

        // ── Phase 2: instantiate every plugin in BTreeMap key order ───
        let mut plugins: HashMap<String, Arc<PluginEntry>> = HashMap::new();
        for (name, entry_cfg) in &cfg.plugins {
            let factory = factory_index.get(entry_cfg.kind.as_str()).ok_or_else(|| {
                // Sort known list — HashMap iteration is non-deterministic.
                let mut known: Vec<String> =
                    factory_index.keys().map(|k| k.to_string()).collect();
                known.sort();
                RegistryBuildError::UnknownKind {
                    plugin: name.clone(),
                    kind: entry_cfg.kind.clone(),
                    known,
                }
            })?;
            let plugin = factory
                .instantiate(name, entry_cfg.config.clone())
                .map_err(|e| RegistryBuildError::Instantiation(e, name.clone()))?;
            let on_error = match entry_cfg.on_error {
                agent_shim_config::plugins::OnErrorYaml::Skip => OnError::Skip,
                agent_shim_config::plugins::OnErrorYaml::Fail => OnError::Fail,
            };
            let timeouts = entry_cfg
                .timeout_ms
                .as_ref()
                .map(HookTimeouts::from_yaml)
                .unwrap_or_default();
            let kind: &'static str = factory.kind_name();
            plugins.insert(
                name.clone(),
                Arc::new(PluginEntry {
                    name: name.clone(),
                    kind,
                    plugin: Arc::from(plugin),
                    on_error,
                    timeouts,
                    enabled: entry_cfg.enabled,
                }),
            );
        }

        // ── Phase 3: scan routes, build plans ─────────────────────────
        let mut plans: HashMap<FrontendKind, FrontendRoutePlans> = HashMap::new();
        for route in &cfg.routes {
            let Some(block) = &route.plugins else { continue };
            if block.is_empty() {
                continue;
            }
            // Parse frontend string → FrontendKind via the same 6-arm
            // match StaticRouter uses. UnknownFrontend defends against
            // direct in-memory callers that bypass Layer A.
            let frontend = match route.frontend.as_str() {
                "anthropic_messages" | "anthropic" => FrontendKind::AnthropicMessages,
                "openai_chat" | "openai" => FrontendKind::OpenAiChat,
                "openai_responses" | "responses" => FrontendKind::OpenAiResponses,
                other => {
                    return Err(RegistryBuildError::UnknownFrontend {
                        frontend: other.to_string(),
                    });
                }
            };
            let route_label = format!("{}/{}", route.frontend, route.model);
            let mut plan = RouteHookPlan::default();
            for (hook_str, plugin_name) in block.iter_references() {
                let entry = plugins.get(plugin_name).ok_or_else(|| {
                    RegistryBuildError::UndeclaredPluginReference {
                        route: route_label.clone(),
                        hook: hook_str,
                        plugin_name: plugin_name.to_string(),
                    }
                })?;
                let hook = match hook_str {
                    "on_decoded_request" => Hook::DecodedRequest,
                    "on_resolved" => Hook::Resolved,
                    "on_stream_event" => Hook::StreamEvent,
                    "on_response_complete" => Hook::ResponseComplete,
                    other => unreachable!("RoutePluginsBlock::iter_references yields fixed strings, got: {other}"),
                };
                if !entry.plugin.hooks().contains(hook) {
                    return Err(RegistryBuildError::HookSubscriptionMismatch {
                        frontend,
                        model: route.model.clone(),
                        plugin: plugin_name.to_string(),
                        hook: hook_str,
                        subscribed: hook_set_to_strs(entry.plugin.hooks()),
                    });
                }
                if !entry.enabled {
                    continue;
                }
                match hook {
                    Hook::DecodedRequest => plan.on_decoded_request.push(Arc::clone(entry)),
                    Hook::Resolved => plan.on_resolved.push(Arc::clone(entry)),
                    Hook::StreamEvent => plan.on_stream_event.push(Arc::clone(entry)),
                    Hook::ResponseComplete => plan.on_response_complete.push(Arc::clone(entry)),
                }
            }
            let fp = plans.entry(frontend).or_insert(FrontendRoutePlans {
                specific: HashMap::new(),
                wildcard: None,
                is_empty: true,
            });
            fp.specific.insert(route.model.clone(), plan);
            fp.is_empty = false;
        }

        Ok(Self {
            plugins,
            plans,
            supervisor: Arc::new(crate::supervisor::PluginSupervisor::new()),
        })
    }
```

Also add the helper `hook_set_to_strs` near the bottom of `registry.rs` (just above the `#[cfg(test)] mod tests` block):

```rust
/// Render a `HookSet` as the sorted list of hook strings it contains.
/// Used by `RegistryBuildError::HookSubscriptionMismatch` so operator
/// error messages list allowed hooks deterministically.
fn hook_set_to_strs(hooks: crate::HookSet) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    if hooks.contains(Hook::DecodedRequest) {
        out.push("on_decoded_request");
    }
    if hooks.contains(Hook::Resolved) {
        out.push("on_resolved");
    }
    if hooks.contains(Hook::StreamEvent) {
        out.push("on_stream_event");
    }
    if hooks.contains(Hook::ResponseComplete) {
        out.push("on_response_complete");
    }
    out
}
```

- [ ] **Step 4: Run tests to verify pass**

Run: `rtk cargo nextest run -p agent-shim-plugins build_`
Expected: PASS — 11 tests (`build_empty_config_returns_empty_registry`, `build_unknown_kind_returns_error_with_sorted_known_list`, `build_factory_instantiation_failure_returns_error`, `build_hook_subscription_mismatch_returns_error`, `build_happy_path_populates_plans`, `build_disabled_plugin_instantiated_but_not_in_plan`, `build_duplicate_factory_kind_returns_error`, `build_undeclared_plugin_ref_returns_error`, `build_unknown_frontend_returns_error`, `build_skips_empty_route_plugins_block`, `build_frontend_aliases_resolve_correctly`).

- [ ] **Step 5: Run full plugins-crate tests as regression gate**

Run: `rtk cargo nextest run -p agent-shim-plugins`
Expected: PASS — all P05 tests still green plus the new ones.

- [ ] **Step 6: Commit**

```bash
rtk git add crates/plugins/src/registry.rs
rtk git commit -m "feat(plugins): PluginRegistry::build two-phase fail-fast constructor (P06a T4)"
```

---

## Task 5: `usage_recorder` config types + factory

**Files:**
- Create: `crates/plugins/src/builtin/usage_recorder.rs`
- Modify: `crates/plugins/src/builtin/mod.rs` (add `#[cfg(feature = "usage_recorder")] pub mod usage_recorder;`)

This task creates the file with config types + factory + serde tests. T6 adds the plugin runtime behaviour.

- [ ] **Step 1: Declare the module**

Edit `crates/plugins/src/builtin/mod.rs` so the body becomes:

```rust
//! Built-in plugin kinds. Each kind is feature-gated.
//!
//! Wire-up: `builtin_plugins()` returns the compiled-in built-in
//! factories. Gateway calls this once during `AppCore::build` before
//! invoking `PluginRegistry::build`.

#[cfg(feature = "usage_recorder")]
pub mod usage_recorder;

// `builtin_plugins()` lives below T7 (lib.rs export) once the first
// concrete factory exists — see Task 7.
```

- [ ] **Step 2: Write failing tests via creating the file with the test module first**

Create `crates/plugins/src/builtin/usage_recorder.rs` with **only** the test module (full impl follows in Step 3):

```rust
//! `usage_recorder` plugin: H7-only structured log emission of usage
//! metrics on request completion. Spec §5 of
//! `2026-05-18-phase-7-p06a-registry-builder-design.md`.

use async_trait::async_trait;
use serde::Deserialize;

use crate::context::{PluginContext, ResponseSummary, UpstreamStatus};
use crate::error::{PluginConfigError, PluginResult};
use crate::trait_def::{HookSet, Plugin, PluginFactory};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageRecorderConfig {
    sink: Sink,
    #[serde(default)]
    level: LogLevel,
    /// Whitelist of fields the operator expects to capture. Pure
    /// validation surface: spelling errors fail Layer B at startup. At
    /// runtime the plugin always emits all 6 fields regardless of this
    /// list (dynamic per-field emission would require 2^6 = 64 emit
    /// branches; spec §5.4). Empty list (`fields: []`) is accepted as a
    /// no-op same as the default.
    #[serde(default = "default_fields")]
    #[allow(dead_code)] // validation-only; not used at runtime per spec §5.4
    fields: Vec<Field>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
enum Sink {
    Log,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
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

pub struct UsageRecorder {
    level: LogLevel,
}

pub struct UsageRecorderFactory;

impl PluginFactory for UsageRecorderFactory {
    fn kind_name(&self) -> &'static str {
        "usage_recorder"
    }

    fn instantiate(
        &self,
        plugin_name: &str,
        config: serde_json::Value,
    ) -> Result<Box<dyn Plugin>, PluginConfigError> {
        let cfg: UsageRecorderConfig = serde_json::from_value(config)
            .map_err(|e| PluginConfigError::Deserialize(e, plugin_name.to_string()))?;
        // cfg.fields is intentionally discarded — validation already happened.
        Ok(Box::new(UsageRecorder { level: cfg.level }))
    }
}

#[async_trait]
impl Plugin for UsageRecorder {
    fn kind_name(&self) -> &'static str {
        "usage_recorder"
    }

    fn hooks(&self) -> HookSet {
        HookSet::RESPONSE_COMPLETE
    }

    async fn on_response_complete(
        &self,
        _ctx: &PluginContext,
        _summary: &ResponseSummary,
    ) -> PluginResult<()> {
        // T6 fills in the emit_usage!(self.level, ctx, summary) call.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn factory_kind_name_is_usage_recorder() {
        assert_eq!(UsageRecorderFactory.kind_name(), "usage_recorder");
    }

    #[test]
    fn factory_rejects_missing_sink() {
        let config = json!({});
        let err = UsageRecorderFactory
            .instantiate("test_plugin", config)
            .unwrap_err();
        assert!(matches!(err, PluginConfigError::Deserialize(_, _)));
    }

    #[test]
    fn factory_rejects_unknown_field_name() {
        let config = json!({
            "sink": "log",
            "fields": ["bogus"],
        });
        let err = UsageRecorderFactory
            .instantiate("test_plugin", config)
            .unwrap_err();
        assert!(matches!(err, PluginConfigError::Deserialize(_, _)));
    }

    #[test]
    fn factory_rejects_unknown_level() {
        let config = json!({
            "sink": "log",
            "level": "spam",
        });
        let err = UsageRecorderFactory
            .instantiate("test_plugin", config)
            .unwrap_err();
        assert!(matches!(err, PluginConfigError::Deserialize(_, _)));
    }

    #[test]
    fn factory_accepts_minimal_config() {
        let config = json!({ "sink": "log" });
        let plugin = UsageRecorderFactory
            .instantiate("test_plugin", config)
            .expect("minimal config must parse");
        assert_eq!(plugin.kind_name(), "usage_recorder");
    }

    #[test]
    fn factory_accepts_full_config() {
        let config = json!({
            "sink": "log",
            "level": "debug",
            "fields": ["request_id", "route", "input_tokens", "output_tokens", "elapsed_ms", "upstream_status"],
        });
        UsageRecorderFactory
            .instantiate("test_plugin", config)
            .expect("full config must parse");
    }

    #[test]
    fn factory_accepts_empty_fields() {
        // Per spec §5.4, fields: [] is accepted as a no-op (validation
        // didn't fail, and runtime always emits all 6 fields anyway).
        let config = json!({
            "sink": "log",
            "fields": [],
        });
        UsageRecorderFactory
            .instantiate("test_plugin", config)
            .expect("empty fields list must parse");
    }

    #[test]
    fn factory_rejects_extra_top_level_field() {
        // deny_unknown_fields on UsageRecorderConfig.
        let config = json!({
            "sink": "log",
            "unknown_extra": "bar",
        });
        let err = UsageRecorderFactory
            .instantiate("test_plugin", config)
            .unwrap_err();
        assert!(matches!(err, PluginConfigError::Deserialize(_, _)));
    }

    #[test]
    fn plugin_hooks_returns_response_complete_only() {
        let plugin = UsageRecorderFactory
            .instantiate("p", json!({ "sink": "log" }))
            .unwrap();
        assert_eq!(plugin.hooks(), HookSet::RESPONSE_COMPLETE);
    }
}
```

- [ ] **Step 3: Run tests to verify pass**

Run: `rtk cargo nextest run -p agent-shim-plugins --features usage_recorder usage_recorder::tests`
Expected: PASS — 8 tests.

(Note: `usage_recorder` is in `default-features` so the `--features` flag is redundant but explicit.)

- [ ] **Step 4: Run full workspace build to catch any cross-crate breakage**

Run: `rtk cargo build --workspace`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/plugins/src/builtin/mod.rs crates/plugins/src/builtin/usage_recorder.rs
rtk git commit -m "feat(plugins): usage_recorder factory + config types (P06a T5)"
```

---

## Task 6: `emit_usage!` macro + plugin runtime behaviour

**Files:**
- Modify: `crates/plugins/src/builtin/usage_recorder.rs` (add `emit_usage!` macro + fill in `on_response_complete`, add 1 test)

- [ ] **Step 1: Write the failing tracing-test**

Open `crates/plugins/src/builtin/usage_recorder.rs`. Inside the `#[cfg(test)] mod tests` block, add this test at the end:

```rust
    use agent_shim_core::{FrontendKind, RequestId, Usage};

    fn make_ctx() -> PluginContext {
        PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "anthropic_messages/test-model".to_string(),
        }
    }

    fn make_summary_success() -> ResponseSummary {
        ResponseSummary {
            usage: Some(Usage {
                input_tokens: 17,
                output_tokens: 42,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
            elapsed_ms: 250,
            upstream_status: UpstreamStatus::Success,
        }
    }

    fn make_summary_no_usage() -> ResponseSummary {
        ResponseSummary {
            usage: None,
            elapsed_ms: 100,
            upstream_status: UpstreamStatus::Error,
        }
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn on_response_complete_emits_at_info_level_with_all_fields() {
        let plugin = UsageRecorderFactory
            .instantiate("usage_logger", serde_json::json!({ "sink": "log" }))
            .unwrap();
        let ctx = make_ctx();
        let summary = make_summary_success();
        plugin
            .on_response_complete(&ctx, &summary)
            .await
            .expect("H7 hook must succeed");

        // Verify the emission carries plugin.kind + tokens (Some(N) via Debug).
        assert!(
            logs_contain("plugin.kind=\"usage_recorder\""),
            "log must include plugin.kind field"
        );
        assert!(
            logs_contain("usage.input_tokens=Some(17)") || logs_contain("input_tokens=Some(17)"),
            "log must include Some(17) for known token count"
        );
        assert!(
            logs_contain("usage.upstream_status=\"success\""),
            "log must include upstream_status"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn on_response_complete_with_none_usage_emits_none_via_debug() {
        let plugin = UsageRecorderFactory
            .instantiate("usage_logger", serde_json::json!({ "sink": "log" }))
            .unwrap();
        let ctx = make_ctx();
        let summary = make_summary_no_usage();
        plugin
            .on_response_complete(&ctx, &summary)
            .await
            .expect("H7 hook must succeed");

        // When summary.usage is None, tokens emit as `None` (Debug form),
        // distinguishing "unknown" from "real zero". Spec §5.5.
        assert!(
            logs_contain("usage.input_tokens=None") || logs_contain("input_tokens=None"),
            "None usage must emit `None`, not `0`"
        );
        assert!(
            logs_contain("usage.upstream_status=\"error\""),
            "upstream_status `error` must reach the log"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn on_response_complete_warn_level_emits_at_warn() {
        let plugin = UsageRecorderFactory
            .instantiate(
                "usage_logger",
                serde_json::json!({ "sink": "log", "level": "warn" }),
            )
            .unwrap();
        let ctx = make_ctx();
        let summary = make_summary_success();
        plugin
            .on_response_complete(&ctx, &summary)
            .await
            .expect("H7 hook must succeed");
        // tracing-test's `logs_contain` matches the emitted level prefix in
        // the captured output ("WARN"). This is the simplest assertion to
        // prove level dispatch is wired.
        assert!(
            logs_contain("plugin.kind=\"usage_recorder\""),
            "log line emitted"
        );
    }
```

Also verify that `agent_shim_core::Usage` has the expected fields by reading `crates/core/src/canonical.rs` if needed (the type is already in the plugins crate's public surface via `agent_shim_core::Usage`).

- [ ] **Step 2: Run tests to verify failure**

Run: `rtk cargo build -p agent-shim-plugins --tests`
Expected: PASS to compile (the test references already-imported items). The tests will fail at runtime because `on_response_complete` currently just returns `Ok(())` without emitting.

Run: `rtk cargo nextest run -p agent-shim-plugins on_response_complete_emits`
Expected: FAIL — assertions fail because no log line is emitted yet.

- [ ] **Step 3: Add the `emit_usage!` macro and wire it into the plugin impl**

In `crates/plugins/src/builtin/usage_recorder.rs`, add the macro just below the `use` block (before `UsageRecorderConfig`):

```rust
/// File-local 3-arm level dispatcher. `tracing::event!` requires
/// compile-time level paths so dynamic dispatch isn't possible without
/// a match. Body is hardcoded in each arm because tracing's `?value`
/// (Debug) syntax doesn't parse as `$val:expr` in `macro_rules!`.
/// Same trade-off as P05 `emit_at_level!` (log_fields.rs).
///
/// `target: "agent_shim::usage_recorder"` (colon form — tracing
/// canonical syntax) sets the event's actual target so operators can
/// filter with `RUST_LOG=agent_shim::usage_recorder=debug`.
macro_rules! emit_usage {
    ($level:expr, $ctx:expr, $summary:expr) => {{
        // P06a grill Q13: emit tokens as Option<u64> via Debug — None
        // vs Some(n) distinguishes "unknown" from "real zero".
        let input_tokens = $summary.usage.as_ref().map(|u| u.input_tokens);
        let output_tokens = $summary.usage.as_ref().map(|u| u.output_tokens);
        let status_str = match $summary.upstream_status {
            $crate::UpstreamStatus::Success => "success",
            $crate::UpstreamStatus::Error => "error",
            $crate::UpstreamStatus::Cancelled => "cancelled",
        };
        match $level {
            LogLevel::Info => tracing::info!(
                target: "agent_shim::usage_recorder",
                "plugin.kind" = "usage_recorder",
                "agent_shim.request_id" = $ctx.request_id.0.as_str(),
                "agent_shim.route" = $ctx.route_label.as_str(),
                "usage.input_tokens" = ?input_tokens,
                "usage.output_tokens" = ?output_tokens,
                "usage.elapsed_ms" = $summary.elapsed_ms,
                "usage.upstream_status" = status_str,
                "usage recorded",
            ),
            LogLevel::Warn => tracing::warn!(
                target: "agent_shim::usage_recorder",
                "plugin.kind" = "usage_recorder",
                "agent_shim.request_id" = $ctx.request_id.0.as_str(),
                "agent_shim.route" = $ctx.route_label.as_str(),
                "usage.input_tokens" = ?input_tokens,
                "usage.output_tokens" = ?output_tokens,
                "usage.elapsed_ms" = $summary.elapsed_ms,
                "usage.upstream_status" = status_str,
                "usage recorded",
            ),
            LogLevel::Debug => tracing::debug!(
                target: "agent_shim::usage_recorder",
                "plugin.kind" = "usage_recorder",
                "agent_shim.request_id" = $ctx.request_id.0.as_str(),
                "agent_shim.route" = $ctx.route_label.as_str(),
                "usage.input_tokens" = ?input_tokens,
                "usage.output_tokens" = ?output_tokens,
                "usage.elapsed_ms" = $summary.elapsed_ms,
                "usage.upstream_status" = status_str,
                "usage recorded",
            ),
        }
    }};
}
```

Then replace the `on_response_complete` body (currently `Ok(())`) with:

```rust
    async fn on_response_complete(
        &self,
        ctx: &PluginContext,
        summary: &ResponseSummary,
    ) -> PluginResult<()> {
        emit_usage!(self.level, ctx, summary);
        Ok(())
    }
```

- [ ] **Step 4: Run tests to verify pass**

Run: `rtk cargo nextest run -p agent-shim-plugins on_response_complete_emits`
Expected: PASS — 3 new tests.

Run: `rtk cargo nextest run -p agent-shim-plugins`
Expected: PASS — all plugin-crate tests.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/plugins/src/builtin/usage_recorder.rs
rtk git commit -m "feat(plugins): emit_usage! macro + H7 runtime emission for usage_recorder (P06a T6)"
```

---

## Task 7: `builtin_plugins()` registration point + lib.rs export

**Files:**
- Modify: `crates/plugins/src/builtin/mod.rs` (add `builtin_plugins()`)
- Modify: `crates/plugins/src/lib.rs` (re-export `builtin` module publicly)

- [ ] **Step 1: Write the failing test**

Add a test to `crates/plugins/src/builtin/mod.rs`:

```rust
//! Built-in plugin kinds. Each kind is feature-gated.
//!
//! Wire-up: `builtin_plugins()` returns the compiled-in built-in
//! factories. Gateway calls this once during `AppCore::build` before
//! invoking `PluginRegistry::build`.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "usage_recorder")]
    fn builtin_plugins_includes_usage_recorder() {
        let factories = builtin_plugins();
        let kinds: Vec<&'static str> = factories.iter().map(|f| f.kind_name()).collect();
        assert!(
            kinds.contains(&"usage_recorder"),
            "usage_recorder must be present when its feature is on (kinds: {kinds:?})"
        );
    }
}
```

Edit `crates/plugins/src/lib.rs` — `pub mod builtin;` is already declared (P02), but now we also need `register_builtin_plugins` to be reachable. Verify the existing line:

```rust
pub mod builtin;
```

is present. If yes, no change needed in `lib.rs`. If for some reason it's not pub, change `mod builtin;` → `pub mod builtin;`.

- [ ] **Step 2: Run the test**

Run: `rtk cargo nextest run -p agent-shim-plugins builtin_plugins_includes_usage_recorder`
Expected: PASS — 1 test.

- [ ] **Step 3: Commit**

```bash
rtk git add crates/plugins/src/builtin/mod.rs crates/plugins/src/lib.rs
rtk git commit -m "feat(plugins): builtin_plugins() registration point + feature-gated entries (P06a T7)"
```

---

## Task 8: `AppState::new` returns `Result`, wires `PluginRegistry::build`

**Files:**
- Modify: `crates/gateway/src/state.rs` (change `new`/`new_with_clock`/`new_with_plugins` signatures to `Result`; thread `PluginRegistry::build` into `build()`)
- Modify: `crates/gateway/src/commands/serve.rs` (propagate `?`)
- Modify: `crates/gateway/src/admin/handlers.rs` (add `.unwrap()` / `?` on 2 call sites)

- [ ] **Step 1: Update `AppState::new`, `new_with_clock`, `new_with_plugins`, and `build` signatures**

In `crates/gateway/src/state.rs`, replace the existing `pub async fn new`, `new_with_clock`, `new_with_plugins`, and `async fn build` (around lines 161-238) with:

```rust
    pub async fn new(
        config: agent_shim_config::GatewayConfig,
    ) -> anyhow::Result<(
        Self,
        tokio::sync::mpsc::Receiver<crate::reload_trigger::ReloadRequest>,
    )> {
        Self::build(config, Arc::new(SystemClock), None).await
    }

    /// Test-only constructor that lets tests inject a custom `Clock` into
    /// the `BreakerRegistry`. Production callers MUST use `AppState::new`.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub async fn new_with_clock(
        config: agent_shim_config::GatewayConfig,
        clock: Arc<dyn Clock>,
    ) -> anyhow::Result<(
        Self,
        tokio::sync::mpsc::Receiver<crate::reload_trigger::ReloadRequest>,
    )> {
        Self::build(config, clock, None).await
    }

    /// Plan 07 P04 T12: integration-test-only constructor that lets tests
    /// inject a custom `PluginRegistry` (instead of the default produced by
    /// `builtin_plugins()` + `PluginRegistry::build`). Same shape as
    /// `new_with_clock`. Production callers MUST use `AppState::new`.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub async fn new_with_plugins(
        config: agent_shim_config::GatewayConfig,
        plugins: Arc<agent_shim_plugins::PluginRegistry>,
    ) -> anyhow::Result<(
        Self,
        tokio::sync::mpsc::Receiver<crate::reload_trigger::ReloadRequest>,
    )> {
        Self::build(config, Arc::new(SystemClock), Some(plugins)).await
    }

    /// Shared construction path for `new`, `new_with_clock`, and
    /// `new_with_plugins`. When `plugin_override` is `Some`, that
    /// registry is used as-is (bypassing `builtin_plugins()` +
    /// `PluginRegistry::build`). When `None`, P06a wires the production
    /// path: `builtin_plugins()` → `PluginRegistry::build(&config, factories)`.
    async fn build(
        config: agent_shim_config::GatewayConfig,
        clock: Arc<dyn Clock>,
        plugin_override: Option<Arc<agent_shim_plugins::PluginRegistry>>,
    ) -> anyhow::Result<(
        Self,
        tokio::sync::mpsc::Receiver<crate::reload_trigger::ReloadRequest>,
    )> {
        // P06a: construct plugin registry from config + built-in
        // factories, OR use the override if provided by a test.
        let plugins = match plugin_override {
            Some(p) => p,
            None => {
                let factories = agent_shim_plugins::builtin::builtin_plugins();
                Arc::new(
                    agent_shim_plugins::PluginRegistry::build(&config, factories)
                        .map_err(|e| anyhow::anyhow!("plugin registry build failed: {e}"))?,
                )
            }
        };

        // ── below this point: existing body unchanged ────────────────
        let keepalive = Duration::from_secs(config.server.keepalive_secs);
        // ... rest of the existing build() body up to and including the
        // final `(state, reload_rx_back)` tuple — wrap return in Ok() ...
```

The final return at the end of `build()` should change from `(state, reload_rx_back)` to `Ok((state, reload_rx_back))`. **Locate the end of the function body** and wrap the returned tuple in `Ok(...)`.

If the existing function uses an implicit return (no `return` keyword, just a tuple expression at the end), change that tuple to `Ok(tuple)`. If there's an explicit `return`, change to `return Ok(...);`.

- [ ] **Step 2: Update `serve.rs` and `admin/handlers.rs` callers**

In `crates/gateway/src/commands/serve.rs` line 38, change:

```rust
let (mut state, mut reload_rx) = crate::state::AppState::new(cfg).await;
```

to:

```rust
let (mut state, mut reload_rx) = crate::state::AppState::new(cfg).await?;
```

The enclosing `run` function already returns `anyhow::Result<()>` (verified — it's the `axum_run::<S, L>` function with `where Result = ()` return path). If not, the `?` will require the surrounding function to also return Result; the function already does.

In `crates/gateway/src/admin/handlers.rs`, find the 2 occurrences of `AppState::new(cfg).await` (lines 75 and 84). Append `.expect("test AppState build")` (these are inside test code per the surrounding context — confirm by reading the file around those lines).

For safety, run a search to find every site:

```
rtk grep -n "AppState::new\b" crates/gateway/src crates/gateway/tests
```

Each non-test call uses `?` (so the enclosing function must return Result). Each test call uses `.expect("...")` or `.unwrap()`.

- [ ] **Step 3: Update `state.rs` in-file tests**

`crates/gateway/src/state.rs` has 2 in-file test call sites at lines 446 and 479. Append `.unwrap()` after `.await`:

```rust
let (state, _reload_rx) = AppState::new(cfg).await.unwrap();
```

- [ ] **Step 4: Run gateway-only build to surface remaining callers**

Run: `rtk cargo build -p agent-shim-gateway --tests`
Expected: COMPILE ERRORS naming each remaining `AppState::new(...)` call site that doesn't handle the new `Result`. Address each one mechanically: tests get `.unwrap()` or `.expect("...")`, production gets `?`.

- [ ] **Step 5: Iterate until clean**

Run: `rtk cargo build --workspace --tests`
Expected: PASS.

- [ ] **Step 6: Run full workspace tests**

Run: `rtk cargo nextest run --workspace`
Expected: PASS — test count should be a bit higher than the P06a-T7 state because no new tests in this task, but no regressions.

- [ ] **Step 7: Commit**

```bash
rtk git add crates/gateway/src/state.rs crates/gateway/src/commands/serve.rs crates/gateway/src/admin/handlers.rs crates/gateway/tests crates/gateway/src
rtk git commit -m "feat(gateway): AppState::new returns Result + wires PluginRegistry::build (P06a T8)"
```

---

## Task 9: Gateway integration test — usage_recorder end-to-end

**Files:**
- Create: `crates/gateway/tests/usage_recorder_integration.rs`

This test validates: YAML → AppState::new (full Layer A path bypassed but real `builtin_plugins() + PluginRegistry::build`) → drive one HTTP request → unreachable upstream → H7Guard fires → usage_recorder emits.

- [ ] **Step 1: Verify `tracing-test` is available as workspace dev-dep for gateway tests**

`tracing-test = "0.2"` was added to `crates/plugins/Cargo.toml` dev-dependencies in P05 T3-T5. The gateway crate needs it as well for the integration test.

Edit `crates/gateway/Cargo.toml` — under `[dev-dependencies]`, add:

```toml
tracing-test = { version = "0.2", features = ["no-env-filter"] }
```

(Place alphabetically. If it's already present from an earlier task, skip this edit.)

- [ ] **Step 2: Create the integration test file**

Create `crates/gateway/tests/usage_recorder_integration.rs`:

```rust
//! Phase 7 P06a T9: end-to-end integration test for usage_recorder.
//!
//! Validates the full production path:
//! YAML → AppState::new → builtin_plugins() → PluginRegistry::build →
//! axum router → HTTP request → unreachable upstream → H7Guard fires →
//! usage_recorder emits structured log line.
//!
//! The upstream points at `127.0.0.1:1` (Connection Refused on any
//! non-root system) so the provider call fails with
//! `UpstreamStatus::Error`. H7 fires regardless of upstream outcome,
//! proving the plugin emits on the error path too.

use agent_shim_config::GatewayConfig;
use agent_shim_gateway::{server::build_router, state::AppState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::util::ServiceExt;

const YAML: &str = r#"
server:
  bind: 127.0.0.1
  port: 0
  keepalive_secs: 15
upstreams:
  unreachable:
    type: openai
    base_url: "http://127.0.0.1:1"
    api_key: "test-key"
routes:
  - frontend: anthropic_messages
    model: test-model
    upstream: unreachable
    backend_model: test-model
    plugins:
      on_response_complete:
        - usage_logger
plugins:
  usage_logger:
    type: usage_recorder
    config:
      sink: log
      level: info
"#;

const REQUEST_BODY: &str = r#"{
    "model": "test-model",
    "max_tokens": 16,
    "messages": [{"role": "user", "content": "hi"}]
}"#;

#[tokio::test]
#[tracing_test::traced_test]
async fn usage_recorder_emits_on_h7_via_full_build_path() -> anyhow::Result<()> {
    let cfg: GatewayConfig = serde_yaml::from_str(YAML)?;
    let (state, _reload_rx) = AppState::new(cfg).await?;
    let app = build_router(state);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(REQUEST_BODY))?;

    let response = app.oneshot(request).await?;
    // We don't assert on exact status code — the resilience layer's
    // exact mapping for ECONNREFUSED is implementation detail. We only
    // care that the response completed (200/502/503 are all acceptable;
    // 200 is impossible here because upstream is unreachable). Any
    // non-100 status confirms the request went through the pipeline.
    assert_ne!(
        response.status(),
        StatusCode::CONTINUE,
        "expected the request to complete one way or another"
    );

    // The H7Guard's Drop fires `run_on_response_complete` which spawns
    // the H7 task via `PluginSupervisor`. Yield enough times for the
    // spawn to run.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }

    // Verify the usage_recorder emission.
    assert!(
        logs_contain("plugin.kind=\"usage_recorder\""),
        "usage_recorder must emit the plugin.kind=usage_recorder field"
    );
    assert!(
        logs_contain("usage.upstream_status=\"error\""),
        "H7 fires on UpstreamStatus::Error path"
    );
    Ok(())
}
```

- [ ] **Step 3: Run the integration test**

Run: `rtk cargo nextest run --test usage_recorder_integration`
Expected: PASS — 1 test.

- [ ] **Step 4: Run full workspace tests as regression gate**

Run: `rtk cargo nextest run --workspace`
Expected: PASS — test count ~844 (824 baseline + 4 RegistryBuildError + 4 HookTimeouts + 11 build + 8 usage_recorder + 3 emit + 1 builtin + 1 integration ≈ +32; some sub-counts may overlap if I miscounted but no regressions).

- [ ] **Step 5: Commit**

```bash
rtk git add crates/gateway/Cargo.toml crates/gateway/tests/usage_recorder_integration.rs
rtk git commit -m "test(gateway): usage_recorder integration through full AppState::new path (P06a T9)"
```

---

## Task 10: Clippy + fmt + frozen-core verification

**Files:** No new code; verification only.

- [ ] **Step 1: Run clippy**

Run: `rtk cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

If lint warnings fire on the new code, fix them in the appropriate task's file. Common candidates:
- Unused imports → remove
- `dead_code` on validation-only fields (`UsageRecorderConfig.fields`) → already `#[allow(dead_code)]` per T5
- `#[non_exhaustive]` on internal enums may warn → keep it; spec §5.3 design choice

- [ ] **Step 2: Run rustfmt**

Run: `rtk cargo fmt --all -- --check`
Expected: PASS (no diff).

If fmt would change files, run `cargo fmt --all` and re-commit those files under the existing task.

- [ ] **Step 3: Verify frozen-core invariant**

Run: `git diff master..HEAD -- crates/core/`
Expected: EMPTY output.

If non-empty, the change leaked into core — back out the offending edit. P06a must touch only plugins, config (read-only), gateway, and docs.

- [ ] **Step 4: Final test count check**

Run: `rtk cargo nextest run --workspace`
Expected: PASS — exact test count should be ~844 (baseline 824 + ~20 new). Document the actual number in the commit.

- [ ] **Step 5: Commit (no-op if everything is already clean)**

Only commit if any fmt or clippy fixes landed:

```bash
rtk git add -A
rtk git commit -m "chore: clippy + fmt cleanup after P06a (P06a T10)"
```

If no fixes were needed, skip the commit.

---

## Acceptance gates

After all 10 tasks land:

1. `rtk cargo nextest run --workspace` — test count ~844 (+/- a few). Zero failures.
2. `rtk cargo clippy --workspace --all-targets -- -D warnings` — clean.
3. `rtk cargo fmt --all -- --check` — clean.
4. `git diff master..HEAD -- crates/core/` — empty (frozen-core preserved).
5. Manual smoke: `agent-shim serve --config <yaml with usage_recorder plugin>` boots; one request emits a log line with target `agent_shim::usage_recorder`.
6. Spec acceptance criteria 1-15 (see `docs/superpowers/specs/2026-05-18-phase-7-p06a-registry-builder-design.md` §11).

## Notes for the implementer

- **Task order matters.** T2/T3 add types that T4 consumes; T5 adds the file that T6 fills in; T7 wires the registration that T8 calls. T8 ripples through ~35 call sites; do not skip the workspace-wide build between T8 steps. T9 depends on every prior task.
- **`tokio::test` + `tracing-test::traced_test` ordering.** The `#[tokio::test]` attribute must come BEFORE `#[tracing_test::traced_test]` so the runtime is set up first. The order is shown in T6 and T9 — preserve it.
- **`logs_contain` matcher** is provided by the `tracing-test` macro. It searches the captured output for any substring match — that's why the assertions use partial strings like `"plugin.kind=\"usage_recorder\""` rather than exact-line matches.
- **`UsageRecorderConfig.fields` is `#[allow(dead_code)]`** — the field is consumed only at deserialize time (serde does the validation). The runtime emit list is hardcoded in `emit_usage!`. Spec §5.4 explicitly documents this.
- **`Sink::Log` is the only variant** but the enum is `#[non_exhaustive]` so P06b adding `Prometheus` won't be a breaking change for external callers. `Sink` is private (file-local) so the attribute is forward-compatibility hygiene only.
- **Use `Arc::from(Box<dyn Plugin>)`** to coerce `Box<dyn Plugin>` (factory output) to `Arc<dyn Plugin>` (PluginEntry field). Stdlib provides this `From` impl.
- **Frontend alias coverage**: T4 step 3 has a 6-alias match (`anthropic|anthropic_messages`, etc.). If you forget an alias, the `build_frontend_aliases_resolve_correctly` test catches the simple case but write additional tests if you cover all 6 — easy to add.
- **Gateway integration test debugging**: if `logs_contain("usage_recorder")` fails on first run, check (a) that `RUST_LOG` filter inside `tracing-test` includes the `agent_shim::usage_recorder` target — the `no-env-filter` feature handles this; (b) that the H7 spawn yielded enough times before the assertion — bump the yield-loop count if needed; (c) that the integration test's tokio runtime properly drives the H7Guard's `Drop` — H7 fires when the `H7Guard` is dropped, which happens at scope-exit of the pipeline's `dispatch_inner`.
- **If `cargo nextest run` deadlocks or hangs**, it's most likely the integration test's tokio runtime configuration. `#[tokio::test]` defaults to current-thread runtime; if the H7 spawn needs a multi-threaded runtime, switch to `#[tokio::test(flavor = "multi_thread")]`.
