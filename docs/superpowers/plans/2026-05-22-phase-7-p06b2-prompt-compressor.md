# Phase 7 P06b2 — prompt_compressor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the `prompt_compressor` built-in plugin (H2 only) with three strategies (`drop_old_turns`, `truncate_to_tokens`, `summarize_old_turns`), plus a factory-signature upgrade that lets factories validate upstream references at startup.

**Architecture:** New plugin module at `crates/plugins/src/builtin/prompt_compressor/` split across 6 files (config, groups, three strategy modules, mod). The `PluginFactory::instantiate` signature gains a `deps: &FactoryDependencies` parameter so `prompt_compressor` can look up an upstream `Arc<dyn BackendProvider>` at startup (fail-fast on unknown name) and call it directly inside H2 — bypassing the gateway pipeline so the summarizer call cannot recurse. Three new declarative metrics live in the observability catalog. New Cargo feature `prompt_compressor` is default-on, brings in `agent-shim-tokens` for cl100k-based token counting.

**Tech Stack:** Rust 1.85+, `agent-shim-tokens` (tiktoken-rs cl100k), `tokio::time::timeout`, `metrics` 0.23 (existing observability infra), `tracing`. Reuses `BackendProvider` trait from `agent-shim-providers` and `ProviderRegistry` from same.

**Source spec:** `docs/superpowers/specs/2026-05-22-phase-7-p06b2-prompt-compressor-design.md` (commit `0689a59`).

---

## Pre-flight

Baseline: **878 tests passing** at P06b1 merge tip (commit `33ca539`). After P06b2: expect ~910 tests (878 baseline + ~31 plugin unit tests + 1 gateway integration test + ~2 trait-level test fixture migrations).

Frozen-core invariant: `crates/core/` MUST be untouched. Acceptance check at end: `git diff master..HEAD -- crates/core/` empty.

ALWAYS prefix bash/git commands with `rtk` for token efficiency. Working dir: `Q:/src/AgentShim/.claude/worktrees/phase-7-p06b2-prompt-compressor`.

---

## File structure overview

```text
crates/plugins/
├── Cargo.toml                              # T1: add feature + agent-shim-tokens dep
├── src/
│   ├── trait_def.rs                        # T2: PluginFactory::instantiate + FactoryDependencies
│   ├── context.rs                          # T2 (optional): FactoryDependencies type may live here or trait_def
│   ├── registry.rs                         # T2 (callsites) + T3 (build() forwards deps)
│   ├── builtin/
│   │   ├── mod.rs                          # T4: register PromptCompressorFactory
│   │   ├── pii_scrubber.rs                 # T2: instantiate +1 arg
│   │   ├── usage_recorder.rs               # T2: instantiate +1 arg
│   │   └── prompt_compressor/              # NEW (T5..T10)
│   │       ├── mod.rs                      # T10: PromptCompressor struct, Plugin impl, dispatcher, metric emit
│   │       ├── config.rs                   # T5: PromptCompressorConfig + Strategy enum + sub-configs
│   │       ├── groups.rs                   # T6: group_boundaries + count_messages_tokens
│   │       ├── drop_old_turns.rs           # T7: apply_drop_old_turns
│   │       ├── truncate.rs                 # T8: apply_truncate_to_tokens
│   │       └── summarize.rs                # T9: apply_summarize_old_turns + SUMMARY_PROMPT_PREFIX
│   │                                       #     + serialize_groups_to_text + drain_text_from_stream
crates/observability/
├── src/metrics/catalog.rs                  # T4: 3 new metric declarations
├── tests/v060_metrics_baseline.json        # T4: 3 new baseline entries; count 19 -> 22
crates/gateway/
├── src/state.rs                            # T3: reorder build (providers first), pass FactoryDependencies
├── tests/prompt_compressor_integration.rs  # T11: end-to-end mockito test (summarize path)
```

---

## Task 1: Add `agent-shim-tokens` dep + `prompt_compressor` feature

**Files:**
- Modify: `crates/plugins/Cargo.toml`

`agent-shim-tokens` is already in the workspace (path dep at `crates/tokens`). We add it under a new `prompt_compressor` feature in the plugins crate.

- [ ] **Step 1: Add feature + optional `agent-shim-tokens` dep**

Open `crates/plugins/Cargo.toml`. Update `[features]`:

```toml
[features]
default = ["usage_recorder", "pii_scrubber", "prompt_compressor"]
usage_recorder = []
pii_scrubber = ["dep:regex"]
prompt_compressor = ["dep:agent-shim-tokens"]
```

In `[dependencies]`, `agent-shim-tokens` already exists as a non-optional path dep. Change it to optional and explicit:

```toml
agent-shim-tokens = { path = "../tokens", optional = true }
```

Note: if `agent-shim-tokens` is currently a non-optional dependency elsewhere in plugins (used by other code), keep it non-optional and remove the `optional = true` + `dep:` prefix from the feature line. Verify before editing:

```bash
rtk grep -n "agent_shim_tokens" crates/plugins/src/
```

If `grep` returns no hits, the dep is unused today and can safely become `optional = true`.

- [ ] **Step 2: Verify default build**

Run: `rtk cargo build -p agent-shim-plugins`
Expected: PASS — `prompt_compressor` feature on, `agent-shim-tokens` linked.

Run: `rtk cargo build -p agent-shim-plugins --no-default-features --features usage_recorder`
Expected: PASS — both `pii_scrubber` and `prompt_compressor` features off; `regex` and `agent-shim-tokens` NOT pulled in.

- [ ] **Step 3: Commit**

```bash
rtk git add crates/plugins/Cargo.toml Cargo.lock
rtk git commit -m "build(plugins): add prompt_compressor feature + agent-shim-tokens dep (P06b2 T1)"
```

---

## Task 2: `PluginFactory::instantiate` signature upgrade

**Files:**
- Modify: `crates/plugins/src/trait_def.rs` (trait signature + new `FactoryDependencies` struct)
- Modify: `crates/plugins/src/builtin/pii_scrubber.rs` (add `_deps: &FactoryDependencies` arg)
- Modify: `crates/plugins/src/builtin/usage_recorder.rs` (same)
- Modify: `crates/plugins/src/registry.rs` (Phase 2 callsite + `build()` signature stays for now — Task 3 will update it; for THIS task, build a stack-local `FactoryDependencies::empty()` and pass it)
- Modify: `crates/plugins/src/trait_def.rs` (test fixture `NoopPlugin` factory if any)
- Modify: `crates/plugins/src/invoke.rs` test fixtures (if they call `instantiate`)

This task changes the trait but defers updating `PluginRegistry::build`'s public signature to Task 3 (where the gateway also gets reordered). For now, `build()` constructs a `FactoryDependencies::empty()` internally; Task 3 will thread the real one through.

- [ ] **Step 1: Add `FactoryDependencies` and update trait signature**

Open `crates/plugins/src/trait_def.rs`. At the top of the file, add the import (after existing imports):

```rust
use std::collections::BTreeMap;
use std::sync::Arc;

use agent_shim_providers::BackendProvider;
```

Add `agent-shim-providers` to `[dependencies]` in `crates/plugins/Cargo.toml`:

```toml
agent-shim-providers = { path = "../providers" }
```

Below the existing `use` statements in `trait_def.rs`, add:

```rust
/// Dependencies threaded into every `PluginFactory::instantiate` call.
///
/// Lifetime is bound to the gateway startup phase — factories may
/// snapshot what they need (e.g. clone an `Arc<dyn BackendProvider>`)
/// but MUST NOT retain the `FactoryDependencies` reference itself.
///
/// Single-field today; future fields land here without breaking the
/// trait signature (P06c will add e.g. metrics handles).
pub struct FactoryDependencies<'a> {
    pub providers: &'a BTreeMap<String, Arc<dyn BackendProvider>>,
}

impl<'a> FactoryDependencies<'a> {
    /// Test-only: build a `FactoryDependencies` pointing at an empty
    /// providers map. Useful when a factory under test doesn't touch
    /// providers at all (e.g. pii_scrubber, usage_recorder).
    pub fn empty() -> FactoryDependencies<'static> {
        static EMPTY: std::sync::OnceLock<BTreeMap<String, Arc<dyn BackendProvider>>> =
            std::sync::OnceLock::new();
        FactoryDependencies {
            providers: EMPTY.get_or_init(BTreeMap::new),
        }
    }
}
```

Change the `PluginFactory::instantiate` signature (around line 167):

```rust
    fn instantiate(
        &self,
        plugin_name: &str,
        config: serde_json::Value,
        deps: &FactoryDependencies,
    ) -> Result<Box<dyn Plugin>, PluginConfigError>;
```

- [ ] **Step 2: Re-export from plugins lib**

Open `crates/plugins/src/lib.rs`. Find the `pub use trait_def::{...}` line (or `pub use crate::trait_def::*`); add `FactoryDependencies` to the export list. Example:

```rust
pub use trait_def::{FactoryDependencies, Hook, HookSet, Plugin, PluginFactory};
```

- [ ] **Step 3: Migrate `pii_scrubber` factory**

Open `crates/plugins/src/builtin/pii_scrubber.rs`. Find `impl PluginFactory for PiiScrubberFactory` block, locate `fn instantiate(&self, plugin_name: &str, config: serde_json::Value)`. Add `_deps` parameter:

```rust
    fn instantiate(
        &self,
        plugin_name: &str,
        config: serde_json::Value,
        _deps: &crate::FactoryDependencies,
    ) -> Result<Box<dyn Plugin>, PluginConfigError> {
```

(Body unchanged.)

In the same file's tests module, every test that calls `PiiScrubberFactory.instantiate("p", cfg)` becomes `PiiScrubberFactory.instantiate("p", cfg, &crate::FactoryDependencies::empty())`. Approximately 9 sites. Use `replace_all` carefully with the longer string for uniqueness.

- [ ] **Step 4: Migrate `usage_recorder` factory**

Open `crates/plugins/src/builtin/usage_recorder.rs`. Make the same changes — add `_deps` to `instantiate`, update test callsites (around 3 sites).

- [ ] **Step 5: Update registry callsites**

Open `crates/plugins/src/registry.rs`. Find the line where `factory.instantiate(name, spec.config.clone())` is called inside `build` (around line 733):

```rust
            let plugin_box = factory
                .instantiate(name, spec.config.clone(), &deps_for_this_task)
                .map_err(|e| RegistryBuildError::Instantiation(e, name.clone()))?;
```

Where `deps_for_this_task` is a stack-local — add **at the top of the `build` function**, right after `let supervisor = ...`:

```rust
        // P06b2 T2: factory signature requires deps. Task 3 will replace
        // this empty() with a real one passed through from AppCore.
        let deps_for_this_task = crate::FactoryDependencies::empty();
```

In `registry.rs::tests` (around line 1690+), find every `factory.instantiate(...)` call and add `&crate::FactoryDependencies::empty()` as the third arg. Estimate ~3-5 sites.

In `crates/plugins/src/invoke.rs::tests` and `crates/plugins/src/trait_def.rs::tests`, same migration if they call `instantiate` (likely 1-2 sites each).

- [ ] **Step 6: Verify everything compiles and tests pass**

Run: `rtk cargo build -p agent-shim-plugins --tests --all-features`
Expected: PASS — all trait callers updated.

Run: `rtk cargo nextest run -p agent-shim-plugins`
Expected: PASS — baseline 102 plugin tests still pass.

Run: `rtk cargo build --workspace`
Expected: PASS — gateway still calls `PluginRegistry::build` (signature unchanged externally in this task).

- [ ] **Step 7: Commit**

```bash
rtk git add crates/plugins/Cargo.toml crates/plugins/src/trait_def.rs crates/plugins/src/lib.rs crates/plugins/src/registry.rs crates/plugins/src/invoke.rs crates/plugins/src/builtin/pii_scrubber.rs crates/plugins/src/builtin/usage_recorder.rs Cargo.lock
rtk git commit -m "refactor(plugins): PluginFactory::instantiate accepts FactoryDependencies (P06b2 T2)"
```

---

## Task 3: Thread `FactoryDependencies` from gateway through `PluginRegistry::build`

**Files:**
- Modify: `crates/plugins/src/registry.rs` (`build` signature: add `deps: FactoryDependencies` param)
- Modify: `crates/gateway/src/state.rs` (reorder so providers build before plugins; pass `FactoryDependencies`)

The stack-local `FactoryDependencies::empty()` from T2 is a placeholder; replace it with a real reference passed in from the caller.

- [ ] **Step 1: Update `PluginRegistry::build` signature**

Open `crates/plugins/src/registry.rs`. Find the `pub fn build(...)` around line 700. Add the `deps` parameter:

```rust
    pub fn build(
        factories: Vec<Box<dyn PluginFactory>>,
        plugin_specs: &[(String, agent_shim_config::plugins::PluginEntry)],
        route_specs: &[agent_shim_config::schema::RouteEntry],
        deps: crate::FactoryDependencies<'_>,
    ) -> Result<Self, RegistryBuildError> {
```

Remove the `let deps_for_this_task = ...empty()` line added in T2. Update the `factory.instantiate(...)` call inside Phase 2 to use `&deps`:

```rust
            let plugin_box = factory
                .instantiate(name, spec.config.clone(), &deps)
                .map_err(|e| RegistryBuildError::Instantiation(e, name.clone()))?;
```

- [ ] **Step 2: Update `registry.rs` tests that call `build`**

In `crates/plugins/src/registry.rs::tests`, find every `PluginRegistry::build(factories, &plugin_specs, &routes)` call (search for `::build(`). Add `crate::FactoryDependencies::empty()` as the fourth argument:

```rust
let result = PluginRegistry::build(
    factories,
    &plugin_specs,
    &routes,
    crate::FactoryDependencies::empty(),
);
```

Approximately 10-15 callsites — use `Edit replace_all` if you find unique substring patterns.

- [ ] **Step 3: Reorder `AppCore::build` and pass real deps**

Open `crates/gateway/src/state.rs`. Find the `async fn build(...)` around line 221. Currently lines 229-247 build `plugins` first; lines 261-298 build the provider registry. **Reorder so providers come first.**

Replace lines 229-247 with: **delete** the entire `let plugins = match plugin_override { ... };` block here.

After line 298 (after `for (name, upstream) in &config.upstreams { ... }` finishes), but BEFORE `let static_router = ...` (line 300), insert:

```rust
        // P06b2 T3: plugin registry build needs an immutable reference to
        // the provider registry so factories (e.g. prompt_compressor's
        // summarize_old_turns) can validate `upstream:` references at
        // startup and snapshot an Arc<dyn BackendProvider>.
        let plugins = match plugin_override {
            Some(p) => p,
            None => {
                let factories = agent_shim_plugins::builtin::builtin_plugins();
                let plugin_specs: Vec<(String, agent_shim_config::plugins::PluginEntry)> = config
                    .plugins
                    .iter()
                    .map(|(name, entry)| (name.clone(), entry.clone()))
                    .collect();
                let deps = agent_shim_plugins::FactoryDependencies {
                    providers: registry.inner_map(),
                };
                Arc::new(
                    agent_shim_plugins::PluginRegistry::build(
                        factories,
                        &plugin_specs,
                        &config.routes,
                        deps,
                    )
                    .map_err(|e| anyhow::anyhow!("plugin registry build failed: {e}"))?,
                )
            }
        };
```

The reference is `registry.inner_map()` — see Step 4 for adding this accessor.

- [ ] **Step 4: Add `ProviderRegistry::inner_map()` accessor**

Open `crates/providers/src/lib.rs`. Find `impl ProviderRegistry` around line 98. Add a new accessor method:

```rust
    /// Borrow the underlying provider map. Plan 07 P06b2: needed for
    /// `agent_shim_plugins::FactoryDependencies` so plugin factories
    /// can validate upstream references at startup.
    pub fn inner_map(&self) -> &std::collections::BTreeMap<String, Arc<dyn BackendProvider>> {
        &self.providers
    }
```

- [ ] **Step 5: Verify**

Run: `rtk cargo build --workspace`
Expected: PASS — gateway reordered, real deps threaded through.

Run: `rtk cargo nextest run --workspace`
Expected: PASS — 878 baseline + 0 new tests (this task is wiring; tests come in T5+).

- [ ] **Step 6: Commit**

```bash
rtk git add crates/plugins/src/registry.rs crates/gateway/src/state.rs crates/providers/src/lib.rs
rtk git commit -m "refactor(gateway): build providers before plugins; thread FactoryDependencies (P06b2 T3)"
```

---

## Task 4: Observability — 3 new metric declarations

**Files:**
- Modify: `crates/observability/src/metrics/catalog.rs` (3 new `#[derive(Metric)]` blocks; bump test count)
- Modify: `crates/observability/tests/v060_metrics_baseline.json` (3 new entries sorted by name)

- [ ] **Step 1: Add the 3 metric structs**

Open `crates/observability/src/metrics/catalog.rs`. Find the existing `PluginPiiScrubberMatchesTotal` block (around line 247) — the P06b1 marker for "Built-in plugins (Phase 7 P06b)". Below it, append:

```rust
#[derive(Metric)]
#[metric(
    name = "agent_shim_plugin_prompt_compressor_actions_total",
    kind = "counter",
    help = "prompt_compressor actions by strategy and outcome (Plan 07 P06b2)"
)]
pub struct PluginPromptCompressorActionsTotal;

#[derive(Metric)]
#[metric(
    name = "agent_shim_plugin_prompt_compressor_messages_dropped_total",
    kind = "counter",
    help = "Total messages dropped by prompt_compressor by strategy (Plan 07 P06b2)"
)]
pub struct PluginPromptCompressorMessagesDroppedTotal;

#[derive(Metric)]
#[metric(
    name = "agent_shim_plugin_prompt_compressor_summary_duration_seconds",
    kind = "histogram",
    help = "summarize_old_turns provider call duration by outcome (Plan 07 P06b2)"
)]
pub struct PluginPromptCompressorSummaryDurationSeconds;
```

- [ ] **Step 2: Bump the descriptor count test**

In the same file, find the `distributed_slice_collects_all_markers` test at the bottom:

```rust
    #[test]
    fn distributed_slice_collects_all_markers() {
        let descriptors = iter_descriptors();
        assert_eq!(
            descriptors.len(),
            19,
            "expected 19 metric descriptors in catalog, got {}",
            descriptors.len()
        );
    }
```

Change `19` to `22` (twice — in the literal and in the message):

```rust
        assert_eq!(
            descriptors.len(),
            22,
            "expected 22 metric descriptors in catalog, got {}",
            descriptors.len()
        );
```

- [ ] **Step 3: Update baseline JSON**

Open `crates/observability/tests/v060_metrics_baseline.json`. The entries are sorted alphabetically by `name`. Find the existing `agent_shim_plugin_pii_scrubber_matches_total` entry (around line 10) and insert the 3 new entries in alphabetical position. The names are:

```
agent_shim_plugin_prompt_compressor_actions_total
agent_shim_plugin_prompt_compressor_messages_dropped_total
agent_shim_plugin_prompt_compressor_summary_duration_seconds
```

Sorted, they all come AFTER `agent_shim_plugin_pii_scrubber_matches_total` and BEFORE `agent_shim_rate_limit_rejected_total`. Insert:

```json
  {"name": "agent_shim_plugin_prompt_compressor_actions_total", "kind": "counter", "help": "prompt_compressor actions by strategy and outcome (Plan 07 P06b2)"},
  {"name": "agent_shim_plugin_prompt_compressor_messages_dropped_total", "kind": "counter", "help": "Total messages dropped by prompt_compressor by strategy (Plan 07 P06b2)"},
  {"name": "agent_shim_plugin_prompt_compressor_summary_duration_seconds", "kind": "histogram", "help": "summarize_old_turns provider call duration by outcome (Plan 07 P06b2)"},
```

- [ ] **Step 4: Verify**

Run: `rtk cargo nextest run -p agent-shim-observability`
Expected: PASS — `distributed_slice_collects_all_markers` passes with 22; `registered_metrics_match_v060_baseline` passes with the new 3 entries.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/observability/src/metrics/catalog.rs crates/observability/tests/v060_metrics_baseline.json
rtk git commit -m "feat(observability): declare 3 prompt_compressor metrics (P06b2 T4)"
```

---

## Task 5: `prompt_compressor` config types + factory skeleton

**Files:**
- Create: `crates/plugins/src/builtin/prompt_compressor/mod.rs` (skeleton: `PromptCompressor` struct, `PromptCompressorFactory`, Plugin impl with stub hooks)
- Create: `crates/plugins/src/builtin/prompt_compressor/config.rs` (full config types)
- Modify: `crates/plugins/src/builtin/mod.rs` (add the new module behind `#[cfg(feature = "prompt_compressor")]`)

This task lays the file structure and gets config-only tests passing. Hooks stay stubbed (`Ok(req)`); strategies implemented in T6-T9.

- [ ] **Step 1: Create `config.rs` with full types + 5 tests**

Create `crates/plugins/src/builtin/prompt_compressor/config.rs`:

```rust
//! Per-plugin YAML `config:` block for `prompt_compressor`.
//!
//! Spec: `docs/superpowers/specs/2026-05-22-phase-7-p06b2-prompt-compressor-design.md` §4.

#![cfg(feature = "prompt_compressor")]

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptCompressorConfig {
    pub strategy: Strategy,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Strategy {
    DropOldTurns(DropOldTurnsConfig),
    TruncateToTokens(TruncateToTokensConfig),
    SummarizeOldTurns(SummarizeOldTurnsConfig),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DropOldTurnsConfig {
    pub keep_last_n: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TruncateToTokensConfig {
    pub target_tokens: u32,
    pub keep_last_n: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummarizeOldTurnsConfig {
    pub keep_last_n: usize,
    pub summarizer: SummarizerConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummarizerConfig {
    pub upstream: String,
    pub model: String,
    #[serde(default = "default_max_summary_tokens")]
    pub max_summary_tokens: u32,
    #[serde(default = "default_summary_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_max_summary_tokens() -> u32 {
    300
}

fn default_summary_timeout_ms() -> u64 {
    5000
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn config_drop_old_turns_round_trips() {
        let cfg: PromptCompressorConfig = serde_json::from_value(json!({
            "strategy": { "type": "drop_old_turns", "keep_last_n": 4 }
        }))
        .unwrap();
        match cfg.strategy {
            Strategy::DropOldTurns(d) => assert_eq!(d.keep_last_n, 4),
            other => panic!("expected DropOldTurns, got {other:?}"),
        }
    }

    #[test]
    fn config_truncate_to_tokens_round_trips() {
        let cfg: PromptCompressorConfig = serde_json::from_value(json!({
            "strategy": {
                "type": "truncate_to_tokens",
                "target_tokens": 8192,
                "keep_last_n": 2
            }
        }))
        .unwrap();
        match cfg.strategy {
            Strategy::TruncateToTokens(t) => {
                assert_eq!(t.target_tokens, 8192);
                assert_eq!(t.keep_last_n, 2);
            }
            other => panic!("expected TruncateToTokens, got {other:?}"),
        }
    }

    #[test]
    fn config_summarize_round_trips_with_defaults() {
        let cfg: PromptCompressorConfig = serde_json::from_value(json!({
            "strategy": {
                "type": "summarize_old_turns",
                "keep_last_n": 2,
                "summarizer": {
                    "upstream": "cheap-haiku",
                    "model": "claude-haiku-4-5"
                }
            }
        }))
        .unwrap();
        match cfg.strategy {
            Strategy::SummarizeOldTurns(s) => {
                assert_eq!(s.keep_last_n, 2);
                assert_eq!(s.summarizer.upstream, "cheap-haiku");
                assert_eq!(s.summarizer.model, "claude-haiku-4-5");
                assert_eq!(s.summarizer.max_summary_tokens, 300);
                assert_eq!(s.summarizer.timeout_ms, 5000);
            }
            other => panic!("expected SummarizeOldTurns, got {other:?}"),
        }
    }

    #[test]
    fn config_unknown_strategy_type_rejected() {
        let result: Result<PromptCompressorConfig, _> = serde_json::from_value(json!({
            "strategy": { "type": "bogus" }
        }));
        assert!(result.is_err(), "unknown strategy type must be rejected");
    }

    #[test]
    fn config_unknown_field_rejected() {
        let result: Result<PromptCompressorConfig, _> = serde_json::from_value(json!({
            "strategy": { "type": "drop_old_turns", "keep_last_n": 2, "foo": 1 }
        }));
        assert!(
            result.is_err(),
            "deny_unknown_fields must reject extra keys"
        );
    }
}
```

- [ ] **Step 2: Create skeleton `mod.rs`**

Create `crates/plugins/src/builtin/prompt_compressor/mod.rs`:

```rust
//! `prompt_compressor` built-in plugin — H2 token-aware compression.
//!
//! Spec: `docs/superpowers/specs/2026-05-22-phase-7-p06b2-prompt-compressor-design.md`.

#![cfg(feature = "prompt_compressor")]

pub mod config;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use agent_shim_core::{BackendTarget, CanonicalRequest};
use agent_shim_providers::BackendProvider;

use crate::context::PluginContext;
use crate::error::{PluginConfigError, PluginResult};
use crate::trait_def::{FactoryDependencies, HookSet, Plugin, PluginFactory};

use config::{PromptCompressorConfig, Strategy};

pub struct PromptCompressor {
    #[allow(dead_code)] // filled in by T6-T10
    strategy: CompiledStrategy,
}

#[allow(dead_code)] // variants populated by T6-T10
enum CompiledStrategy {
    DropOldTurns {
        keep_last_n: usize,
    },
    TruncateToTokens {
        target_tokens: u32,
        keep_last_n: usize,
    },
    SummarizeOldTurns {
        keep_last_n: usize,
        provider: Arc<dyn BackendProvider>,
        target: BackendTarget,
        max_summary_tokens: u32,
        timeout: Duration,
    },
}

pub struct PromptCompressorFactory;

impl PluginFactory for PromptCompressorFactory {
    fn kind_name(&self) -> &'static str {
        "prompt_compressor"
    }

    fn instantiate(
        &self,
        plugin_name: &str,
        config: serde_json::Value,
        deps: &FactoryDependencies,
    ) -> Result<Box<dyn Plugin>, PluginConfigError> {
        let cfg: PromptCompressorConfig = serde_json::from_value(config)
            .map_err(|e| PluginConfigError::Deserialize(e, plugin_name.to_string()))?;
        let strategy = compile_strategy(plugin_name, cfg.strategy, deps)?;
        Ok(Box::new(PromptCompressor { strategy }))
    }
}

fn compile_strategy(
    plugin_name: &str,
    strategy: Strategy,
    deps: &FactoryDependencies,
) -> Result<CompiledStrategy, PluginConfigError> {
    match strategy {
        Strategy::DropOldTurns(c) => {
            if c.keep_last_n == 0 {
                return Err(invalid(plugin_name, "strategy.keep_last_n", "must be >= 1"));
            }
            Ok(CompiledStrategy::DropOldTurns {
                keep_last_n: c.keep_last_n,
            })
        }
        Strategy::TruncateToTokens(c) => {
            if c.keep_last_n == 0 {
                return Err(invalid(plugin_name, "strategy.keep_last_n", "must be >= 1"));
            }
            if c.target_tokens == 0 {
                return Err(invalid(
                    plugin_name,
                    "strategy.target_tokens",
                    "must be >= 1",
                ));
            }
            Ok(CompiledStrategy::TruncateToTokens {
                target_tokens: c.target_tokens,
                keep_last_n: c.keep_last_n,
            })
        }
        Strategy::SummarizeOldTurns(c) => {
            if c.keep_last_n == 0 {
                return Err(invalid(plugin_name, "strategy.keep_last_n", "must be >= 1"));
            }
            if c.summarizer.max_summary_tokens == 0 {
                return Err(invalid(
                    plugin_name,
                    "strategy.summarizer.max_summary_tokens",
                    "must be >= 1",
                ));
            }
            if c.summarizer.timeout_ms == 0 {
                return Err(invalid(
                    plugin_name,
                    "strategy.summarizer.timeout_ms",
                    "must be >= 1",
                ));
            }
            let provider = deps.providers.get(&c.summarizer.upstream).ok_or_else(|| {
                let mut known: Vec<String> = deps.providers.keys().cloned().collect();
                known.sort();
                invalid(
                    plugin_name,
                    "strategy.summarizer.upstream",
                    &format!(
                        "unknown upstream `{}`; known: {:?}",
                        c.summarizer.upstream, known
                    ),
                )
            })?;
            let target = BackendTarget {
                provider: c.summarizer.upstream.clone(),
                model: c.summarizer.model.clone(),
                policy: Default::default(),
            };
            Ok(CompiledStrategy::SummarizeOldTurns {
                keep_last_n: c.keep_last_n,
                provider: provider.clone(),
                target,
                max_summary_tokens: c.summarizer.max_summary_tokens,
                timeout: Duration::from_millis(c.summarizer.timeout_ms),
            })
        }
    }
}

fn invalid(plugin: &str, field: &str, reason: &str) -> PluginConfigError {
    PluginConfigError::InvalidValue {
        plugin: plugin.to_string(),
        field: field.to_string(),
        reason: reason.to_string(),
    }
}

#[async_trait]
impl Plugin for PromptCompressor {
    fn kind_name(&self) -> &'static str {
        "prompt_compressor"
    }

    fn hooks(&self) -> HookSet {
        HookSet::DECODED_REQUEST
    }

    async fn on_decoded_request(
        &self,
        _ctx: &PluginContext,
        req: CanonicalRequest,
    ) -> PluginResult<CanonicalRequest> {
        // Filled in by T10.
        Ok(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty_deps() -> FactoryDependencies<'static> {
        FactoryDependencies::empty()
    }

    #[test]
    fn factory_kind_name() {
        assert_eq!(PromptCompressorFactory.kind_name(), "prompt_compressor");
    }

    #[test]
    fn factory_compiles_drop_old_turns() {
        let cfg = json!({
            "strategy": { "type": "drop_old_turns", "keep_last_n": 2 }
        });
        let plugin = PromptCompressorFactory
            .instantiate("p", cfg, &empty_deps())
            .expect("should compile");
        assert_eq!(plugin.kind_name(), "prompt_compressor");
        assert!(plugin.hooks().contains(crate::Hook::DecodedRequest));
    }

    #[test]
    fn factory_rejects_keep_last_n_zero() {
        let cfg = json!({
            "strategy": { "type": "drop_old_turns", "keep_last_n": 0 }
        });
        let err = match PromptCompressorFactory.instantiate("p", cfg, &empty_deps()) {
            Ok(_) => panic!("expected InvalidValue"),
            Err(e) => e,
        };
        match err {
            PluginConfigError::InvalidValue { field, reason, .. } => {
                assert_eq!(field, "strategy.keep_last_n");
                assert!(reason.contains("must be >= 1"));
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    #[test]
    fn factory_rejects_unknown_upstream() {
        let cfg = json!({
            "strategy": {
                "type": "summarize_old_turns",
                "keep_last_n": 2,
                "summarizer": {
                    "upstream": "no-such-upstream",
                    "model": "x"
                }
            }
        });
        let err = match PromptCompressorFactory.instantiate("p", cfg, &empty_deps()) {
            Ok(_) => panic!("expected InvalidValue"),
            Err(e) => e,
        };
        match err {
            PluginConfigError::InvalidValue { field, reason, .. } => {
                assert_eq!(field, "strategy.summarizer.upstream");
                assert!(reason.contains("unknown upstream"));
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }
}
```

- [ ] **Step 3: Wire the new module into `builtin/mod.rs`**

Open `crates/plugins/src/builtin/mod.rs`. Add the module declaration alongside existing ones:

```rust
#[cfg(feature = "pii_scrubber")]
pub mod pii_scrubber;
#[cfg(feature = "prompt_compressor")]
pub mod prompt_compressor;
#[cfg(feature = "usage_recorder")]
pub mod usage_recorder;
```

In `builtin_plugins()`, register the new factory in alphabetical order (between `pii_scrubber` and `usage_recorder`):

```rust
pub fn builtin_plugins() -> Vec<Box<dyn PluginFactory>> {
    #[allow(unused_mut)]
    let mut factories: Vec<Box<dyn PluginFactory>> = Vec::new();
    #[cfg(feature = "pii_scrubber")]
    factories.push(Box::new(pii_scrubber::PiiScrubberFactory));
    #[cfg(feature = "prompt_compressor")]
    factories.push(Box::new(prompt_compressor::PromptCompressorFactory));
    #[cfg(feature = "usage_recorder")]
    factories.push(Box::new(usage_recorder::UsageRecorderFactory));
    factories
}
```

- [ ] **Step 4: Run tests**

Run: `rtk cargo nextest run -p agent-shim-plugins`
Expected: PASS — all config + factory skeleton tests pass.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/plugins/src/builtin/prompt_compressor/ crates/plugins/src/builtin/mod.rs
rtk git commit -m "feat(plugins): prompt_compressor config + factory skeleton (P06b2 T5)"
```

---

## Task 6: `groups.rs` — message grouping + token counting

**Files:**
- Create: `crates/plugins/src/builtin/prompt_compressor/groups.rs`
- Modify: `crates/plugins/src/builtin/prompt_compressor/mod.rs` (add `pub(super) mod groups;`)

- [ ] **Step 1: Write failing tests + minimal helpers**

Create `crates/plugins/src/builtin/prompt_compressor/groups.rs`:

```rust
//! Pure helpers: split a `&[Message]` into user-led groups, and count
//! tokens of a group's text/reasoning blocks.
//!
//! Spec §6.1 (groups) + §6.2 (token counting).

#![cfg(feature = "prompt_compressor")]

use std::ops::Range;

use agent_shim_core::{ContentBlock, Message, MessageRole};

/// Split messages into groups. Each group is a contiguous [start, end)
/// range starting at a user-role message and extending up to (but not
/// including) the next user-role message.
///
/// Properties:
/// - `groups.iter().map(|r| r.len()).sum::<usize>() == messages.len()`
/// - Adjacent groups never overlap.
/// - `messages.is_empty()` -> `[]`.
/// - The first group's first message is user-role unless `messages[0]`
///   itself is non-user (defensive: preserve input, never panic).
pub fn group_boundaries(messages: &[Message]) -> Vec<Range<usize>> {
    if messages.is_empty() {
        return Vec::new();
    }
    let mut groups = Vec::new();
    let mut start = 0usize;
    for (i, msg) in messages.iter().enumerate().skip(1) {
        if msg.role == MessageRole::User {
            groups.push(start..i);
            start = i;
        }
    }
    groups.push(start..messages.len());
    groups
}

/// Sum cl100k tokens of text + reasoning blocks across the given message
/// range. Image/audio/file/tool_call/tool_result/redacted_reasoning/
/// unsupported blocks contribute 0 — they are not text-billed via cl100k.
pub fn count_messages_tokens(messages: &[Message]) -> u32 {
    let mut total: u32 = 0;
    for msg in messages {
        for block in &msg.content {
            match block {
                ContentBlock::Text(t) => {
                    total = total.saturating_add(agent_shim_tokens::count_text(&t.text));
                }
                ContentBlock::Reasoning(r) => {
                    total = total.saturating_add(agent_shim_tokens::count_text(&r.text));
                }
                _ => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        BinarySource, ContentBlock, ImageBlock, Message, MessageRole, ReasoningBlock, TextBlock,
    };

    fn user_text(s: &str) -> Message {
        Message::user(vec![ContentBlock::Text(TextBlock {
            text: s.to_string(),
            extensions: Default::default(),
        })])
    }

    fn assistant_text(s: &str) -> Message {
        Message::assistant(vec![ContentBlock::Text(TextBlock {
            text: s.to_string(),
            extensions: Default::default(),
        })])
    }

    #[test]
    fn empty_messages_no_groups() {
        let groups = group_boundaries(&[]);
        assert!(groups.is_empty());
    }

    #[test]
    fn single_user_message_one_group() {
        let messages = vec![user_text("hi")];
        let groups = group_boundaries(&messages);
        assert_eq!(groups, vec![0..1]);
    }

    #[test]
    fn user_assistant_pair_one_group() {
        let messages = vec![user_text("hi"), assistant_text("hello")];
        let groups = group_boundaries(&messages);
        assert_eq!(groups, vec![0..2]);
    }

    #[test]
    fn two_user_turns_two_groups() {
        let messages = vec![
            user_text("a"),
            assistant_text("b"),
            user_text("c"),
            assistant_text("d"),
        ];
        let groups = group_boundaries(&messages);
        assert_eq!(groups, vec![0..2, 2..4]);
    }

    #[test]
    fn tool_turn_grouped_with_assistant() {
        // [user, assistant(toolcall), user(toolresult), assistant, user]
        // Groups should be: [0..1, 1..4, 4..5]
        let messages = vec![
            user_text("question"),
            assistant_text("calling tool..."), // simplified — using text in lieu of ToolCall
            user_text("tool result echo"),     // user message containing what would be a tool_result
            assistant_text("answer"),
            user_text("next question"),
        ];
        let groups = group_boundaries(&messages);
        assert_eq!(groups, vec![0..1, 1..4, 4..5]);
    }

    #[test]
    fn count_messages_tokens_text_only() {
        let messages = vec![
            user_text("hello world"),
            assistant_text("hi there"),
            Message::user(vec![
                ContentBlock::Reasoning(ReasoningBlock {
                    text: "thinking...".to_string(),
                    extensions: Default::default(),
                }),
                ContentBlock::Image(ImageBlock {
                    source: BinarySource::Url {
                        url: "https://example.com/cat.png".to_string(),
                    },
                    extensions: Default::default(),
                }),
            ]),
        ];
        let n = count_messages_tokens(&messages);
        // hello world + hi there + thinking... — all positive, image contributes 0.
        // Exact count depends on cl100k tokenizer; just assert > 0 and not absurd.
        assert!(n > 0, "expected positive token count, got {n}");
        assert!(n < 100, "expected < 100 tokens, got {n}");
    }
}
```

- [ ] **Step 2: Register the submodule**

In `crates/plugins/src/builtin/prompt_compressor/mod.rs`, add (below `pub mod config;`):

```rust
pub(super) mod groups;
```

- [ ] **Step 3: Run tests**

Run: `rtk cargo nextest run -p agent-shim-plugins prompt_compressor::groups`
Expected: PASS — 6 tests.

- [ ] **Step 4: Commit**

```bash
rtk git add crates/plugins/src/builtin/prompt_compressor/groups.rs crates/plugins/src/builtin/prompt_compressor/mod.rs
rtk git commit -m "feat(plugins): prompt_compressor group_boundaries + count_messages_tokens (P06b2 T6)"
```

---

## Task 7: `drop_old_turns.rs` — first strategy

**Files:**
- Create: `crates/plugins/src/builtin/prompt_compressor/drop_old_turns.rs`
- Modify: `crates/plugins/src/builtin/prompt_compressor/mod.rs` (add `pub(super) mod drop_old_turns;` + `CompressionResult` type definition)

- [ ] **Step 1: Define `CompressionResult` in mod.rs**

In `crates/plugins/src/builtin/prompt_compressor/mod.rs`, add near the top (below `use` statements):

```rust
/// Result of applying a single strategy. `action` becomes a metric label.
pub(super) struct CompressionResult {
    pub new_messages: Vec<agent_shim_core::Message>,
    pub dropped: usize,
    pub action: &'static str,
}
```

- [ ] **Step 2: Create `drop_old_turns.rs` with tests + impl**

Create `crates/plugins/src/builtin/prompt_compressor/drop_old_turns.rs`:

```rust
//! `drop_old_turns` strategy — delete oldest user-led groups until only
//! `keep_last_n` groups remain at the tail.
//!
//! Spec §6.3.

#![cfg(feature = "prompt_compressor")]

use agent_shim_core::Message;

use super::groups::group_boundaries;
use super::CompressionResult;

pub(super) fn apply(messages: &[Message], keep_last_n: usize) -> CompressionResult {
    let groups = group_boundaries(messages);
    if groups.len() <= keep_last_n {
        return CompressionResult {
            new_messages: messages.to_vec(),
            dropped: 0,
            action: "skipped",
        };
    }
    let drop_until = groups.len() - keep_last_n;
    let cut_idx = groups[drop_until].start;
    CompressionResult {
        new_messages: messages[cut_idx..].to_vec(),
        dropped: cut_idx,
        action: "compressed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{ContentBlock, Message, TextBlock};

    fn user(s: &str) -> Message {
        Message::user(vec![ContentBlock::Text(TextBlock {
            text: s.to_string(),
            extensions: Default::default(),
        })])
    }

    fn assistant(s: &str) -> Message {
        Message::assistant(vec![ContentBlock::Text(TextBlock {
            text: s.to_string(),
            extensions: Default::default(),
        })])
    }

    #[test]
    fn drop_skipped_when_groups_le_keep_last_n() {
        let messages = vec![user("a"), assistant("b"), user("c"), assistant("d")];
        let result = apply(&messages, 2);
        assert_eq!(result.action, "skipped");
        assert_eq!(result.dropped, 0);
        assert_eq!(result.new_messages.len(), 4);
    }

    #[test]
    fn drop_keeps_exactly_n_groups() {
        // 5 groups, keep_last_n=2 -> drop oldest 3 groups (6 messages).
        let messages = vec![
            user("g1u"), assistant("g1a"),
            user("g2u"), assistant("g2a"),
            user("g3u"), assistant("g3a"),
            user("g4u"), assistant("g4a"),
            user("g5u"), assistant("g5a"),
        ];
        let result = apply(&messages, 2);
        assert_eq!(result.action, "compressed");
        assert_eq!(result.dropped, 6, "3 groups × 2 messages = 6 dropped");
        assert_eq!(result.new_messages.len(), 4, "2 groups × 2 messages = 4 kept");
    }

    #[test]
    fn drop_preserves_kept_messages_verbatim() {
        let messages = vec![user("old"), assistant("old"), user("kept"), assistant("kept")];
        let result = apply(&messages, 1);
        assert_eq!(result.action, "compressed");
        match &result.new_messages[0].content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "kept"),
            _ => panic!("unexpected block"),
        }
    }

    #[test]
    fn drop_with_only_one_group_when_keep_is_one() {
        let messages = vec![user("solo"), assistant("reply")];
        let result = apply(&messages, 1);
        assert_eq!(result.action, "skipped");
        assert_eq!(result.dropped, 0);
    }
}
```

- [ ] **Step 3: Register the submodule**

In `crates/plugins/src/builtin/prompt_compressor/mod.rs`:

```rust
pub(super) mod drop_old_turns;
```

- [ ] **Step 4: Run tests**

Run: `rtk cargo nextest run -p agent-shim-plugins prompt_compressor::drop_old_turns`
Expected: PASS — 4 tests.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/plugins/src/builtin/prompt_compressor/drop_old_turns.rs crates/plugins/src/builtin/prompt_compressor/mod.rs
rtk git commit -m "feat(plugins): prompt_compressor drop_old_turns strategy (P06b2 T7)"
```

---

## Task 8: `truncate.rs` — second strategy

**Files:**
- Create: `crates/plugins/src/builtin/prompt_compressor/truncate.rs`
- Modify: `crates/plugins/src/builtin/prompt_compressor/mod.rs` (add `pub(super) mod truncate;`)

- [ ] **Step 1: Create `truncate.rs` with tests + impl**

Create `crates/plugins/src/builtin/prompt_compressor/truncate.rs`:

```rust
//! `truncate_to_tokens` strategy — drop oldest groups until total token
//! count <= target_tokens, subject to the `keep_last_n` floor.
//!
//! Spec §6.4.

#![cfg(feature = "prompt_compressor")]

use agent_shim_core::Message;

use super::groups::{count_messages_tokens, group_boundaries};
use super::CompressionResult;

pub(super) fn apply(
    messages: &[Message],
    target_tokens: u32,
    keep_last_n: usize,
) -> CompressionResult {
    let groups = group_boundaries(messages);
    if groups.len() <= keep_last_n {
        return CompressionResult {
            new_messages: messages.to_vec(),
            dropped: 0,
            action: "skipped",
        };
    }

    // Per-group token counts (compute once).
    let group_tokens: Vec<u32> = groups
        .iter()
        .map(|r| count_messages_tokens(&messages[r.clone()]))
        .collect();
    let mut total: u32 = group_tokens.iter().copied().sum();
    if total <= target_tokens {
        return CompressionResult {
            new_messages: messages.to_vec(),
            dropped: 0,
            action: "skipped",
        };
    }

    let max_drop = groups.len() - keep_last_n;
    let mut drop_idx = 0usize;
    while total > target_tokens && drop_idx < max_drop {
        total = total.saturating_sub(group_tokens[drop_idx]);
        drop_idx += 1;
    }

    let cut_idx = groups[drop_idx].start;
    let action = if total > target_tokens {
        "compressed_but_over_budget"
    } else {
        "compressed"
    };
    CompressionResult {
        new_messages: messages[cut_idx..].to_vec(),
        dropped: cut_idx,
        action,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{ContentBlock, Message, TextBlock};

    /// Build a user message of approximately N tokens by repeating a known
    /// 4-char ASCII string. cl100k typically encodes 4 chars ≈ 1 token,
    /// so `N` chars ≈ N/4 tokens. Tests below use coarse bounds, not exact
    /// equality, to remain stable across tiktoken updates.
    fn user_of_chars(n: usize) -> Message {
        Message::user(vec![ContentBlock::Text(TextBlock {
            text: "abcd".repeat(n / 4),
            extensions: Default::default(),
        })])
    }

    fn assistant_short() -> Message {
        Message::assistant(vec![ContentBlock::Text(TextBlock {
            text: "ok".to_string(),
            extensions: Default::default(),
        })])
    }

    #[test]
    fn truncate_skipped_when_already_under_budget() {
        let messages = vec![user_of_chars(40), assistant_short()];
        // ~10 input tokens + a couple output tokens, target 10000 -> skipped.
        let result = apply(&messages, 10_000, 1);
        assert_eq!(result.action, "skipped");
    }

    #[test]
    fn truncate_skipped_when_groups_le_keep_last_n() {
        let messages = vec![
            user_of_chars(4000),
            assistant_short(),
            user_of_chars(4000),
            assistant_short(),
        ];
        // 2 groups, keep_last_n=2 -> hard floor wins, skipped regardless of budget.
        let result = apply(&messages, 10, 2);
        assert_eq!(result.action, "skipped");
        assert_eq!(result.dropped, 0);
    }

    #[test]
    fn truncate_drops_oldest_until_under_budget() {
        // 4 groups, ~100 tokens each (400 chars / ~25 tok). target=80, keep=1.
        let messages = vec![
            user_of_chars(400),
            assistant_short(),
            user_of_chars(400),
            assistant_short(),
            user_of_chars(400),
            assistant_short(),
            user_of_chars(400),
            assistant_short(),
        ];
        let result = apply(&messages, 80, 1);
        assert!(
            result.action == "compressed" || result.action == "compressed_but_over_budget",
            "expected compression, got {}",
            result.action
        );
        assert!(result.dropped > 0, "expected at least one dropped message");
    }

    #[test]
    fn truncate_compressed_but_over_budget() {
        // 3 groups all heavy; keep_last_n=1; target very small.
        let messages = vec![
            user_of_chars(400),
            assistant_short(),
            user_of_chars(400),
            assistant_short(),
            user_of_chars(400),
            assistant_short(),
        ];
        let result = apply(&messages, 1, 1);
        // Drop down to 1 group (2 messages); that group is still ~100 tokens.
        assert_eq!(result.action, "compressed_but_over_budget");
        assert_eq!(result.new_messages.len(), 2);
    }

    #[test]
    fn truncate_dropped_count_matches_message_count() {
        let messages = vec![
            user_of_chars(400),
            assistant_short(),
            user_of_chars(400),
            assistant_short(),
            user_of_chars(400),
            assistant_short(),
        ];
        let result = apply(&messages, 50, 1);
        // Dropped should be the number of messages cut off from the front.
        assert_eq!(result.dropped + result.new_messages.len(), messages.len());
    }
}
```

- [ ] **Step 2: Register the submodule**

In `crates/plugins/src/builtin/prompt_compressor/mod.rs`:

```rust
pub(super) mod truncate;
```

- [ ] **Step 3: Run tests**

Run: `rtk cargo nextest run -p agent-shim-plugins prompt_compressor::truncate`
Expected: PASS — 5 tests.

- [ ] **Step 4: Commit**

```bash
rtk git add crates/plugins/src/builtin/prompt_compressor/truncate.rs crates/plugins/src/builtin/prompt_compressor/mod.rs
rtk git commit -m "feat(plugins): prompt_compressor truncate_to_tokens strategy (P06b2 T8)"
```

---

## Task 9: `summarize.rs` — third strategy with provider call + fallback

**Files:**
- Create: `crates/plugins/src/builtin/prompt_compressor/summarize.rs`
- Modify: `crates/plugins/src/builtin/prompt_compressor/mod.rs` (add `pub(super) mod summarize;`)

- [ ] **Step 1: Create `summarize.rs` with helpers, real impl, and 7 tests**

Create `crates/plugins/src/builtin/prompt_compressor/summarize.rs`:

```rust
//! `summarize_old_turns` strategy — call a configured upstream to compress
//! the oldest groups into a single user-role summary message. On any
//! failure (Err, timeout, empty text), fall back to drop_old_turns
//! behavior so the request always proceeds with a fit-able prompt.
//!
//! Spec §6.5.

#![cfg(feature = "prompt_compressor")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;

use agent_shim_core::{
    BackendTarget, CanonicalRequest, ContentBlock, Message, StreamEvent, TextBlock,
};
use agent_shim_providers::BackendProvider;

use super::groups::group_boundaries;
use super::CompressionResult;

const SUMMARY_PROMPT_PREFIX: &str = "Summarize the prior conversation between user and assistant in <= {N} tokens.\nPreserve key facts, decisions, and unresolved questions.\nOutput plain text only, no greetings.";

const SUMMARY_INJECTION_PREFIX: &str = "[Prior conversation summary]: ";

pub(super) async fn apply(
    messages: &[Message],
    keep_last_n: usize,
    provider: &Arc<dyn BackendProvider>,
    target: &BackendTarget,
    max_summary_tokens: u32,
    timeout: Duration,
) -> CompressionResult {
    let groups = group_boundaries(messages);
    if groups.len() <= keep_last_n + 1 {
        return CompressionResult {
            new_messages: messages.to_vec(),
            dropped: 0,
            action: "skipped",
        };
    }

    let drop_count = groups.len() - keep_last_n;
    let kept_start = groups[drop_count].start;
    let serialized = serialize_groups_to_text(&messages[..kept_start]);

    let summary_req = build_summary_request(target, &serialized, max_summary_tokens);

    let start = Instant::now();
    let result = tokio::time::timeout(timeout, provider.complete(summary_req, target.clone())).await;
    let elapsed_secs = start.elapsed().as_secs_f64();

    match result {
        Ok(Ok(stream)) => {
            let summary_text = drain_text_from_stream(stream).await;
            if summary_text.is_empty() {
                metrics::histogram!(
                    agent_shim_observability::metrics::catalog::PluginPromptCompressorSummaryDurationSeconds::NAME,
                    "outcome" => "fallback",
                )
                .record(elapsed_secs);
                tracing::warn!(
                    target: "agent_shim::prompt_compressor",
                    "summarize_old_turns: empty response; falling back to drop_old_turns"
                );
                return CompressionResult {
                    new_messages: messages[kept_start..].to_vec(),
                    dropped: kept_start,
                    action: "summary_fallback",
                };
            }
            metrics::histogram!(
                agent_shim_observability::metrics::catalog::PluginPromptCompressorSummaryDurationSeconds::NAME,
                "outcome" => "success",
            )
            .record(elapsed_secs);
            let mut new_messages =
                Vec::with_capacity(1 + (messages.len() - kept_start));
            new_messages.push(Message::user(vec![ContentBlock::Text(TextBlock {
                text: format!("{SUMMARY_INJECTION_PREFIX}{summary_text}"),
                extensions: Default::default(),
            })]));
            new_messages.extend_from_slice(&messages[kept_start..]);
            CompressionResult {
                new_messages,
                dropped: kept_start,
                action: "compressed",
            }
        }
        Err(_timeout) | Ok(Err(_)) => {
            metrics::histogram!(
                agent_shim_observability::metrics::catalog::PluginPromptCompressorSummaryDurationSeconds::NAME,
                "outcome" => "fallback",
            )
            .record(elapsed_secs);
            tracing::warn!(
                target: "agent_shim::prompt_compressor",
                "summarize_old_turns: provider error or timeout; falling back to drop_old_turns"
            );
            CompressionResult {
                new_messages: messages[kept_start..].to_vec(),
                dropped: kept_start,
                action: "summary_fallback",
            }
        }
    }
}

fn build_summary_request(
    target: &BackendTarget,
    serialized_conversation: &str,
    max_tokens: u32,
) -> CanonicalRequest {
    use agent_shim_core::{
        ExtensionMap, FrontendInfo, FrontendKind, FrontendModel, GenerationOptions, RequestId,
        ResolvedPolicy,
    };
    let prompt = format!(
        "{}\n\nConversation:\n{serialized_conversation}",
        SUMMARY_PROMPT_PREFIX.replace("{N}", &max_tokens.to_string())
    );
    let mut generation = GenerationOptions::default();
    generation.max_tokens = Some(max_tokens);
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::AnthropicMessages,
            requested_model: FrontendModel::from(target.model.as_str()),
        },
        model: FrontendModel::from(target.model.as_str()),
        system: vec![],
        messages: vec![Message::user(vec![ContentBlock::Text(TextBlock {
            text: prompt,
            extensions: Default::default(),
        })])],
        tools: vec![],
        tool_choice: Default::default(),
        generation,
        response_format: None,
        stream: false,
        metadata: Default::default(),
        inbound_anthropic_headers: vec![],
        resolved_policy: ResolvedPolicy::default(),
        extensions: ExtensionMap::new(),
    }
}

async fn drain_text_from_stream(
    mut stream: agent_shim_core::CanonicalStream,
) -> String {
    let mut acc = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamEvent::TextDelta { text, .. }) => acc.push_str(&text),
            _ => {}
        }
    }
    acc
}

pub(super) fn serialize_groups_to_text(messages: &[Message]) -> String {
    use agent_shim_core::MessageRole;
    let mut out = String::new();
    for msg in messages {
        let role_label = match msg.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::Tool => "Tool",
        };
        let mut text_parts: Vec<String> = Vec::new();
        let mut other_lines: Vec<String> = Vec::new();
        for block in &msg.content {
            match block {
                ContentBlock::Text(t) => text_parts.push(t.text.clone()),
                ContentBlock::Reasoning(r) => text_parts.push(r.text.clone()),
                ContentBlock::ToolCall(c) => {
                    let args_str = match &c.arguments {
                        agent_shim_core::ToolCallArguments::Complete { value } => {
                            serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
                        }
                        agent_shim_core::ToolCallArguments::Streaming { data } => data.clone(),
                    };
                    other_lines.push(format!("[tool_call: {}({})]", c.name, args_str));
                }
                ContentBlock::ToolResult(r) => {
                    // ToolResultBlock.content is serde_json::Value — render as JSON.
                    let body = match &r.content {
                        serde_json::Value::String(s) => s.clone(),
                        v => serde_json::to_string(v).unwrap_or_else(|_| "<binary>".to_string()),
                    };
                    other_lines.push(format!("[tool_result: {body}]"));
                }
                ContentBlock::Image(_) => other_lines.push("[image omitted]".to_string()),
                ContentBlock::Audio(_) => other_lines.push("[audio omitted]".to_string()),
                ContentBlock::File(_) => other_lines.push("[file omitted]".to_string()),
                ContentBlock::RedactedReasoning(_) => {
                    other_lines.push("[redacted_reasoning omitted]".to_string());
                }
                ContentBlock::Unsupported(_) => {
                    other_lines.push("[unsupported omitted]".to_string());
                }
            }
        }
        let text_joined = text_parts.join("\n");
        out.push_str(role_label);
        out.push_str(": ");
        out.push_str(&text_joined);
        out.push('\n');
        for line in &other_lines {
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use agent_shim_core::{
        BackendTarget, CanonicalRequest, ContentBlock, Message, MessageRole, StopReason,
        StreamError, StreamEvent, TextBlock,
    };
    use agent_shim_providers::{BackendProvider, ProviderCapabilities, ProviderError};
    use async_trait::async_trait;
    use futures::stream;

    /// Test-only mock provider. Configurable behavior:
    /// - `delay`: sleep before emitting any stream events
    /// - `outcome`: Ok(text_chunks) -> emit one TextDelta per chunk; Err(_) -> return ProviderError
    struct MockProvider {
        delay: Duration,
        outcome: Result<Vec<String>, ()>,
        caps: ProviderCapabilities,
    }

    impl MockProvider {
        fn new(text_chunks: Vec<&str>) -> Self {
            Self {
                delay: Duration::ZERO,
                outcome: Ok(text_chunks.into_iter().map(|s| s.to_string()).collect()),
                caps: ProviderCapabilities::default(),
            }
        }
        fn err() -> Self {
            Self {
                delay: Duration::ZERO,
                outcome: Err(()),
                caps: ProviderCapabilities::default(),
            }
        }
        fn with_delay(mut self, d: Duration) -> Self {
            self.delay = d;
            self
        }
    }

    #[async_trait]
    impl BackendProvider for MockProvider {
        fn name(&self) -> &'static str {
            "mock"
        }
        fn capabilities(&self) -> &ProviderCapabilities {
            &self.caps
        }
        async fn complete(
            &self,
            _req: CanonicalRequest,
            _target: BackendTarget,
        ) -> Result<agent_shim_core::CanonicalStream, ProviderError> {
            tokio::time::sleep(self.delay).await;
            match &self.outcome {
                Err(()) => Err(ProviderError::Upstream {
                    status: 500,
                    body: "mock failure".to_string(),
                }),
                Ok(chunks) => {
                    let events: Vec<Result<StreamEvent, StreamError>> = chunks
                        .iter()
                        .enumerate()
                        .map(|(i, t)| {
                            Ok(StreamEvent::TextDelta {
                                index: i as u32,
                                text: t.clone(),
                            })
                        })
                        .chain(std::iter::once(Ok(StreamEvent::MessageStop {
                            stop_reason: StopReason::EndTurn,
                            stop_sequence: None,
                        })))
                        .collect();
                    let s: agent_shim_core::CanonicalStream =
                        Box::pin(stream::iter(events));
                    Ok(s)
                }
            }
        }
    }

    fn user(s: &str) -> Message {
        Message::user(vec![ContentBlock::Text(TextBlock {
            text: s.to_string(),
            extensions: Default::default(),
        })])
    }
    fn assistant(s: &str) -> Message {
        Message::assistant(vec![ContentBlock::Text(TextBlock {
            text: s.to_string(),
            extensions: Default::default(),
        })])
    }

    fn target() -> BackendTarget {
        BackendTarget {
            provider: "mock".to_string(),
            model: "test-model".to_string(),
            policy: Default::default(),
        }
    }

    fn five_groups() -> Vec<Message> {
        vec![
            user("g1u"), assistant("g1a"),
            user("g2u"), assistant("g2a"),
            user("g3u"), assistant("g3a"),
            user("g4u"), assistant("g4a"),
            user("g5u"), assistant("g5a"),
        ]
    }

    #[tokio::test]
    async fn summarize_skipped_when_too_few_groups() {
        // 2 groups, keep_last_n=2 -> skipped (need at least keep + 2 = 4 groups).
        let messages = vec![user("a"), assistant("b"), user("c"), assistant("d")];
        let p: Arc<dyn BackendProvider> = Arc::new(MockProvider::new(vec!["never"]));
        let result = apply(&messages, 2, &p, &target(), 100, Duration::from_secs(1)).await;
        assert_eq!(result.action, "skipped");
    }

    #[tokio::test]
    async fn summarize_success_replaces_old_groups() {
        let messages = five_groups();
        let p: Arc<dyn BackendProvider> = Arc::new(MockProvider::new(vec!["SUMMARY"]));
        let result = apply(&messages, 2, &p, &target(), 100, Duration::from_secs(2)).await;
        assert_eq!(result.action, "compressed");
        // Kept = 2 groups × 2 messages = 4; plus 1 summary message = 5.
        assert_eq!(result.new_messages.len(), 5);
        assert_eq!(result.new_messages[0].role, MessageRole::User);
    }

    #[tokio::test]
    async fn summarize_summary_message_has_prefix() {
        let messages = five_groups();
        let p: Arc<dyn BackendProvider> = Arc::new(MockProvider::new(vec!["MY-SUMMARY"]));
        let result = apply(&messages, 2, &p, &target(), 100, Duration::from_secs(2)).await;
        match &result.new_messages[0].content[0] {
            ContentBlock::Text(t) => {
                assert!(
                    t.text.starts_with(SUMMARY_INJECTION_PREFIX),
                    "expected prefix, got: {}",
                    t.text
                );
                assert!(t.text.contains("MY-SUMMARY"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn summarize_provider_error_falls_back() {
        let messages = five_groups();
        let p: Arc<dyn BackendProvider> = Arc::new(MockProvider::err());
        let result = apply(&messages, 2, &p, &target(), 100, Duration::from_secs(2)).await;
        assert_eq!(result.action, "summary_fallback");
        // Kept = 2 groups × 2 messages = 4 (no summary message prepended).
        assert_eq!(result.new_messages.len(), 4);
    }

    #[tokio::test]
    async fn summarize_timeout_falls_back() {
        let messages = five_groups();
        let p: Arc<dyn BackendProvider> =
            Arc::new(MockProvider::new(vec!["never seen"]).with_delay(Duration::from_secs(10)));
        let result = apply(&messages, 2, &p, &target(), 100, Duration::from_millis(5)).await;
        assert_eq!(result.action, "summary_fallback");
        assert_eq!(result.new_messages.len(), 4);
    }

    #[tokio::test]
    async fn summarize_empty_response_falls_back() {
        let messages = five_groups();
        let p: Arc<dyn BackendProvider> = Arc::new(MockProvider::new(vec![]));
        let result = apply(&messages, 2, &p, &target(), 100, Duration::from_secs(2)).await;
        assert_eq!(result.action, "summary_fallback");
    }

    #[test]
    fn serialize_groups_to_text_format() {
        let messages = vec![
            user("hi"),
            assistant("hello there"),
            user("bye"),
        ];
        let out = serialize_groups_to_text(&messages);
        assert!(out.contains("User: hi"));
        assert!(out.contains("Assistant: hello there"));
        assert!(out.contains("User: bye"));
    }
}
```

- [ ] **Step 2: Register the submodule**

In `crates/plugins/src/builtin/prompt_compressor/mod.rs`:

```rust
pub(super) mod summarize;
```

- [ ] **Step 3: Run tests**

Run: `rtk cargo nextest run -p agent-shim-plugins prompt_compressor::summarize`
Expected: PASS — 7 tests.

If `tool_call` / `tool_result` block fields don't match (the `arguments_json` / `content` accessors above were inferred from the `ContentBlock` enum), inspect `crates/core/src/tool.rs` for the correct field names and adjust `serialize_groups_to_text` accordingly.

- [ ] **Step 4: Commit**

```bash
rtk git add crates/plugins/src/builtin/prompt_compressor/summarize.rs crates/plugins/src/builtin/prompt_compressor/mod.rs
rtk git commit -m "feat(plugins): prompt_compressor summarize_old_turns + provider call + fallback (P06b2 T9)"
```

---

## Task 10: `mod.rs` — wire dispatch + metric emission

**Files:**
- Modify: `crates/plugins/src/builtin/prompt_compressor/mod.rs` (real H2 body; metric emission)

The H2 stub from T5 just returned `Ok(req)`. Now dispatch by strategy variant, get a `CompressionResult`, emit metrics, swap `req.messages`.

- [ ] **Step 1: Replace `on_decoded_request` body**

In `crates/plugins/src/builtin/prompt_compressor/mod.rs`, replace the existing stub:

```rust
    async fn on_decoded_request(
        &self,
        _ctx: &PluginContext,
        mut req: CanonicalRequest,
    ) -> PluginResult<CanonicalRequest> {
        let (result, strategy_label) = match &self.strategy {
            CompiledStrategy::DropOldTurns { keep_last_n } => (
                drop_old_turns::apply(&req.messages, *keep_last_n),
                "drop_old_turns",
            ),
            CompiledStrategy::TruncateToTokens {
                target_tokens,
                keep_last_n,
            } => (
                truncate::apply(&req.messages, *target_tokens, *keep_last_n),
                "truncate_to_tokens",
            ),
            CompiledStrategy::SummarizeOldTurns {
                keep_last_n,
                provider,
                target,
                max_summary_tokens,
                timeout,
            } => (
                summarize::apply(
                    &req.messages,
                    *keep_last_n,
                    provider,
                    target,
                    *max_summary_tokens,
                    *timeout,
                )
                .await,
                "summarize_old_turns",
            ),
        };

        metrics::counter!(
            agent_shim_observability::metrics::catalog::PluginPromptCompressorActionsTotal::NAME,
            "strategy" => strategy_label,
            "action" => result.action,
        )
        .increment(1);

        if result.action != "skipped" {
            if result.dropped > 0 {
                metrics::counter!(
                    agent_shim_observability::metrics::catalog::PluginPromptCompressorMessagesDroppedTotal::NAME,
                    "strategy" => strategy_label,
                )
                .increment(result.dropped as u64);
            }
            req.messages = result.new_messages;
        }
        Ok(req)
    }
```

- [ ] **Step 2: Add H2 integration-style tests in `mod.rs::tests`**

Append these tests at the bottom of the existing `tests` module in `mod.rs`:

```rust
    use agent_shim_core::{ContentBlock, FrontendInfo, FrontendKind, FrontendModel, Message, MessageRole, RequestId, ResolvedPolicy, TextBlock};
    use agent_shim_core::request::RequestMetadata;
    use agent_shim_core::ExtensionMap;
    use agent_shim_core::GenerationOptions;
    use agent_shim_core::CanonicalRequest;

    fn req_with(messages: Vec<Message>) -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: FrontendModel::from("test-model"),
            },
            model: FrontendModel::from("test-model"),
            system: vec![],
            messages,
            tools: vec![],
            tool_choice: Default::default(),
            generation: GenerationOptions::default(),
            response_format: None,
            stream: false,
            metadata: RequestMetadata::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: ResolvedPolicy::default(),
            extensions: ExtensionMap::new(),
        }
    }

    fn user_t(s: &str) -> Message {
        Message::user(vec![ContentBlock::Text(TextBlock {
            text: s.to_string(),
            extensions: Default::default(),
        })])
    }
    fn assistant_t(s: &str) -> Message {
        Message::assistant(vec![ContentBlock::Text(TextBlock {
            text: s.to_string(),
            extensions: Default::default(),
        })])
    }

    fn ctx() -> PluginContext {
        PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/test-model".to_string(),
        )
    }

    #[tokio::test]
    async fn h2_skipped_no_metric_emission() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let cfg = json!({
            "strategy": { "type": "drop_old_turns", "keep_last_n": 5 }
        });
        let plugin = PromptCompressorFactory
            .instantiate("p", cfg, &empty_deps())
            .unwrap();
        let req = req_with(vec![user_t("a"), assistant_t("b")]);
        let out = plugin.on_decoded_request(&ctx(), req).await.unwrap();
        // Skipped: returned unchanged.
        assert_eq!(out.messages.len(), 2);

        // Metric was emitted (action=skipped is still counted), but messages_dropped_total
        // must NOT have been incremented.
        let snapshot = snapshotter.snapshot().into_vec();
        let dropped_count: u64 = snapshot
            .iter()
            .filter_map(|(key, _, _, value)| {
                if key.key().name() == "agent_shim_plugin_prompt_compressor_messages_dropped_total"
                {
                    match value {
                        DebugValue::Counter(v) => Some(*v),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .sum();
        assert_eq!(dropped_count, 0);
    }

    #[tokio::test]
    async fn h2_compressed_emits_metrics() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let cfg = json!({
            "strategy": { "type": "drop_old_turns", "keep_last_n": 1 }
        });
        let plugin = PromptCompressorFactory
            .instantiate("p", cfg, &empty_deps())
            .unwrap();
        let messages = vec![
            user_t("old-u"), assistant_t("old-a"),
            user_t("kept-u"), assistant_t("kept-a"),
        ];
        let req = req_with(messages);
        let out = plugin.on_decoded_request(&ctx(), req).await.unwrap();
        assert_eq!(out.messages.len(), 2);

        let snapshot = snapshotter.snapshot().into_vec();
        let actions: u64 = snapshot
            .iter()
            .filter_map(|(key, _, _, value)| {
                if key.key().name() == "agent_shim_plugin_prompt_compressor_actions_total" {
                    match value {
                        DebugValue::Counter(v) => Some(*v),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .sum();
        assert_eq!(actions, 1, "one action total");

        let dropped: u64 = snapshot
            .iter()
            .filter_map(|(key, _, _, value)| {
                if key.key().name() == "agent_shim_plugin_prompt_compressor_messages_dropped_total"
                {
                    match value {
                        DebugValue::Counter(v) => Some(*v),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .sum();
        assert_eq!(dropped, 2, "two messages dropped (1 group × 2 msgs)");
    }
```

- [ ] **Step 3: Run tests**

Run: `rtk cargo nextest run -p agent-shim-plugins prompt_compressor`
Expected: PASS — all prompt_compressor unit tests pass (~31 total).

Run: `rtk cargo nextest run -p agent-shim-plugins`
Expected: PASS — full plugins crate regression.

- [ ] **Step 4: Commit**

```bash
rtk git add crates/plugins/src/builtin/prompt_compressor/mod.rs
rtk git commit -m "feat(plugins): prompt_compressor H2 dispatch + metric emission (P06b2 T10)"
```

---

## Task 11: Gateway integration test (mockito for summarize path)

**Files:**
- Create: `crates/gateway/tests/prompt_compressor_integration.rs`

Validates the full production wiring: YAML -> `AppState::new` -> `FactoryDependencies` populated -> factory looks up `summarizer` upstream -> `PromptCompressor::on_decoded_request` calls provider -> upstream `main` sees scrubbed body.

- [ ] **Step 1: Create the test file**

Create `crates/gateway/tests/prompt_compressor_integration.rs`:

```rust
//! Phase 7 P06b2: end-to-end integration test for prompt_compressor.
//!
//! Validates the full production path:
//! YAML -> AppState::new -> ProviderRegistry built before PluginRegistry ->
//! FactoryDependencies populated -> PromptCompressorFactory resolves
//! `summarizer` upstream -> H2 calls provider -> upstream `main` receives
//! a body containing the summary message.

use agent_shim_config::GatewayConfig;
use agent_shim_gateway::{server::run_on_listener, state::AppState};

const REQUEST_BODY: &str = r#"{
    "model": "test-model",
    "max_tokens": 16,
    "messages": [
        {"role": "user", "content": "msg1"},
        {"role": "assistant", "content": "reply1"},
        {"role": "user", "content": "msg2"},
        {"role": "assistant", "content": "reply2"},
        {"role": "user", "content": "msg3"},
        {"role": "assistant", "content": "reply3"},
        {"role": "user", "content": "msg4"},
        {"role": "assistant", "content": "reply4"},
        {"role": "user", "content": "msg5"},
        {"role": "assistant", "content": "reply5"},
        {"role": "user", "content": "final question"}
    ]
}"#;

const UPSTREAM_OAI_RESPONSE: &str = r#"{
    "id": "chatcmpl-test",
    "object": "chat.completion",
    "created": 1700000000,
    "model": "test-model",
    "choices": [{
        "index": 0,
        "message": {"role": "assistant", "content": "ok"},
        "finish_reason": "stop"
    }],
    "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
}"#;

const SUMMARIZER_OAI_RESPONSE: &str = r#"{
    "id": "chatcmpl-summarizer",
    "object": "chat.completion",
    "created": 1700000000,
    "model": "cheap-model",
    "choices": [{
        "index": 0,
        "message": {"role": "assistant", "content": "FAKE SUMMARY"},
        "finish_reason": "stop"
    }],
    "usage": {"prompt_tokens": 100, "completion_tokens": 5, "total_tokens": 105}
}"#;

fn yaml_for(main_url: &str, summarizer_url: &str) -> String {
    format!(
        r#"
server:
  bind: 127.0.0.1
  port: 0
  keepalive_secs: 15
upstreams:
  main:
    type: open_ai_compatible
    base_url: "{main_url}"
    api_key: "test-key"
    tier: standard
  summarizer:
    type: open_ai_compatible
    base_url: "{summarizer_url}"
    api_key: "test-key"
    tier: standard
routes:
  - frontend: anthropic_messages
    model: test-model
    upstream: main
    upstream_model: test-model
    plugins:
      on_decoded_request:
        - compressor
plugins:
  compressor:
    type: prompt_compressor
    config:
      strategy:
        type: summarize_old_turns
        keep_last_n: 2
        summarizer:
          upstream: summarizer
          model: cheap-model
          max_summary_tokens: 100
          timeout_ms: 5000
"#
    )
}

#[tokio::test]
async fn prompt_compressor_summarize_path_end_to_end() {
    let mut main_upstream = mockito::Server::new_async().await;
    let mut summarizer_upstream = mockito::Server::new_async().await;

    // Main upstream MUST see a body containing "[Prior conversation summary]: FAKE SUMMARY".
    let main_mock = main_upstream
        .mock("POST", "/v1/chat/completions")
        .match_body(mockito::Matcher::AllOf(vec![mockito::Matcher::Regex(
            r"\[Prior conversation summary\]: FAKE SUMMARY".to_string(),
        )]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(UPSTREAM_OAI_RESPONSE)
        .expect(1)
        .create_async()
        .await;

    let summarizer_mock = summarizer_upstream
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SUMMARIZER_OAI_RESPONSE)
        .expect(1)
        .create_async()
        .await;

    let cfg: GatewayConfig =
        serde_yaml::from_str(&yaml_for(&main_upstream.url(), &summarizer_upstream.url()))
            .expect("yaml parses");
    let (state, _reload_rx) = AppState::new(cfg).await.expect("AppState::new");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        let _ = run_on_listener(listener, state, async {
            let _ = rx.await;
        })
        .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url = format!("http://{}/v1/messages", addr);
    let resp = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .body(REQUEST_BODY)
        .send()
        .await
        .expect("send request");

    assert_eq!(resp.status(), 200);

    let _ = tx.send(());
    let _ = server_handle.await;

    main_mock.assert_async().await;
    summarizer_mock.assert_async().await;
}
```

- [ ] **Step 2: Run the test**

Run: `rtk cargo nextest run --test prompt_compressor_integration`
Expected: PASS — 1 test.

If the summarizer upstream call fails (e.g. because the openai-compatible provider expects a different streaming response shape), inspect `crates/providers/src/openai_compatible/` to confirm — the `complete` call drives a chat completions request and expects either streaming SSE or a unary JSON response per its config. The non-streaming JSON response in `SUMMARIZER_OAI_RESPONSE` should work; if not, switch to an SSE response body matching `crates/gateway/tests/usage_recorder_integration.rs` style.

- [ ] **Step 3: Commit**

```bash
rtk git add crates/gateway/tests/prompt_compressor_integration.rs
rtk git commit -m "test(gateway): prompt_compressor summarize-path end-to-end via mockito (P06b2 T11)"
```

---

## Task 12: Clippy + fmt + frozen-core verification

**Files:** No new code; verification only.

- [ ] **Step 1: Format**

Run: `rtk cargo fmt --all`

Verify no diffs:

Run: `rtk cargo fmt --all -- --check`
Expected: exit 0.

- [ ] **Step 2: Clippy with `-D warnings`**

Run: `rtk cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS — zero warnings.

If clippy complains about an `apply_*` helper module being unused under certain feature combinations, gate with `#[cfg(feature = "prompt_compressor")]`.

- [ ] **Step 3: Full workspace regression**

Run: `rtk cargo nextest run --workspace`
Expected: PASS — ~910 tests (878 baseline + ~31 prompt_compressor unit + 1 integration = ~910).

- [ ] **Step 4: Frozen-core invariant**

Run: `git -C Q:/src/AgentShim/.claude/worktrees/phase-7-p06b2-prompt-compressor diff master..HEAD -- crates/core/`
Expected: empty output.

If output is non-empty, find what touched core and either revert or move the change elsewhere. The plugin and the factory-signature work MUST NOT modify core types.

- [ ] **Step 5: Feature-off build check**

Run: `rtk cargo build -p agent-shim-plugins --no-default-features --features usage_recorder`
Expected: PASS — `prompt_compressor` code stripped from binary, no compile errors.

- [ ] **Step 6: Commit any fmt cleanup**

If `cargo fmt` produced diffs (it often does after large edits), commit them:

```bash
rtk git add -u
rtk git commit -m "chore: cargo fmt cleanup (P06b2 T12)"
```

If no diffs, skip the commit.

---

## Acceptance summary

After T12, the following must be true:

1. `git log --oneline 33ca539..HEAD` shows ~13 commits (12 task commits + 1 fmt cleanup if any).
2. `cargo nextest run --workspace` shows ~910 passing tests.
3. `cargo fmt --all -- --check` shows no diff.
4. `cargo clippy --workspace --all-targets -- -D warnings` returns 0.
5. `git diff master..HEAD -- crates/core/` is empty.
6. Three YAML examples in §3 of the spec each have at least one config round-trip test.
7. `crates/observability/tests/v060_metrics_baseline.json` contains 3 new entries.
8. `cargo build -p agent-shim-plugins --no-default-features --features usage_recorder` compiles cleanly.

When all 8 are satisfied, the branch is ready for `git merge --no-ff worktree-phase-7-p06b2-prompt-compressor -m "Merge Phase 7 P06b2: prompt_compressor built-in plugin"` into master.
