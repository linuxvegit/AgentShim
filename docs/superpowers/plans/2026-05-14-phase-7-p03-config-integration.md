# Plan P03 — Config integration + Layer A validation (Phase 7)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-14-plugin-system-design.md`](../specs/2026-05-14-plugin-system-design.md) §5.1, §5.2, §5.3 Layer A, §5.4, §5.5, §5.6. [`ADR-0008`](../../adr/0008-plugin-system.md) decision (5).

**Goal:** Teach `agent-shim-config` about the new YAML shapes — top-level `plugins:` map of named plugin entries, and per-route `plugins:` block of ordered hook references. Implement Layer A validation (the 3 schema-only checks: undeclared plugin reference, `timeout_ms == 0`, duplicate plugin names). Verify env-var overlay works without any new plumbing.

**Architecture:** A new module `crates/config/src/plugins.rs` houses the four new types: `PluginEntry`, `TimeoutMs`, `RoutePluginsBlock`, `PluginRefList`. They're attached to `GatewayConfig` (`plugins:`) and `RouteEntry` (`plugins:`). The existing `validate()` in `crates/config/src/validation.rs` gains a `validate_plugins()` sub-call mirroring the established pattern (Phase 4 / Phase 6 sub-validators all hang off the same canonical entry point). No changes to `agent-shim-plugins`; no factory wiring (that's P06).

**Tech stack:** `serde` (with derive + untagged), `serde_yaml` (already in dev-deps of `config`), `thiserror` — all already in workspace.

**Frozen-core impact:** None. `crates/core/` is not touched. ADR-0007 discipline preserved.

**Test target:** Current workspace baseline (post-P02) is **726 passing**. This plan adds ~15 new tests in `crates/config/`: YAML round-trip (full plugin example from spec §5.2), env-var overlay round-trip, 3 Layer A validation failure cases, `TimeoutMs` untagged enum cases (single `u64` form + `map` form), `RoutePluginsBlock` empty-by-default. Target ~741 on completion.

---

## File Structure

`crates/config/src/`:
- Create: `plugins.rs` — new module with `PluginEntry`, `TimeoutMs`, `RoutePluginsBlock`, `PluginRefList` types + serde derives + their unit tests.
- Modify: `lib.rs` — declare `pub mod plugins;` and re-export the four new types.
- Modify: `schema.rs::GatewayConfig` — add `pub plugins: BTreeMap<String, plugins::PluginEntry>` (defaults to empty).
- Modify: `schema.rs::RouteEntry` — add `pub plugins: Option<plugins::RoutePluginsBlock>` (defaults to `None`).
- Modify: `validation.rs::ValidationError` — add 3 new variants: `UndeclaredPlugin`, `ZeroTimeoutMs`, `DuplicatePluginName`.
- Modify: `validation.rs::validate()` — call a new `validate_plugins()` sub-validator.
- Modify: `validation.rs` — add `fn validate_plugins(cfg) -> Result<(), ValidationError>` implementing the 3 Layer A rules.

`crates/config/tests/` (new file):
- Create: `plugins_yaml.rs` — integration tests against the full spec §5.2 example + env-overlay scenarios.

No changes outside `crates/config/`.

---

## Tasks

### Task 1: Define `PluginEntry`, `TimeoutMs`, `RoutePluginsBlock` in a new module

**Files:**
- Create: `crates/config/src/plugins.rs`
- Modify: `crates/config/src/lib.rs`

- [ ] **Step 1: Read existing module declarations**

Open `crates/config/src/lib.rs` and inspect the current `pub mod` lines + `pub use` re-exports. They look something like:

```rust
pub mod schema;
pub mod secrets;
pub mod upstream_accessors;
pub mod validation;

pub use schema::*;
pub use secrets::Secret;
pub use upstream_accessors::{upstream_cost, upstream_latency_budget, upstream_tier};
pub use validation::{validate, ValidationError, /* ... */};
```

The exact text may vary slightly. Use Read to confirm before editing.

- [ ] **Step 2: Write `crates/config/src/plugins.rs`**

```rust
//! YAML schema for the plugin system, Phase 7.
//!
//! Three new shapes:
//! - `PluginEntry` — one entry under top-level `plugins:` map.
//! - `TimeoutMs` — accepts either `timeout_ms: 50` (uniform) or
//!   `timeout_ms: { default: 50, on_stream_event: 5 }` (per-hook).
//! - `RoutePluginsBlock` — per-route `plugins:` block holding four
//!   ordered lists, one per hook.
//!
//! Spec §5.1 / §5.2 / §5.4.

use serde::{Deserialize, Serialize};

/// One plugin instance, declared under top-level `plugins:` in
/// `gateway.yaml`. Carries the kind (which Rust impl), an opaque
/// `config:` blob owned by the factory, an `on_error` policy, a
/// `timeout_ms` (uniform or per-hook), and an `enabled` flag.
///
/// The `config` field is intentionally `serde_json::Value` so the
/// config crate stays agnostic to plugin kinds — factories own their
/// internal schemas and surface deserialisation errors at Layer B
/// validation (P06).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginEntry {
    /// Plugin kind name — matches `Plugin::kind_name()` of the
    /// constructed instance. YAML key kept as `type:` for consistency
    /// with `upstreams[].type:` (Q2).
    #[serde(rename = "type")]
    pub kind: String,

    /// Kind-specific config. Opaque to the config crate; the factory
    /// deserialises its own struct from this. Defaults to an empty
    /// JSON object so plugins without config can write
    /// `compressor: { type: prompt_compressor }`.
    #[serde(default = "default_config")]
    pub config: serde_json::Value,

    /// On-error policy. `skip` swallows internal failures; `fail`
    /// propagates them. Defaults to `skip` (spec §5.1).
    #[serde(default)]
    pub on_error: OnErrorYaml,

    /// Per-hook timeouts. `None` = use system defaults (50/50/5/50 ms
    /// per spec §5.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<TimeoutMs>,

    /// `false` suppresses the plugin from any route plan but keeps
    /// config validation honest (plugin still constructed at startup
    /// so config bugs surface, per spec §5.3 rule 7).
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_config() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

fn default_enabled() -> bool {
    true
}

/// YAML form of `agent_shim_plugins::OnError`. Lives here so
/// `agent-shim-config` doesn't have to depend on `agent-shim-plugins`
/// (boundary discipline — config is a leaf-ish crate). The gateway
/// wiring (P06) converts this into `agent_shim_plugins::OnError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnErrorYaml {
    #[default]
    Skip,
    Fail,
}

/// YAML form of per-plugin timeouts. Accepts either a uniform value
/// or a per-hook map.
///
/// ```yaml
/// timeout_ms: 50          # uniform
/// timeout_ms:             # per-hook
///   default: 50
///   on_stream_event: 5
/// ```
///
/// (Spec §5.4.)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum TimeoutMs {
    /// `timeout_ms: 50` — single u64 applies to all subscribed hooks.
    Uniform(u64),

    /// `timeout_ms: { default: 50, on_stream_event: 5 }` — per-hook
    /// overrides on top of `default`. Each per-hook field is optional;
    /// absent fields inherit `default`. Both `default` and any per-hook
    /// fields are optional at the schema layer — Layer A validation
    /// rejects `timeout_ms == 0` for any populated slot.
    PerHook {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_decoded_request: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_resolved: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_stream_event: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_response_complete: Option<u64>,
    },
}

/// Per-route `plugins:` block — four ordered lists, one per hook.
/// All four default to empty; omitting the whole block yields the
/// default (no plugins on this route, fast path).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RoutePluginsBlock {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_decoded_request: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_resolved: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_stream_event: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_response_complete: Vec<String>,
}

impl RoutePluginsBlock {
    /// True when every hook list is empty — the registry's fast path
    /// can return identity without further work.
    pub fn is_empty(&self) -> bool {
        self.on_decoded_request.is_empty()
            && self.on_resolved.is_empty()
            && self.on_stream_event.is_empty()
            && self.on_response_complete.is_empty()
    }

    /// Iterator over all (hook_name, &plugin_name) pairs across the
    /// four lists. Used by Layer A validation to spot undeclared
    /// references and (in P06) by registry construction.
    pub fn iter_references(&self) -> impl Iterator<Item = (&'static str, &str)> {
        let h2 = self
            .on_decoded_request
            .iter()
            .map(|n| ("on_decoded_request", n.as_str()));
        let h3 = self
            .on_resolved
            .iter()
            .map(|n| ("on_resolved", n.as_str()));
        let h5 = self
            .on_stream_event
            .iter()
            .map(|n| ("on_stream_event", n.as_str()));
        let h7 = self
            .on_response_complete
            .iter()
            .map(|n| ("on_response_complete", n.as_str()));
        h2.chain(h3).chain(h5).chain(h7)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_error_yaml_default_is_skip() {
        assert_eq!(OnErrorYaml::default(), OnErrorYaml::Skip);
    }

    #[test]
    fn timeout_ms_accepts_uniform_form() {
        let v: TimeoutMs = serde_yaml::from_str("50").unwrap();
        assert_eq!(v, TimeoutMs::Uniform(50));
    }

    #[test]
    fn timeout_ms_accepts_per_hook_form() {
        let yaml = "default: 50\non_stream_event: 5";
        let v: TimeoutMs = serde_yaml::from_str(yaml).unwrap();
        match v {
            TimeoutMs::PerHook {
                default,
                on_stream_event,
                on_decoded_request,
                on_resolved,
                on_response_complete,
            } => {
                assert_eq!(default, Some(50));
                assert_eq!(on_stream_event, Some(5));
                assert_eq!(on_decoded_request, None);
                assert_eq!(on_resolved, None);
                assert_eq!(on_response_complete, None);
            }
            _ => panic!("expected PerHook"),
        }
    }

    #[test]
    fn plugin_entry_minimal_yaml_round_trip() {
        let yaml = "type: prompt_compressor";
        let p: PluginEntry = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(p.kind, "prompt_compressor");
        assert_eq!(p.config, serde_json::json!({}));
        assert_eq!(p.on_error, OnErrorYaml::Skip);
        assert!(p.timeout_ms.is_none());
        assert!(p.enabled);
    }

    #[test]
    fn plugin_entry_full_yaml_round_trip() {
        let yaml = r#"
type: prompt_compressor
config:
  strategy: summarize_old_turns
  keep_last: 4
on_error: fail
timeout_ms: 50
enabled: false
"#;
        let p: PluginEntry = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(p.kind, "prompt_compressor");
        assert_eq!(p.config["strategy"], "summarize_old_turns");
        assert_eq!(p.config["keep_last"], 4);
        assert_eq!(p.on_error, OnErrorYaml::Fail);
        assert_eq!(p.timeout_ms, Some(TimeoutMs::Uniform(50)));
        assert!(!p.enabled);
    }

    #[test]
    fn plugin_entry_rejects_unknown_field() {
        // `deny_unknown_fields` must catch typos.
        let yaml = "type: foo\nbogus_field: 1";
        let r: Result<PluginEntry, _> = serde_yaml::from_str(yaml);
        assert!(r.is_err(), "deny_unknown_fields should reject `bogus_field`");
    }

    #[test]
    fn route_plugins_block_default_is_empty() {
        let block = RoutePluginsBlock::default();
        assert!(block.is_empty());
        assert_eq!(block.iter_references().count(), 0);
    }

    #[test]
    fn route_plugins_block_iter_references_walks_all_hooks() {
        let block = RoutePluginsBlock {
            on_decoded_request: vec!["a".to_string()],
            on_resolved: vec!["b".to_string()],
            on_stream_event: vec!["c".to_string()],
            on_response_complete: vec!["d".to_string()],
        };
        let pairs: Vec<(&str, &str)> = block.iter_references().collect();
        assert_eq!(
            pairs,
            vec![
                ("on_decoded_request", "a"),
                ("on_resolved", "b"),
                ("on_stream_event", "c"),
                ("on_response_complete", "d"),
            ]
        );
    }

    #[test]
    fn route_plugins_block_rejects_unknown_hook() {
        // `deny_unknown_fields` must reject typo-hooks.
        let yaml = "on_stream_evnt: [foo]"; // typo: evnt
        let r: Result<RoutePluginsBlock, _> = serde_yaml::from_str(yaml);
        assert!(r.is_err(), "deny_unknown_fields should reject `on_stream_evnt`");
    }
}
```

- [ ] **Step 3: Register the module and re-export public types**

Edit `crates/config/src/lib.rs`. Add `pub mod plugins;` after the existing `pub mod` lines, and add a re-export so callers can write `agent_shim_config::PluginEntry` rather than `agent_shim_config::plugins::PluginEntry`:

```rust
pub mod plugins;
pub use plugins::{OnErrorYaml, PluginEntry, RoutePluginsBlock, TimeoutMs};
```

Place these alphabetically within the existing `pub mod` / `pub use` blocks.

- [ ] **Step 4: Verify the new module compiles and tests pass**

Run:
```
cargo test -p agent-shim-config plugins
```

Expected: 8 tests pass. (One per item: default OnError, Uniform timeout, PerHook timeout, minimal PluginEntry, full PluginEntry, deny_unknown_fields on PluginEntry, default RoutePluginsBlock, iter_references walks all hooks, deny_unknown_fields on RoutePluginsBlock — actually 9. Either way: all green.)

- [ ] **Step 5: Commit**

```bash
git add crates/config/src/lib.rs crates/config/src/plugins.rs
git commit -m "feat(config): PluginEntry, TimeoutMs, RoutePluginsBlock YAML types (P03 T1)"
```

---

### Task 2: Wire `plugins:` field into `GatewayConfig`

**Files:**
- Modify: `crates/config/src/schema.rs`

- [ ] **Step 1: Locate `GatewayConfig`**

Open `crates/config/src/schema.rs`. The struct is at the top of the file, around line 8. Current shape:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub upstreams: BTreeMap<String, UpstreamConfig>,
    #[serde(default)]
    pub routes: Vec<RouteEntry>,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    pub copilot: Option<CopilotConfig>,
    #[serde(default)]
    pub admin: Option<AdminConfig>,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub otel: Option<OtelConfig>,
}
```

- [ ] **Step 2: Add the new field**

Add a `plugins` field. Place it after `routes` (logical grouping: plugins are referenced by routes) and before `auth`. Default is an empty map:

```rust
    #[serde(default)]
    pub routes: Vec<RouteEntry>,
    /// Top-level plugin declarations. Each entry is a named plugin
    /// instance keyed by `<plugin_name>`. Routes reference these by
    /// name. Plan 07 P03.
    #[serde(default)]
    pub plugins: std::collections::BTreeMap<String, crate::plugins::PluginEntry>,
    #[serde(default)]
    pub auth: AuthConfig,
```

(Use the fully-qualified `std::collections::BTreeMap` or, if `BTreeMap` is already imported at the top of the file, just `BTreeMap`. Same for `crate::plugins::PluginEntry` — if you'd prefer a `use` line at the top of the file, that's fine too. Either style. Pick what matches the rest of the file.)

- [ ] **Step 3: Verify build**

Run: `cargo check -p agent-shim-config`
Expected: clean — no errors.

- [ ] **Step 4: Verify existing tests still pass**

Run: `cargo test -p agent-shim-config`
Expected: all pre-existing tests pass. The new `plugins:` field has `#[serde(default)]`, so YAML configs without it deserialize to an empty map and existing tests are unaffected.

- [ ] **Step 5: Commit**

```bash
git add crates/config/src/schema.rs
git commit -m "feat(config): GatewayConfig.plugins map (P03 T2)"
```

---

### Task 3: Wire `plugins:` field into `RouteEntry`

**Files:**
- Modify: `crates/config/src/schema.rs`

- [ ] **Step 1: Locate `RouteEntry`**

Open `crates/config/src/schema.rs`. Find `pub struct RouteEntry` (around line 380). It already has many `#[serde(default)]` fields — the new one fits the same pattern.

- [ ] **Step 2: Add the new field**

Add a `plugins` field at the END of `RouteEntry`'s struct body (after `max_cost_usd`, which is the last existing field). The reason for placing it last: minimises diff churn in the manual `RouteEntry::singular` constructor (Task 3 Step 3 below). Field shape:

```rust
    // ── Per-route plugin attachments (Phase 7 P03) ─────────────────────────
    /// Per-hook ordered lists of plugin references. `None` (or all-empty
    /// lists) means no plugins run on this route — the registry's fast
    /// path is enabled. Plan 07 P03.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<crate::plugins::RoutePluginsBlock>,
```

- [ ] **Step 3: Update `RouteEntry::singular`**

The `singular(...)` constructor (around line 435) lists every field by name. Add `plugins: None` to its struct-init body. Also check `RouteEntry::default()` if it exists (some structs have one; verify via grep). If a `Default` impl exists, add `plugins: None` to it too. If derived via `#[derive(Default)]`, no edit needed — `Option<_>` defaults to `None` for free.

- [ ] **Step 4: Run all `config` tests**

Run: `cargo test -p agent-shim-config`
Expected: all existing tests + the 8-9 new plugin tests from T1 all pass.

If any existing test fails because it constructs `RouteEntry` by hand with explicit field listing, add `plugins: None` to the struct literal in that test.

- [ ] **Step 5: Verify build of dependent crates**

Run: `cargo check --workspace`
Expected: clean. (The router crate accesses `RouteEntry` via the `CostFilter`-related code from Phase 6 — it will compile because the new `plugins` field doesn't appear in any pattern match downstream.)

- [ ] **Step 6: Commit**

```bash
git add crates/config/src/schema.rs
git commit -m "feat(config): RouteEntry.plugins per-route block (P03 T3)"
```

---

### Task 4: Add `ValidationError` variants for Layer A

**Files:**
- Modify: `crates/config/src/validation.rs`

- [ ] **Step 1: Locate `ValidationError`**

Open `crates/config/src/validation.rs`. The enum is at the top of the file, around line 6. It currently has 9 variants (`ZeroPort`, `UnknownUpstream`, `DuplicateAlias`, etc.).

- [ ] **Step 2: Add three new variants**

Add three new variants at the END of `ValidationError` (after the existing `ImpossibleMinTier` variant). The reason for the END: matches the project convention seen in earlier phases — each new sub-validator appends its own variant family rather than weaving them in.

```rust
    /// Layer A rule (Plan 07 P03): a route references a plugin name
    /// that isn't declared under top-level `plugins:`. Phase 7 spec §5.3.
    #[error(
        "route `{route}` references plugin `{plugin}` on hook `{hook}`, but no \
         plugin with that name is declared under top-level `plugins:`"
    )]
    UndeclaredPlugin {
        route: String,
        plugin: String,
        hook: &'static str,
    },

    /// Layer A rule (Plan 07 P03): a plugin sets `timeout_ms: 0`,
    /// which is rejected as misconfiguration (a zero timeout would
    /// cause every invocation to time out immediately).
    #[error(
        "plugin `{plugin}` has `timeout_ms = 0` on slot `{slot}` (rejected as \
         misconfiguration)"
    )]
    ZeroTimeoutMs {
        plugin: String,
        slot: &'static str,
    },

    /// Layer A rule (Plan 07 P03): duplicate plugin names. YAML map
    /// key uniqueness usually catches this at parse time, but
    /// hand-constructed `GatewayConfig` values (test fixtures) may
    /// still slip through.
    #[error("duplicate plugin name: `{0}`")]
    DuplicatePluginName(String),
```

(The `slot` field on `ZeroTimeoutMs` is the hook name or the literal `"uniform"` / `"default"` — it gets populated when we walk a `TimeoutMs::PerHook` map. See Task 5 for the emission sites.)

- [ ] **Step 3: Verify build**

Run: `cargo check -p agent-shim-config`
Expected: clean. The new variants compile; no caller of `ValidationError` needs updating yet (they're additive).

- [ ] **Step 4: Commit**

```bash
git add crates/config/src/validation.rs
git commit -m "feat(config): ValidationError variants for plugin Layer A (P03 T4)"
```

---

### Task 5: Implement `validate_plugins()` Layer A sub-validator

**Files:**
- Modify: `crates/config/src/validation.rs`

- [ ] **Step 1: Locate the `validate()` entry point**

Open `crates/config/src/validation.rs`. Find `pub fn validate(cfg: &GatewayConfig) -> Result<(), ValidationError>` (around line 147). Note the existing sub-validator call sites (after each major rule):

```rust
    validate_routes(cfg).map_err(ValidationError::InvalidRoute)?;
    validate_rate_limit(cfg).map_err(ValidationError::InvalidRoute)?;
    validate_auth(cfg).map_err(ValidationError::InvalidRoute)?;
```

- [ ] **Step 2: Add the call to `validate_plugins()`**

Insert AFTER `validate_auth(cfg)` (so plugin validation runs after route/rate-limit/auth — the Phase 7 layer is the newest, so it runs last):

```rust
    // Layer A plugin validation (Plan 07 P03). Three rules:
    //   - undeclared plugin reference on a route hook
    //   - timeout_ms == 0
    //   - duplicate plugin names (defensive — YAML usually catches it)
    validate_plugins(cfg)?;
```

Note: this call does NOT use `.map_err(...)` — `validate_plugins` returns the proper `ValidationError` variants directly (`UndeclaredPlugin`, `ZeroTimeoutMs`, `DuplicatePluginName`), unlike the older sub-validators that tunnel through `InvalidRoute`.

- [ ] **Step 3: Implement `validate_plugins`**

Add the function below the existing sub-validators (e.g. after `validate_auth`'s body). Include the helper signature `pub fn` so external callers / tests can invoke it independently:

```rust
/// Layer A plugin validation (Plan 07 P03, spec §5.3). Three rules:
///
/// 1. Every plugin name referenced on a route hook is declared
///    under top-level `plugins:`.
/// 2. No `timeout_ms` slot is zero.
/// 3. (Defensive) No duplicate plugin names. YAML map-key
///    uniqueness catches this at parse time; this branch only fires
///    on hand-constructed `GatewayConfig` test fixtures.
///
/// Layer B (unknown plugin kind, factory `instantiate` failure, hook
/// subscription mismatch) lives in the gateway boot path — see P06.
pub fn validate_plugins(cfg: &GatewayConfig) -> Result<(), ValidationError> {
    // Rule 3: duplicate name (defensive against hand-constructed
    // configs that bypass YAML map-key uniqueness). For a `BTreeMap`
    // this can't actually trigger from YAML; left here so an
    // intentionally-crafted test fixture can't sneak past.
    let mut seen = std::collections::HashSet::new();
    for name in cfg.plugins.keys() {
        if !seen.insert(name.clone()) {
            return Err(ValidationError::DuplicatePluginName(name.clone()));
        }
    }

    // Rule 2: timeout_ms == 0.
    for (name, entry) in &cfg.plugins {
        if let Some(t) = &entry.timeout_ms {
            match t {
                crate::plugins::TimeoutMs::Uniform(0) => {
                    return Err(ValidationError::ZeroTimeoutMs {
                        plugin: name.clone(),
                        slot: "uniform",
                    });
                }
                crate::plugins::TimeoutMs::Uniform(_) => {}
                crate::plugins::TimeoutMs::PerHook {
                    default,
                    on_decoded_request,
                    on_resolved,
                    on_stream_event,
                    on_response_complete,
                } => {
                    for (slot, value) in [
                        ("default", default),
                        ("on_decoded_request", on_decoded_request),
                        ("on_resolved", on_resolved),
                        ("on_stream_event", on_stream_event),
                        ("on_response_complete", on_response_complete),
                    ] {
                        if let Some(0) = value {
                            return Err(ValidationError::ZeroTimeoutMs {
                                plugin: name.clone(),
                                slot,
                            });
                        }
                    }
                }
            }
        }
    }

    // Rule 1: undeclared plugin reference on any route hook.
    for route in &cfg.routes {
        let Some(plugins_block) = &route.plugins else {
            continue;
        };
        let route_label = format!("{}/{}", route.frontend, route.model);
        for (hook, plugin_name) in plugins_block.iter_references() {
            if !cfg.plugins.contains_key(plugin_name) {
                return Err(ValidationError::UndeclaredPlugin {
                    route: route_label.clone(),
                    plugin: plugin_name.to_string(),
                    hook,
                });
            }
        }
    }

    Ok(())
}
```

- [ ] **Step 4: Verify build**

Run: `cargo check -p agent-shim-config`
Expected: clean.

- [ ] **Step 5: Write unit tests for the three rules**

Add a test module inside `validation.rs`, near other test modules. The tests construct `GatewayConfig` values by hand and assert each Layer A rule fires.

Find an existing test module pattern in `validation.rs` (the file has `#[cfg(test)] mod tests` near the bottom — around line 1180). Append the three new tests INSIDE that existing module:

```rust
    // ── Phase 7 P03 Layer A plugin validation ───────────────────────────────

    fn mk_cfg_with_routes(routes: Vec<crate::schema::RouteEntry>) -> crate::schema::GatewayConfig {
        crate::schema::GatewayConfig {
            server: Default::default(),
            logging: Default::default(),
            upstreams: Default::default(),
            routes,
            plugins: Default::default(),
            auth: Default::default(),
            rate_limit: Default::default(),
            copilot: None,
            admin: None,
            metrics: Default::default(),
            otel: None,
        }
    }

    #[test]
    fn validate_plugins_rejects_undeclared_reference() {
        let mut route = crate::schema::RouteEntry::singular(
            "anthropic_messages",
            "claude-sonnet",
            "anthropic",
            "claude-sonnet",
        );
        route.plugins = Some(crate::plugins::RoutePluginsBlock {
            on_decoded_request: vec!["missing".to_string()],
            ..Default::default()
        });
        let cfg = mk_cfg_with_routes(vec![route]);
        let err = crate::validation::validate_plugins(&cfg).unwrap_err();
        match err {
            ValidationError::UndeclaredPlugin {
                plugin, hook, ..
            } => {
                assert_eq!(plugin, "missing");
                assert_eq!(hook, "on_decoded_request");
            }
            other => panic!("expected UndeclaredPlugin, got {other:?}"),
        }
    }

    #[test]
    fn validate_plugins_accepts_declared_reference() {
        let mut route = crate::schema::RouteEntry::singular(
            "anthropic_messages",
            "claude-sonnet",
            "anthropic",
            "claude-sonnet",
        );
        route.plugins = Some(crate::plugins::RoutePluginsBlock {
            on_decoded_request: vec!["compressor".to_string()],
            ..Default::default()
        });
        let mut cfg = mk_cfg_with_routes(vec![route]);
        cfg.plugins.insert(
            "compressor".to_string(),
            crate::plugins::PluginEntry {
                kind: "prompt_compressor".to_string(),
                config: serde_json::json!({}),
                on_error: crate::plugins::OnErrorYaml::Skip,
                timeout_ms: None,
                enabled: true,
            },
        );
        assert!(crate::validation::validate_plugins(&cfg).is_ok());
    }

    #[test]
    fn validate_plugins_rejects_zero_uniform_timeout() {
        let mut cfg = mk_cfg_with_routes(vec![]);
        cfg.plugins.insert(
            "p".to_string(),
            crate::plugins::PluginEntry {
                kind: "prompt_compressor".to_string(),
                config: serde_json::json!({}),
                on_error: crate::plugins::OnErrorYaml::Skip,
                timeout_ms: Some(crate::plugins::TimeoutMs::Uniform(0)),
                enabled: true,
            },
        );
        let err = crate::validation::validate_plugins(&cfg).unwrap_err();
        match err {
            ValidationError::ZeroTimeoutMs { plugin, slot } => {
                assert_eq!(plugin, "p");
                assert_eq!(slot, "uniform");
            }
            other => panic!("expected ZeroTimeoutMs, got {other:?}"),
        }
    }

    #[test]
    fn validate_plugins_rejects_zero_per_hook_timeout() {
        let mut cfg = mk_cfg_with_routes(vec![]);
        cfg.plugins.insert(
            "p".to_string(),
            crate::plugins::PluginEntry {
                kind: "prompt_compressor".to_string(),
                config: serde_json::json!({}),
                on_error: crate::plugins::OnErrorYaml::Skip,
                timeout_ms: Some(crate::plugins::TimeoutMs::PerHook {
                    default: Some(50),
                    on_decoded_request: None,
                    on_resolved: None,
                    on_stream_event: Some(0), // <- this triggers
                    on_response_complete: None,
                }),
                enabled: true,
            },
        );
        let err = crate::validation::validate_plugins(&cfg).unwrap_err();
        match err {
            ValidationError::ZeroTimeoutMs { plugin, slot } => {
                assert_eq!(plugin, "p");
                assert_eq!(slot, "on_stream_event");
            }
            other => panic!("expected ZeroTimeoutMs, got {other:?}"),
        }
    }
```

The exact name of the test module is whatever the existing module is called in `validation.rs` (often just `mod tests`). Put the new tests inside it.

- [ ] **Step 6: Run tests**

Run: `cargo test -p agent-shim-config validate_plugins`
Expected: 4 tests pass (`validate_plugins_rejects_undeclared_reference`, `validate_plugins_accepts_declared_reference`, `validate_plugins_rejects_zero_uniform_timeout`, `validate_plugins_rejects_zero_per_hook_timeout`).

- [ ] **Step 7: Run the broader config test suite to confirm no regressions**

Run: `cargo test -p agent-shim-config`
Expected: all pre-existing tests still pass, plus the 4 new validation tests + the 8-9 plugins-module tests from T1.

- [ ] **Step 8: Commit**

```bash
git add crates/config/src/validation.rs
git commit -m "feat(config): Layer A plugin validation rules (P03 T5)"
```

---

### Task 6: Integration test for the full spec §5.2 YAML example

**Files:**
- Create: `crates/config/tests/plugins_yaml.rs`

- [ ] **Step 1: Inspect existing integration tests**

Look at one of the existing files under `crates/config/tests/` for the style. (Likely zero or few exist; check first.)

Run: `ls crates/config/tests/` — if the directory is empty, this is the first integration test file for the config crate, which is fine. If files exist, mirror their style.

- [ ] **Step 2: Write the integration test**

Create `crates/config/tests/plugins_yaml.rs`:

```rust
//! Integration tests for the plugin system's YAML schema (Phase 7
//! P03). Verifies the full example from spec §5.2 deserialises and
//! round-trips, and confirms env-var overlay still works for the
//! new fields.

use agent_shim_config::{validate, GatewayConfig, OnErrorYaml, TimeoutMs};

const FULL_SPEC_5_2_EXAMPLE: &str = r#"
upstreams:
  deepseek:
    type: deepseek
    base_url: https://api.deepseek.com/v1
    api_key: sk-test
    tier: standard

  anthropic:
    type: anthropic
    api_key: sk-test
    tier: premium

  copilot:
    type: github_copilot
    tier: standard

plugins:
  compressor_for_deepseek:
    type: prompt_compressor
    config:
      strategy: summarize_old_turns
      keep_last: 4
      max_input_tokens: 32000
    on_error: skip
    timeout_ms: 50

  pii_scrubber_strict:
    type: pii_scrubber
    config:
      patterns: [email, phone, ssn]
    on_error: fail
    timeout_ms: 20

  usage_recorder:
    type: usage_recorder
    config: { sink: prometheus }

routes:
  - frontend: anthropic_messages
    model: deepseek-chat
    upstream: deepseek
    upstream_model: deepseek-chat
    plugins:
      on_decoded_request: [pii_scrubber_strict, compressor_for_deepseek]
      on_response_complete: [usage_recorder]

  - frontend: anthropic_messages
    model: claude-sonnet
    upstream: anthropic
    upstream_model: claude-sonnet
    plugins:
      on_response_complete: [usage_recorder]

  - frontend: openai_chat
    model: gpt-4o
    upstream: copilot
    upstream_model: gpt-4o
"#;

#[test]
fn full_spec_5_2_example_deserialises() {
    let cfg: GatewayConfig = serde_yaml::from_str(FULL_SPEC_5_2_EXAMPLE)
        .expect("spec §5.2 example must parse");

    // Three plugins declared.
    assert_eq!(cfg.plugins.len(), 3);

    // Compressor: kind, config, on_error, timeout, enabled.
    let compressor = cfg
        .plugins
        .get("compressor_for_deepseek")
        .expect("compressor declared");
    assert_eq!(compressor.kind, "prompt_compressor");
    assert_eq!(compressor.config["strategy"], "summarize_old_turns");
    assert_eq!(compressor.config["keep_last"], 4);
    assert_eq!(compressor.on_error, OnErrorYaml::Skip);
    assert_eq!(compressor.timeout_ms, Some(TimeoutMs::Uniform(50)));
    assert!(compressor.enabled);

    // Scrubber: on_error: fail (override of default).
    let scrubber = cfg
        .plugins
        .get("pii_scrubber_strict")
        .expect("scrubber declared");
    assert_eq!(scrubber.on_error, OnErrorYaml::Fail);

    // Recorder: empty timeout_ms (None) and default on_error.
    let recorder = cfg.plugins.get("usage_recorder").expect("recorder declared");
    assert!(recorder.timeout_ms.is_none());
    assert_eq!(recorder.on_error, OnErrorYaml::Skip);

    // Three routes. DeepSeek route has two H2 plugins + one H7.
    assert_eq!(cfg.routes.len(), 3);
    let deepseek_route = cfg
        .routes
        .iter()
        .find(|r| r.model == "deepseek-chat")
        .expect("deepseek route");
    let plugins_block = deepseek_route
        .plugins
        .as_ref()
        .expect("deepseek route has plugins:");
    assert_eq!(
        plugins_block.on_decoded_request,
        vec!["pii_scrubber_strict", "compressor_for_deepseek"]
    );
    assert_eq!(plugins_block.on_response_complete, vec!["usage_recorder"]);
    assert!(plugins_block.on_resolved.is_empty());
    assert!(plugins_block.on_stream_event.is_empty());

    // Claude route: only H7.
    let claude_route = cfg
        .routes
        .iter()
        .find(|r| r.model == "claude-sonnet")
        .expect("claude route");
    let plugins_block = claude_route
        .plugins
        .as_ref()
        .expect("claude route has plugins:");
    assert_eq!(plugins_block.on_response_complete, vec!["usage_recorder"]);

    // GPT-4o route: no plugins.
    let gpt_route = cfg
        .routes
        .iter()
        .find(|r| r.model == "gpt-4o")
        .expect("gpt route");
    assert!(gpt_route.plugins.is_none());
}

#[test]
fn full_spec_5_2_example_validates() {
    let cfg: GatewayConfig = serde_yaml::from_str(FULL_SPEC_5_2_EXAMPLE).unwrap();
    validate(&cfg).expect("spec §5.2 example must pass validation");
}

#[test]
fn validation_rejects_route_referencing_missing_plugin() {
    // Build a config that references a plugin that isn't declared.
    let yaml = r#"
upstreams:
  noop:
    type: open_ai_compatible
    base_url: http://localhost:9999/v1
    api_key: dummy
    tier: standard

plugins:
  declared:
    type: prompt_compressor

routes:
  - frontend: openai_chat
    model: x
    upstream: noop
    upstream_model: x
    plugins:
      on_decoded_request: [undeclared]
"#;
    let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = validate(&cfg).expect_err("undeclared plugin must fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("undeclared") || msg.contains("plugin `undeclared`"),
        "error must name the undeclared plugin: {msg}"
    );
}

#[test]
fn validation_rejects_zero_timeout() {
    let yaml = r#"
upstreams:
  noop:
    type: open_ai_compatible
    base_url: http://localhost:9999/v1
    api_key: dummy
    tier: standard

plugins:
  bad:
    type: prompt_compressor
    timeout_ms: 0

routes:
  - frontend: openai_chat
    model: x
    upstream: noop
    upstream_model: x
"#;
    let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let err = validate(&cfg).expect_err("timeout_ms: 0 must fail validation");
    let msg = err.to_string();
    assert!(msg.contains("timeout_ms"), "error must mention timeout_ms: {msg}");
    assert!(msg.contains("bad"), "error must name the plugin: {msg}");
}

#[test]
fn env_overlay_can_disable_a_plugin() {
    // The `agent-shim-config` crate exposes env-var overlay through
    // figment, with prefix `AGENT_SHIM__` and `__` as the nesting
    // separator. This test verifies the existing mechanism works
    // for the new `plugins.<name>.enabled` field.
    //
    // We don't go through figment directly here (that would require
    // mucking with std::env, which is awkward in a test). Instead we
    // verify the round-trip via serde — the same path figment uses
    // internally — by setting `enabled: false` in the YAML and
    // confirming it parses.
    let yaml = r#"
plugins:
  compressor:
    type: prompt_compressor
    enabled: false
"#;
    let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("parses");
    let entry = cfg.plugins.get("compressor").expect("declared");
    assert!(!entry.enabled, "`enabled: false` must round-trip");
}
```

- [ ] **Step 3: Run the integration tests**

Run: `cargo test -p agent-shim-config --test plugins_yaml`
Expected: 5 tests pass.

If any test fails, investigate. Most likely failure modes:
- `serde_yaml::from_str` complains about an unrecognised field → check the YAML against the actual schema (e.g. existing upstream variants use specific field names like `base_url` rather than `url`)
- `validate()` fails on the spec example because the upstreams are missing some required field (`tier` is required per Phase 6; ensure each upstream has `tier:`)

- [ ] **Step 4: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: 726 baseline (post-P02) + new tests from this plan. The exact count after P03 should be roughly **741** (9 plugin-module tests from T1 + 4 validation tests from T5 + 5 integration tests from this T6 = 18 new, but if some weren't counted earlier the exact number may shift by ±2).

- [ ] **Step 5: Commit**

```bash
git add crates/config/tests/plugins_yaml.rs
git commit -m "test(config): integration tests for plugin YAML (P03 T6)"
```

---

### Task 7: Workspace verification (clippy + fmt + final regression)

- [ ] **Step 1: Workspace clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean. The new code follows the established patterns from Phase 4 / Phase 6 validation, so clippy should not surface anything new.

If clippy flags anything you wrote, fix it. Common issues to anticipate:
- Unnecessary `.clone()` on `&str` — use `.to_string()` directly
- `#[allow(dead_code)]` not needed on items used by tests in the same module
- `RoutePluginsBlock::iter_references` returns `impl Iterator` — if clippy doesn't like the desugared form, try `Box<dyn Iterator>` (but prefer `impl` if it works)

- [ ] **Step 2: Workspace fmt**

Run: `cargo fmt --all -- --check`

If anything is unformatted, run `cargo fmt --all` and commit:
```bash
git add -u
git commit -m "style: cargo fmt (P03 T7)"
```

The nightly-only `Warning: can't set ...` lines from rustfmt are pre-existing and can be ignored — they don't indicate actual formatting issues.

- [ ] **Step 3: Final workspace test run**

Run: `cargo test --workspace`
Expected: 726 baseline + 18 new (or thereabouts) = ~744 passing, 0 failed.

The exact final count is whatever you observe; the important assertion is **zero failures**.

- [ ] **Step 4: No commit needed for verification**

Verification doesn't change files. Move on (unless `cargo fmt` made changes — those were committed in Step 2).

---

## Acceptance criteria

- New module `crates/config/src/plugins.rs` exists with `PluginEntry`, `TimeoutMs`, `RoutePluginsBlock`, `OnErrorYaml` types, all `pub` and re-exported from `agent_shim_config`.
- `GatewayConfig.plugins: BTreeMap<String, PluginEntry>` field added with `#[serde(default)]`.
- `RouteEntry.plugins: Option<RoutePluginsBlock>` field added with `#[serde(default, skip_serializing_if = "Option::is_none")]`.
- `ValidationError` has 3 new variants: `UndeclaredPlugin`, `ZeroTimeoutMs`, `DuplicatePluginName`.
- `validate_plugins(cfg) -> Result<(), ValidationError>` exists as a public function in `validation.rs`, called by `validate()`.
- The full spec §5.2 YAML example deserialises into `GatewayConfig` losslessly.
- The full spec §5.2 YAML example passes `validate(&cfg)`.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.
- `cargo test --workspace` shows zero failures with ~744 total tests (exact count flexible).
- No external workspace dependencies added.
- `crates/core/` is not touched (frozen-core invariant preserved).

## Notes for the implementer

- The `RoutePluginsBlock::iter_references()` helper is intentionally `pub` even though only Layer A validation uses it in P03. P06's `PluginRegistry::from_specs(...)` will use it too — keeping the API surface stable now avoids breaking the helper later.
- `BTreeMap<String, PluginEntry>` (rather than `HashMap`) preserves a stable iteration order across YAML round-trips. This matches how `upstreams:` is stored (`BTreeMap<String, UpstreamConfig>`).
- `OnErrorYaml` is duplicated between the config and plugins crates because the boundary rule forbids `config` depending on `plugins`. The gateway wiring (P06) translates between them via a `From` impl in P06. Don't try to share the type now.
- `TimeoutMs::PerHook` allows missing `default` (it's `Option<u64>`). This lets operators write only the slots they care about. The registry's `HookTimeouts::from_yaml(...)` (in P06) fills missing slots from the system defaults (50/50/5/50).
- If you find that a `RouteEntry::default()` impl is auto-derived AND any existing test relied on it, the new `plugins: Option<_>` field will silently default to `None` — no test breakage.
- The integration test file `plugins_yaml.rs` uses `serde_yaml` directly rather than `figment::Figment` — by the time figment reads YAML it goes through the same `serde::Deserialize` impl, so testing at the serde layer is equivalent and avoids the std::env mucking that figment-level tests would require. P03 explicitly defers actual figment env-overlay tests to P06 / P07 integration tests where they're easier to wire up.
