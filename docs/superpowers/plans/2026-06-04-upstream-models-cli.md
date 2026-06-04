# `agent-shim models <name>` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a new `agent-shim models <upstream-name>` CLI subcommand that prints the model catalog of any upstream configured in `gateway.yaml`, and fill in `list_models()` for the two providers that still default to `Ok(None)`.

**Architecture:** Lift the fixed-width table renderer out of `crates/gateway/src/commands/copilot_models.rs` into a new shared module `render_models.rs`. The new `commands::models::run` builds `AppState` (same construction path as `serve` and `show-catalog`), looks up the upstream by name in the provider registry, calls `BackendProvider::list_models()`, and renders. Anthropic and Gemini gain real `list_models()` implementations following the existing GLM / DeepSeek / OpenAI-compatible pattern.

**Tech Stack:** Rust (workspace edition 2021), `clap`, `anyhow`, `thiserror`, `reqwest` (via `crate::ProviderHttpClient`), `tokio`, `serde_json`, `mockito` (dev-dep), `cargo nextest`.

**Source spec:** `docs/superpowers/specs/2026-06-04-upstream-models-cli-design.md`

---

## File Structure

**New files:**
- `crates/gateway/src/commands/render_models.rs` — shared renderer
- `crates/gateway/src/commands/models.rs` — new subcommand entry
- `crates/gateway/tests/models_cli.rs` — function-level integration test for the new subcommand

**Modified files:**
- `crates/gateway/src/commands/mod.rs` — declare the two new modules
- `crates/gateway/src/commands/copilot_models.rs` — replace inline renderer with calls into `render_models`
- `crates/gateway/src/cli.rs` — add `Commands::Models` variant
- `crates/gateway/src/main.rs` — wire `Commands::Models` to `commands::models::run`
- `crates/providers/src/anthropic/mod.rs` — implement `list_models()`
- `crates/providers/src/gemini/mod.rs` — implement `list_models()`
- `CLAUDE.md` — add `models <name> [--format json|table]` to the CLI surface list

---

## Task 1: Lift shared renderer into `render_models.rs`

This is a pure refactor that produces no behaviour change. Done first so subsequent tasks can call into the shared module.

**Files:**
- Create: `crates/gateway/src/commands/render_models.rs`
- Modify: `crates/gateway/src/commands/mod.rs`
- Modify: `crates/gateway/src/commands/copilot_models.rs`

- [ ] **Step 1: Create `render_models.rs` with the lifted functions**

Create `crates/gateway/src/commands/render_models.rs` with the following content. This is the exact code currently in `copilot_models.rs` lines 42-148, plus a `RenderHeader` struct and a `render()` entry point that the existing six unit tests + a new one will cover.

```rust
//! Shared renderer for `BTreeMap<String, ModelMetadata>` used by both
//! `agent-shim copilot models` (device-flow path) and `agent-shim models
//! <name>` (config path). The only thing that differs between the two
//! callers is the first header line, so it becomes the only parameter.

use std::collections::BTreeMap;

use agent_shim_core::ModelMetadata;

/// Source-of-the-models metadata line printed at the top of the table
/// output. `copilot models` uses `"Copilot API base: {url}"`; `models
/// <name>` uses `"Upstream '{name}' (type: {kind})"`.
pub struct RenderHeader {
    pub line1: String,
}

/// Render the model map in the requested format. Returns `Err` only on
/// JSON serialisation failure (in practice impossible for `BTreeMap` of
/// `ModelMetadata`) or unknown `format`.
pub fn render(
    models: &BTreeMap<String, ModelMetadata>,
    format: &str,
    header: &RenderHeader,
) -> anyhow::Result<()> {
    match format {
        "json" => render_json(models),
        "table" => {
            render_table(header, models);
            Ok(())
        }
        other => anyhow::bail!("unknown --format {other:?}; expected `table` or `json`"),
    }
}

fn render_json(models: &BTreeMap<String, ModelMetadata>) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(models)?);
    Ok(())
}

fn render_table(header: &RenderHeader, models: &BTreeMap<String, ModelMetadata>) {
    println!("{}", header.line1);
    println!("{} models available\n", models.len());

    // Column widths chosen to fit Copilot's longest real ids without
    // wrapping on a typical 200-col terminal; wider terminals get
    // padding, narrower terminals will wrap (acceptable for a CLI).
    println!(
        "{:<32} {:<26} {:>8} {:>8} {:<32} {:<22} {:<6} {:<6}",
        "Model ID", "Family", "Ctx", "Out", "Effort", "Thinking", "Vis", "Tool"
    );
    println!("{}", "-".repeat(146));
    for (id, m) in models {
        let fam = m.family.as_deref().unwrap_or("?");
        let ctx = m
            .context_window_tokens
            .map(short_num)
            .unwrap_or_else(|| "?".into());
        let out = m
            .max_output_tokens
            .map(short_num)
            .unwrap_or_else(|| "?".into());
        let effort = format_effort(m);
        let thinking = format_thinking(m);
        let vis = bool_label(m.supports.vision);
        let tool = bool_label(m.supports.tool_calls);
        println!(
            "{:<32} {:<26} {:>8} {:>8} {:<32} {:<22} {:<6} {:<6}",
            id, fam, ctx, out, effort, thinking, vis, tool
        );
    }
}

/// Format the reasoning-effort column. Prefers the discovered value
/// list ("low,medium,high"); falls back to a bool label ("yes"/"no");
/// renders "—" when the upstream surfaced nothing.
fn format_effort(m: &ModelMetadata) -> String {
    if let Some(values) = m.supports.reasoning_effort_values.as_ref() {
        if !values.is_empty() {
            // Compact the longer canonical strings to fit the column
            // without losing meaning — "med" is unambiguous next to
            // "low" and "high"; "xh" is unambiguous next to "x" being
            // absent from the canonical vocabulary.
            return values
                .iter()
                .map(|v| match v.as_str() {
                    "minimal" => "min",
                    "medium" => "med",
                    "xhigh" => "xh",
                    other => other,
                })
                .collect::<Vec<_>>()
                .join(",");
        }
    }
    match m.supports.reasoning_effort {
        Some(true) => "yes".into(),
        Some(false) => "no".into(),
        None => "—".into(),
    }
}

/// Format the thinking-budget column. "adaptive(1k-32k)" when the
/// upstream advertises adaptive thinking with a numeric range; "1k-32k"
/// when only the range is known; "yes" / "no" / "—" otherwise.
fn format_thinking(m: &ModelMetadata) -> String {
    let range = match (m.thinking_budget_min, m.thinking_budget_max) {
        (Some(lo), Some(hi)) => Some(format!("{}-{}", short_num(lo), short_num(hi))),
        (Some(lo), None) => Some(format!("≥{}", short_num(lo))),
        (None, Some(hi)) => Some(format!("≤{}", short_num(hi))),
        (None, None) => None,
    };
    match (m.adaptive_thinking, range) {
        (Some(true), Some(r)) => format!("adaptive({r})"),
        (Some(true), None) => "adaptive".into(),
        (Some(false), Some(r)) => r,
        (Some(false), None) => "no".into(),
        (None, Some(r)) => r,
        (None, None) => "—".into(),
    }
}

fn bool_label(v: Option<bool>) -> String {
    match v {
        Some(true) => "yes".into(),
        Some(false) => "no".into(),
        None => "?".into(),
    }
}

/// Short integer renderer used by every numeric column. Matches the
/// `show-catalog` CLI's shape so operators reading both don't have to
/// re-parse a different scheme.
fn short_num(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::ModelSupports;

    fn meta(reasoning_values: Option<Vec<&str>>, adaptive: Option<bool>) -> ModelMetadata {
        ModelMetadata {
            context_window_tokens: Some(1_000_000),
            max_output_tokens: Some(64_000),
            family: Some("claude-opus-4.7".into()),
            supports: ModelSupports {
                reasoning_effort_values: reasoning_values
                    .map(|v| v.into_iter().map(String::from).collect()),
                ..Default::default()
            },
            adaptive_thinking: adaptive,
            thinking_budget_min: Some(1024),
            thinking_budget_max: Some(32000),
            ..Default::default()
        }
    }

    #[test]
    fn format_effort_compacts_long_values() {
        let m = meta(Some(vec!["low", "medium", "high", "xhigh"]), None);
        assert_eq!(format_effort(&m), "low,med,high,xh");
    }

    #[test]
    fn format_effort_handles_empty_list_via_bool_fallback() {
        let mut m = meta(Some(vec![]), None);
        m.supports.reasoning_effort = Some(true);
        assert_eq!(format_effort(&m), "yes");
    }

    #[test]
    fn format_effort_em_dash_when_unknown() {
        let m = meta(None, None);
        assert_eq!(format_effort(&m), "—");
    }

    #[test]
    fn format_thinking_combines_adaptive_and_range() {
        let m = meta(None, Some(true));
        assert_eq!(format_thinking(&m), "adaptive(1K-32K)");
    }

    #[test]
    fn format_thinking_range_only_when_no_adaptive_flag() {
        let m = meta(None, None);
        assert_eq!(format_thinking(&m), "1K-32K");
    }

    #[test]
    fn format_thinking_em_dash_when_truly_absent() {
        let mut m = meta(None, None);
        m.thinking_budget_min = None;
        m.thinking_budget_max = None;
        assert_eq!(format_thinking(&m), "—");
    }

    #[test]
    fn short_num_handles_thresholds() {
        assert_eq!(short_num(0), "0");
        assert_eq!(short_num(999), "999");
        assert_eq!(short_num(1000), "1K");
        assert_eq!(short_num(64_000), "64K");
        assert_eq!(short_num(1_000_000), "1.0M");
    }

    #[test]
    fn render_unknown_format_errors() {
        let m: BTreeMap<String, ModelMetadata> = BTreeMap::new();
        let h = RenderHeader { line1: "x".into() };
        let err = render(&m, "yaml", &h).unwrap_err().to_string();
        assert!(
            err.contains("unknown --format")
                && err.contains("yaml")
                && err.contains("`table` or `json`"),
            "unexpected error wording: {err}"
        );
    }
}
```

- [ ] **Step 2: Register the module**

Edit `crates/gateway/src/commands/mod.rs`. Add the new `render_models` module to the list (alphabetic order matches the existing style):

```rust
pub mod copilot_login;
pub mod copilot_models;
pub mod render_models;
pub mod serve;
pub mod show_catalog;
pub mod validate_config;

#[cfg(windows)]
pub mod service;
```

- [ ] **Step 3: Rewrite `copilot_models.rs` to use the shared renderer**

Replace the entire contents of `crates/gateway/src/commands/copilot_models.rs` with:

```rust
use std::path::PathBuf;

use agent_shim_providers::github_copilot::{credential_store, models, token_manager};

use super::render_models::{self, RenderHeader};

pub async fn run(credential_path: Option<PathBuf>, format: &str) -> anyhow::Result<()> {
    let path = match credential_path {
        Some(p) => p,
        None => credential_store::default_path()
            .map_err(|e| anyhow::anyhow!("could not determine credential path: {e}"))?,
    };

    let creds = credential_store::load(&path).map_err(|e| {
        anyhow::anyhow!(
            "failed to load credentials from {}: {e} — run `agent-shim copilot login` first",
            path.display()
        )
    })?;

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let token = token_manager::exchange_with_base(&http, &creds, "https://api.github.com")
        .await
        .map_err(|e| anyhow::anyhow!("token exchange failed: {e}"))?;

    let model_map = models::list_models(&http, &token)
        .await
        .map_err(|e| anyhow::anyhow!("failed to list models: {e}"))?;

    let header = RenderHeader {
        line1: format!("Copilot API base: {}", token.api_base),
    };
    render_models::render(&model_map, format, &header)
}
```

- [ ] **Step 4: Build and run the renderer tests**

Run: `cargo build -p agent_shim_gateway`
Expected: builds clean (no warnings — CI treats warnings as errors per CLAUDE.md).

Run: `cargo nextest run -p agent_shim_gateway render_models`
Expected: all 8 tests pass — `format_effort_compacts_long_values`, `format_effort_handles_empty_list_via_bool_fallback`, `format_effort_em_dash_when_unknown`, `format_thinking_combines_adaptive_and_range`, `format_thinking_range_only_when_no_adaptive_flag`, `format_thinking_em_dash_when_truly_absent`, `short_num_handles_thresholds`, `render_unknown_format_errors`.

- [ ] **Step 5: Verify nothing else broke**

Run: `cargo build --workspace`
Expected: workspace builds clean.

Run: `cargo nextest run -p agent_shim_gateway`
Expected: full gateway test suite passes (regressions in any pre-existing `copilot_models` test would surface here).

- [ ] **Step 6: Commit**

```bash
git add crates/gateway/src/commands/render_models.rs \
        crates/gateway/src/commands/mod.rs \
        crates/gateway/src/commands/copilot_models.rs
git commit -m "refactor: lift model-table renderer into render_models module

Extract the fixed-width BTreeMap<String, ModelMetadata> renderer out of
copilot_models.rs so the upcoming \`agent-shim models <name>\` command can
share it. Header line is the only per-caller difference, so it becomes
the only parameter. Behaviour is byte-identical."
```

---

## Task 2: Add `Commands::Models` to the CLI surface

Land the clap variant and main dispatch arm before the runtime module so subsequent tasks have a place to plug into.

**Files:**
- Modify: `crates/gateway/src/cli.rs`
- Modify: `crates/gateway/src/main.rs`

- [ ] **Step 1: Add the variant**

Edit `crates/gateway/src/cli.rs`. After the existing `ShowCatalog` variant (around line 31-43) and before `Copilot { ... }` (around line 44), insert a new variant:

```rust
    /// Print the model catalog for a single upstream named in the
    /// config file. Mirrors `copilot models` but works for every
    /// upstream type (`<name>` is the key under `cfg.upstreams`, not
    /// the provider type — so multi-upstream configs with two `glm`
    /// entries can disambiguate by `glm-prod` vs `glm-staging`).
    Models {
        /// Name of the upstream to query (must appear as a key under
        /// `upstreams` in the config file).
        name: String,
        /// Path to the config file
        #[arg(
            short,
            long,
            env = "AGENT_SHIM_CONFIG",
            default_value = "config/gateway.yaml"
        )]
        config: PathBuf,
        /// Output format: `table` (default) or `json`.
        #[arg(long, default_value = "table")]
        format: String,
    },
```

- [ ] **Step 2: Add the dispatch arm**

Edit `crates/gateway/src/main.rs`. The `match cli.command` block needs a new arm. Insert it after the `ShowCatalog { ... }` arm (around line 27) and before the `Copilot { sub }` arm (line 28):

```rust
        Commands::Models {
            name,
            config,
            format,
        } => commands::models::run(&name, &config, &format).await,
```

- [ ] **Step 3: Verify the build fails on the missing module**

Run: `cargo build -p agent_shim_gateway`
Expected: FAIL with `error[E0433]: failed to resolve: could not find \`models\` in \`commands\`` (we haven't created `commands::models` yet — this confirms the wiring is correct).

- [ ] **Step 4: Commit the failing wiring**

The next task creates the missing module. Stage and hold this commit; we'll squash if you prefer, but committing the surface first keeps the diffs reviewable.

```bash
git add crates/gateway/src/cli.rs crates/gateway/src/main.rs
git commit -m "feat(cli): add Commands::Models variant skeleton

Wire the new \`agent-shim models <name>\` subcommand into the clap parser
and main dispatch. The handler module lands next."
```

---

## Task 3: Implement `commands::models::run`

The runtime module. Drives config → AppState → registry lookup → list_models → render.

**Files:**
- Create: `crates/gateway/src/commands/models.rs`
- Modify: `crates/gateway/src/commands/mod.rs`

- [ ] **Step 1: Register the new module**

Edit `crates/gateway/src/commands/mod.rs`. Add `pub mod models;` in alphabetic order:

```rust
pub mod copilot_login;
pub mod copilot_models;
pub mod models;
pub mod render_models;
pub mod serve;
pub mod show_catalog;
pub mod validate_config;

#[cfg(windows)]
pub mod service;
```

- [ ] **Step 2: Create `models.rs` with the runtime + structured tests**

Create `crates/gateway/src/commands/models.rs` with the following content. The `provider_kind_label` helper produces the variant tag used in the error string for `Ok(None)` — keep it in sync with the variant set if a new `UpstreamConfig` variant lands.

```rust
//! `agent-shim models <name>` — print the model catalog for one upstream
//! configured in `gateway.yaml`. Mirrors `copilot models` shape for every
//! provider type without requiring device-flow credentials.
//!
//! Reuses `AppState::new` for provider registry construction — the same
//! code path as `serve` and `show-catalog`. Single source of truth at
//! the cost of running discovery for every other upstream during a
//! one-shot CLI invocation; this is the price of not maintaining a
//! parallel match-on-`UpstreamConfig` factory.

use std::path::Path;

use anyhow::Result;

use agent_shim_config::UpstreamConfig;

use super::render_models::{self, RenderHeader};
use crate::state::AppState;

pub async fn run(name: &str, config_path: &Path, format: &str) -> Result<()> {
    let cfg = agent_shim_config::load_from_path(config_path)
        .map_err(|e| anyhow::anyhow!("Failed to load config: {}", e))?;
    agent_shim_config::validate(&cfg)
        .map_err(|e| anyhow::anyhow!("Config validation failed: {}", e))?;

    // Look up the kind label first (before consuming cfg into AppState)
    // so the not-found error can list it cleanly.
    let kind = cfg.upstreams.get(name).map(provider_kind_label);
    let available = available_names(&cfg.upstreams);

    let (state, _reload_rx) = AppState::new(cfg).await?;

    let provider = state
        .core
        .providers
        .get(name)
        .ok_or_else(|| missing_upstream_error(name, &available))?;

    let map = provider
        .list_models()
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "upstream '{name}' (provider kind '{}') does not support model discovery",
                kind.unwrap_or("unknown")
            )
        })?;

    let header = RenderHeader {
        line1: format!(
            "Upstream '{name}' (type: {})",
            kind.unwrap_or("unknown")
        ),
    };
    render_models::render(&map, format, &header)
}

/// Variant tag matching the `#[serde(tag = "type", rename_all = "snake_case")]`
/// shape on `UpstreamConfig`. The match is exhaustive on purpose — adding
/// a new variant should force a compile error here so the operator-facing
/// error string stays accurate.
fn provider_kind_label(cfg: &UpstreamConfig) -> &'static str {
    match cfg {
        UpstreamConfig::OpenAiCompatible(_) => "openai_compatible",
        UpstreamConfig::GithubCopilot(_) => "github_copilot",
        UpstreamConfig::Anthropic(_) => "anthropic",
        UpstreamConfig::Deepseek(_) => "deepseek",
        UpstreamConfig::Gemini(_) => "gemini",
        UpstreamConfig::Glm(_) => "glm",
    }
}

fn available_names(
    upstreams: &std::collections::BTreeMap<String, UpstreamConfig>,
) -> Vec<String> {
    // BTreeMap iteration is already sorted; collect into Vec so callers
    // can format the comma-separated list directly.
    upstreams.keys().cloned().collect()
}

fn missing_upstream_error(name: &str, available: &[String]) -> anyhow::Error {
    let available_str = if available.is_empty() {
        "none".to_string()
    } else {
        available.join(", ")
    };
    let mut msg = format!("no upstream named '{name}' in config (available: {available_str})");
    if name == "copilot" {
        msg.push_str(
            "\n  (hint: `agent-shim copilot models` uses the device-flow credential \
             and does not require a config entry)",
        );
    }
    anyhow::anyhow!(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_config::{
        AnthropicUpstream, DeepseekUpstream, GeminiUpstream, GithubCopilotUpstream, GlmUpstream,
        OpenAiCompatibleUpstream, Secret, Tier, UpstreamConfig,
    };
    use std::collections::BTreeMap;

    fn openai_compat() -> UpstreamConfig {
        UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: "https://api.example.com".into(),
            api_key: Secret::new("k".into()),
            default_headers: Default::default(),
            request_timeout_secs: 30,
            tier: Tier::Standard,
            cost: None,
            p95_latency_budget_ms: None,
        })
    }

    #[test]
    fn provider_kind_label_covers_every_variant() {
        // Exhaustive check by variant constructor — if a new variant is
        // added without updating provider_kind_label, this test forces
        // a compile error via the match in provider_kind_label itself.
        assert_eq!(provider_kind_label(&openai_compat()), "openai_compatible");
        assert_eq!(
            provider_kind_label(&UpstreamConfig::GithubCopilot(GithubCopilotUpstream {
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            })),
            "github_copilot"
        );
        assert_eq!(
            provider_kind_label(&UpstreamConfig::Anthropic(AnthropicUpstream {
                base_url: "https://api.anthropic.com".into(),
                api_key: Secret::new("k".into()),
                anthropic_version: "2023-06-01".into(),
                default_headers: Default::default(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            })),
            "anthropic"
        );
        assert_eq!(
            provider_kind_label(&UpstreamConfig::Deepseek(DeepseekUpstream {
                base_url: "https://api.deepseek.com".into(),
                api_key: Secret::new("k".into()),
                default_headers: Default::default(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            })),
            "deepseek"
        );
        assert_eq!(
            provider_kind_label(&UpstreamConfig::Gemini(GeminiUpstream {
                base_url: "https://generativelanguage.googleapis.com".into(),
                api_key: Secret::new("k".into()),
                default_headers: Default::default(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            })),
            "gemini"
        );
        assert_eq!(
            provider_kind_label(&UpstreamConfig::Glm(GlmUpstream {
                base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
                api_key: Secret::new("k".into()),
                default_headers: Default::default(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            })),
            "glm"
        );
    }

    #[test]
    fn missing_upstream_error_includes_available_names_sorted() {
        let err = missing_upstream_error("nope", &["a".into(), "b".into()]).to_string();
        assert!(err.contains("no upstream named 'nope'"), "{err}");
        assert!(err.contains("available: a, b"), "{err}");
    }

    #[test]
    fn missing_upstream_error_empty_available_says_none() {
        let err = missing_upstream_error("nope", &[]).to_string();
        assert!(err.contains("available: none"), "{err}");
    }

    #[test]
    fn missing_upstream_error_copilot_includes_device_flow_hint() {
        let err = missing_upstream_error("copilot", &["glm".into()]).to_string();
        assert!(err.contains("`agent-shim copilot models`"), "{err}");
        assert!(err.contains("device-flow"), "{err}");
    }

    #[test]
    fn missing_upstream_error_non_copilot_omits_hint() {
        let err = missing_upstream_error("glm-typo", &["glm".into()]).to_string();
        assert!(!err.contains("device-flow"), "{err}");
    }

    #[test]
    fn available_names_returns_sorted_keys() {
        let mut upstreams: BTreeMap<String, UpstreamConfig> = BTreeMap::new();
        upstreams.insert("zeta".into(), openai_compat());
        upstreams.insert("alpha".into(), openai_compat());
        let names = available_names(&upstreams);
        // BTreeMap iteration is already sorted lexicographically.
        assert_eq!(names, vec!["alpha".to_string(), "zeta".to_string()]);
    }
}
```

> **Note on test imports:** `Secret::new` may not be the exact constructor name in `agent-shim-config`. Verify with `rg "impl.*Secret" crates/config/src/secrets.rs` before running the tests; if the field is `Secret(String)` tuple, use `Secret("k".into())` instead. Likewise verify the `Tier` variants — replace `Tier::Standard` with whatever the actual default tier variant is (likely `Tier::Cloud` or similar). These tests only check string formatting, so the choice doesn't matter functionally as long as it compiles.

- [ ] **Step 3: Verify the build now compiles**

Run: `cargo build -p agent_shim_gateway`
Expected: builds clean.

If `Secret::new` / `Tier::Standard` don't compile, fix per the Step 2 note and rebuild.

- [ ] **Step 4: Run the new unit tests**

Run: `cargo nextest run -p agent_shim_gateway commands::models`
Expected: all six unit tests pass:
- `provider_kind_label_covers_every_variant`
- `missing_upstream_error_includes_available_names_sorted`
- `missing_upstream_error_empty_available_says_none`
- `missing_upstream_error_copilot_includes_device_flow_hint`
- `missing_upstream_error_non_copilot_omits_hint`
- `available_names_returns_sorted_keys`

- [ ] **Step 5: Smoke-test the CLI surface**

Run: `cargo run -p agent_shim_gateway -- models --help`
Expected output contains:
```
Print the model catalog for a single upstream named in the
config file.

Usage: agent-shim models [OPTIONS] <NAME>

Arguments:
  <NAME>  Name of the upstream to query ...
```

Run: `cargo run -p agent_shim_gateway -- models nonexistent --config /tmp/does-not-exist.yaml`
Expected: exits with `Failed to load config: ...` (the config-not-found path, not the upstream-not-found path — that's expected since we never get to the registry lookup).

- [ ] **Step 6: Commit**

```bash
git add crates/gateway/src/commands/mod.rs \
        crates/gateway/src/commands/models.rs
git commit -m "feat(cli): implement \`agent-shim models <name>\` runtime

Loads config via the same AppState::new path used by serve/show-catalog,
looks up the upstream in the provider registry, calls list_models(), and
renders via the shared render_models module. Error wording per spec
§8 — including the device-flow hint when the missing name is exactly
'copilot'."
```

---

## Task 4: Implement `list_models()` for Anthropic

Adds a real `list_models()` to the Anthropic backend provider so `agent-shim models <anthropic-name>` returns IDs instead of the "does not support discovery" message.

**Files:**
- Modify: `crates/providers/src/anthropic/mod.rs`

- [ ] **Step 1: Write the failing tests**

Append to `crates/providers/src/anthropic/mod.rs` inside (or extending) the existing `#[cfg(test)] mod tests` block. If there is no test module yet, add one at the bottom of the file:

```rust
#[cfg(test)]
mod list_models_tests {
    use super::*;

    fn provider(server_url: String) -> AnthropicProvider {
        AnthropicProvider::new(
            "test",
            server_url,
            "test-key",
            "2023-06-01",
            Default::default(),
            30,
        )
        .expect("provider construction must succeed")
    }

    #[tokio::test]
    async fn list_models_returns_discovered_models() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/v1/models")
            .match_header("x-api-key", "test-key")
            .match_header("anthropic-version", "2023-06-01")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "data": [
                        {"id": "claude-opus-4-7", "type": "model"},
                        {"id": "claude-sonnet-4-7", "type": "model"}
                    ]
                }"#,
            )
            .create_async()
            .await;

        let p = provider(server.url());
        let map = p.list_models().await.unwrap().unwrap();
        assert!(map.contains_key("claude-opus-4-7"));
        assert!(map.contains_key("claude-sonnet-4-7"));
        assert_eq!(map.len(), 2);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_models_returns_none_on_404() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/v1/models")
            .with_status(404)
            .with_body("not found")
            .create_async()
            .await;

        let p = provider(server.url());
        assert!(p.list_models().await.unwrap().is_none());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_models_returns_none_on_401() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/v1/models")
            .with_status(401)
            .with_body(r#"{"error":"unauthorized"}"#)
            .create_async()
            .await;

        let p = provider(server.url());
        assert!(p.list_models().await.unwrap().is_none());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_models_empty_data_returns_none() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/v1/models")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data": []}"#)
            .create_async()
            .await;

        let p = provider(server.url());
        assert!(p.list_models().await.unwrap().is_none());
        mock.assert_async().await;
    }
}
```

- [ ] **Step 2: Verify tests fail**

Run: `cargo nextest run -p agent-shim-providers list_models_tests`
Expected: FAIL — the default `list_models()` returns `Ok(None)`, so `list_models_returns_discovered_models` will fail on the `.unwrap().unwrap()` (None unwrap panic) and the others will pass vacuously. The discovery test failing is what proves we're about to write real code.

- [ ] **Step 3: Implement `list_models()`**

In `crates/providers/src/anthropic/mod.rs`, inside `impl BackendProvider for AnthropicProvider`, immediately after the `complete()` method (before `proxy_raw()`), add:

```rust
    async fn list_models(
        &self,
    ) -> Result<
        Option<std::collections::BTreeMap<String, agent_shim_core::ModelMetadata>>,
        ProviderError,
    > {
        let url = format!("{}/v1/models", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .get(&url)
            .header("x-api-key", self.api_key.as_str())
            .header("anthropic-version", self.anthropic_version.as_str())
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Ok(None);
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Decode(e.to_string()))?;

        let models = body
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|id| id.as_str()).map(String::from))
                    .map(|id| (id, agent_shim_core::ModelMetadata::default()))
                    .collect::<std::collections::BTreeMap<String, agent_shim_core::ModelMetadata>>()
            })
            .unwrap_or_default();

        if models.is_empty() {
            return Ok(None);
        }
        Ok(Some(models))
    }
```

- [ ] **Step 4: Verify tests pass**

Run: `cargo nextest run -p agent-shim-providers list_models_tests`
Expected: all four tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/anthropic/mod.rs
git commit -m "feat(anthropic): implement BackendProvider::list_models

GET {base_url}/v1/models with x-api-key + anthropic-version headers.
Non-2xx returns Ok(None) so the new \`agent-shim models <name>\` CLI
surfaces a clean error instead of leaking the HTTP failure."
```

---

## Task 5: Implement `list_models()` for Gemini

Mirrors Task 4 but with Gemini's auth (`?key=...` query parameter via `AiStudioAuth`) and response shape (`models[].name` prefixed with `"models/"`).

**Files:**
- Modify: `crates/providers/src/gemini/mod.rs`

- [ ] **Step 1: Write the failing tests**

Append a new test module to `crates/providers/src/gemini/mod.rs` (the file already has a `#[cfg(test)] mod tests`; add as a sibling at the bottom of the file or inside the existing module — sibling is cleaner because the imports are scoped):

```rust
#[cfg(test)]
mod list_models_tests {
    use super::*;

    fn provider(server_url: String) -> GeminiProvider {
        GeminiProvider::new(
            "test",
            server_url,
            "AIza-test-key",
            Default::default(),
            30,
        )
        .expect("provider construction must succeed")
    }

    #[tokio::test]
    async fn list_models_returns_discovered_models_stripping_prefix() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/v1beta/models")
            .match_query(mockito::Matcher::UrlEncoded("key".into(), "AIza-test-key".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "models": [
                        {"name": "models/gemini-2.5-pro"},
                        {"name": "models/gemini-2.5-flash"}
                    ]
                }"#,
            )
            .create_async()
            .await;

        let p = provider(server.url());
        let map = p.list_models().await.unwrap().unwrap();
        // The "models/" prefix must be stripped — operators configure
        // routes against the bare model name.
        assert!(map.contains_key("gemini-2.5-pro"));
        assert!(map.contains_key("gemini-2.5-flash"));
        assert_eq!(map.len(), 2);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_models_returns_none_on_404() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/v1beta/models")
            .with_status(404)
            .create_async()
            .await;

        let p = provider(server.url());
        assert!(p.list_models().await.unwrap().is_none());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_models_returns_none_on_401() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/v1beta/models")
            .with_status(401)
            .create_async()
            .await;

        let p = provider(server.url());
        assert!(p.list_models().await.unwrap().is_none());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_models_empty_returns_none() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/v1beta/models")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"models": []}"#)
            .create_async()
            .await;

        let p = provider(server.url());
        assert!(p.list_models().await.unwrap().is_none());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_models_skips_entries_with_no_models_prefix() {
        // Defensive: AI Studio always prefixes with "models/", but if a
        // future entry ever omits it we don't want to silently produce a
        // misleading bare ID. Filter those out rather than guessing.
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/v1beta/models")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "models": [
                        {"name": "models/gemini-2.5-pro"},
                        {"name": "weird-no-prefix"}
                    ]
                }"#,
            )
            .create_async()
            .await;

        let p = provider(server.url());
        let map = p.list_models().await.unwrap().unwrap();
        assert!(map.contains_key("gemini-2.5-pro"));
        assert!(!map.contains_key("weird-no-prefix"));
        assert_eq!(map.len(), 1);
        mock.assert_async().await;
    }
}
```

- [ ] **Step 2: Verify tests fail**

Run: `cargo nextest run -p agent-shim-providers list_models_tests`
Expected: FAIL — `list_models_returns_discovered_models_stripping_prefix` panics on `.unwrap().unwrap()` because the default impl returns `None`.

- [ ] **Step 3: Implement `list_models()`**

In `crates/providers/src/gemini/mod.rs`, inside `impl BackendProvider for GeminiProvider`, **replace** the existing comment block at line 203-205:

```rust
    // No `list_models` — AI Studio's Models API exists but we don't surface
    // it today; defaulting to None matches the deepseek/openai-compatible
    // behaviour and lets the gateway fall back to the configured aliases.
```

with the real implementation:

```rust
    async fn list_models(
        &self,
    ) -> Result<
        Option<std::collections::BTreeMap<String, agent_shim_core::ModelMetadata>>,
        ProviderError,
    > {
        let url = format!("{}/v1beta/models", self.base_url.trim_end_matches('/'));
        let resp = self
            .auth
            .apply(self.client.get(&url))
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Ok(None);
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Decode(e.to_string()))?;

        // Gemini's shape: { "models": [{ "name": "models/<id>", ... }, ...] }
        // Strip the "models/" prefix so the IDs match what the route
        // aliases reference. Entries that don't have the prefix are
        // skipped rather than passed through bare — see test
        // list_models_skips_entries_with_no_models_prefix for rationale.
        let models = body
            .get("models")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                    .filter_map(|name| name.strip_prefix("models/").map(String::from))
                    .map(|id| (id, agent_shim_core::ModelMetadata::default()))
                    .collect::<std::collections::BTreeMap<String, agent_shim_core::ModelMetadata>>()
            })
            .unwrap_or_default();

        if models.is_empty() {
            return Ok(None);
        }
        Ok(Some(models))
    }
```

- [ ] **Step 4: Verify tests pass**

Run: `cargo nextest run -p agent-shim-providers list_models_tests`
Expected: all five tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/gemini/mod.rs
git commit -m "feat(gemini): implement BackendProvider::list_models

GET {base_url}/v1beta/models with the API key as a ?key= query
parameter (via AiStudioAuth::apply). Strips the \`models/\` prefix from
each entry's name field so IDs match what the route aliases reference.
Non-2xx returns Ok(None)."
```

---

## Task 6: End-to-end integration test

Tests the full `commands::models::run` path against mockito-backed upstreams. Function-level rather than spawned-process — avoids the `assert_cmd` dep, runs in-process under `tokio::test`.

**Files:**
- Create: `crates/gateway/tests/models_cli.rs`

- [ ] **Step 1: Add the integration test**

Create `crates/gateway/tests/models_cli.rs`:

```rust
//! End-to-end coverage for `commands::models::run`. Spins up a mockito
//! server, writes a config file pointing at it, then calls `run()`
//! directly. Captures stdout via a one-shot temp file redirect for the
//! json/table cases; the error-path tests don't need stdout and just
//! match on the anyhow error string.

use std::io::Write;
use std::path::PathBuf;

use agent_shim_gateway::commands::models;
use tempfile::NamedTempFile;

fn write_config(yaml: &str) -> NamedTempFile {
    let mut file = NamedTempFile::new().expect("tempfile creation");
    file.write_all(yaml.as_bytes())
        .expect("config write");
    file
}

fn base_config(upstream_name: &str, base_url: &str) -> String {
    // Minimum-viable config that passes validation and has exactly one
    // upstream pointing at the mockito server. Routes are required by
    // validation, so add a trivial passthrough alias.
    format!(
        r#"
server:
  bind_address: "127.0.0.1:0"
  request_timeout_secs: 30
  sse_keepalive_secs: 15

upstreams:
  {upstream_name}:
    type: openai_compatible
    base_url: "{base_url}"
    api_key: "test-key"
    tier: standard

routes:
  - alias: "test-model"
    frontends: [openai_chat]
    upstreams:
      - upstream: {upstream_name}
        model: "test-model-upstream"
"#
    )
}

#[tokio::test]
async fn unknown_upstream_errors_with_available_list() {
    let mut server = mockito::Server::new_async().await;
    // The /models endpoint won't actually be called for the unknown-name
    // path, but AppState::new runs discovery on all configured
    // upstreams. Provide a 404 so discovery completes cleanly.
    let _mock = server
        .mock("GET", "/v1/models")
        .with_status(404)
        .create_async()
        .await;

    let cfg = base_config("my-upstream", &server.url());
    let cfg_file = write_config(&cfg);

    let err = models::run("nonexistent", cfg_file.path(), "table")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no upstream named 'nonexistent'"), "{err}");
    assert!(err.contains("available: my-upstream"), "{err}");
}

#[tokio::test]
async fn missing_copilot_includes_device_flow_hint() {
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock("GET", "/v1/models")
        .with_status(404)
        .create_async()
        .await;

    let cfg = base_config("my-upstream", &server.url());
    let cfg_file = write_config(&cfg);

    let err = models::run("copilot", cfg_file.path(), "table")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("`agent-shim copilot models`"), "{err}");
    assert!(err.contains("device-flow"), "{err}");
}

#[tokio::test]
async fn happy_path_table_format_renders() {
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock("GET", "/v1/models")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "data": [
                    {"id": "model-a", "object": "model"},
                    {"id": "model-b", "object": "model"}
                ]
            }"#,
        )
        .create_async()
        .await;

    let cfg = base_config("my-upstream", &server.url());
    let cfg_file = write_config(&cfg);

    // Smoke: the call returns Ok. Stdout capture under tokio::test is
    // awkward; the render_models unit tests cover the formatting, so
    // here we only need the path-from-CLI-to-renderer to not error.
    models::run("my-upstream", cfg_file.path(), "table")
        .await
        .expect("run should succeed against mocked 200");
}

#[tokio::test]
async fn happy_path_json_format_renders() {
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock("GET", "/v1/models")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"data":[{"id":"model-a"}]}"#)
        .create_async()
        .await;

    let cfg = base_config("my-upstream", &server.url());
    let cfg_file = write_config(&cfg);

    models::run("my-upstream", cfg_file.path(), "json")
        .await
        .expect("run should succeed");
}

#[tokio::test]
async fn unknown_format_errors() {
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock("GET", "/v1/models")
        .with_status(200)
        .with_body(r#"{"data":[{"id":"x"}]}"#)
        .create_async()
        .await;

    let cfg = base_config("my-upstream", &server.url());
    let cfg_file = write_config(&cfg);

    let err = models::run("my-upstream", cfg_file.path(), "yaml")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown --format"), "{err}");
    assert!(err.contains("`table` or `json`"), "{err}");
}
```

> **Notes on minor risks while writing this test:**
> - `agent_shim_gateway::commands::models` must be reachable as a library item. The crate already has `name = "agent_shim_gateway"` for its lib (per Cargo.toml line 9). If `commands` is not `pub` in `lib.rs`, expose it with `pub mod commands;` — most other tests in `crates/gateway/tests/*.rs` already touch `agent_shim_gateway::...` so this should be in place.
> - If `base_config` fails validation (route shape mismatch, missing field), check `crates/config/src/validation.rs` for the canonical example shape and adjust. The existing `crates/gateway/tests/serve_core_smoke.rs` is a good reference for a minimal valid config.
> - `tempfile` is already a workspace dev-dep (per gateway Cargo.toml line 73).

- [ ] **Step 2: Run the integration tests**

Run: `cargo nextest run -p agent_shim_gateway --test models_cli`
Expected: all five tests pass:
- `unknown_upstream_errors_with_available_list`
- `missing_copilot_includes_device_flow_hint`
- `happy_path_table_format_renders`
- `happy_path_json_format_renders`
- `unknown_format_errors`

If `base_config` fails validation, the happy-path tests will error with a "Config validation failed" message rather than the expected behaviour — read `crates/config/src/validation.rs` and `crates/gateway/tests/serve_core_smoke.rs` to fix the YAML, then re-run.

- [ ] **Step 3: Commit**

```bash
git add crates/gateway/tests/models_cli.rs
git commit -m "test: integration coverage for \`agent-shim models <name>\`

Function-level tests against commands::models::run with a mockito-backed
openai_compatible upstream. Covers: unknown-name error wording (with
sorted available list), the copilot device-flow hint, happy-path
table+json renders, and unknown --format error wording."
```

---

## Task 7: Update CLAUDE.md CLI surface line

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Find the existing CLI surface line**

The current line in `CLAUDE.md` reads:

```
CLI surface: `serve`, `validate-config`, `show-catalog [--format json|table] [--strict]`, `copilot login|models [--format json|table]`, `service install|...` (Windows).
```

- [ ] **Step 2: Insert the new subcommand**

Edit `CLAUDE.md`. Replace the CLI surface line with:

```
CLI surface: `serve`, `validate-config`, `show-catalog [--format json|table] [--strict]`, `models <name> [--format json|table]`, `copilot login|models [--format json|table]`, `service install|...` (Windows).
```

The new entry sits between `show-catalog` and `copilot` because it's the cross-provider per-upstream counterpart to `show-catalog`'s whole-catalog view.

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md
git commit -m "docs(claude-md): list \`models <name>\` in the CLI surface line"
```

---

## Task 8: Workspace-wide green build

Final verification gate. Catches anything the per-task `cargo nextest` runs missed.

- [ ] **Step 1: Run formatter check**

Run: `cargo fmt --all -- --check`
Expected: clean (no output).

If it complains, run `cargo fmt --all` and re-run the check.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean — CI treats warnings as errors per CLAUDE.md.

Fix any warnings before continuing. Common ones to expect in this PR:
- Unused import in `copilot_models.rs` after removing the inline renderer
- Unused `_` binding in models.rs if AppState's reload receiver isn't named

- [ ] **Step 3: Full workspace test run**

Run: `cargo nextest run --workspace`
Expected: full green. The new tests added in this plan:
- 8 in `gateway::commands::render_models::tests`
- 6 in `gateway::commands::models::tests`
- 4 in `providers::anthropic::list_models_tests`
- 5 in `providers::gemini::list_models_tests`
- 5 in `gateway::tests::models_cli`

Plus all existing tests must still pass — the renderer-lift in Task 1 was intended to be byte-identical.

- [ ] **Step 4: Manual smoke check (optional)**

If you have a real `config/gateway.yaml`:

Run: `cargo run -p agent_shim_gateway -- models <some-upstream-name>`
Expected: prints the table; or, if the upstream doesn't return a `/models` shape, prints the "does not support discovery" error cleanly.

- [ ] **Step 5: No commit needed** — Task 8 is verification-only.

---

## Self-Review

**Spec coverage:**
- §3 CLI surface → Task 2 (clap variant), Task 7 (CLAUDE.md update).
- §4 execution path → Task 3 (`commands::models::run`).
- §5 shared renderer → Task 1 (lift), Task 1 Step 3 (copilot_models.rs reduction).
- §6 provider work → Task 4 (anthropic), Task 5 (gemini); openai_compat / glm / deepseek / copilot already implemented.
- §7 wiring → Task 2 (cli.rs + main.rs), Task 3 Step 1 (mod.rs).
- §8 error wording → Task 3 Step 2 (`missing_upstream_error`, `Ok(None)` branch), Task 3 Step 4 (`missing_upstream_error_*` tests), Task 1 (`render_unknown_format_errors` test).
- §9 test plan → Task 1 Step 4 (migrated renderer tests), Tasks 4-5 (per-provider mockito), Task 6 (CLI integration).

**Placeholder scan:** no TBD/TODO/"add appropriate error handling"; every code step has the actual code.

**Type consistency:** `RenderHeader { line1: String }` is consistent across Tasks 1 and 3. `provider_kind_label` returns `&'static str` and is called consistently. `commands::models::run(&str, &Path, &str)` signature matches the dispatch arm in `main.rs` (Task 2).

**One known unknown (called out inline):** the exact `Secret::new` / `Tier::Standard` symbols used in Task 3 Step 2's unit tests — flagged with a verification command. Construction args for non-default-equipped types may need a one-line adjustment after a `grep`. The behaviour under test (error string formatting) is unaffected.
