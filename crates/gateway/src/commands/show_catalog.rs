//! `agent-shim show-catalog` — print the model catalog for the current
//! config and exit.
//!
//! Builds an [`AppState`] from the config (which runs one round of
//! upstream model discovery as a side-effect of construction), then
//! pulls the catalog out via `state.core.resolver.list_catalog()`.
//! Renders either as a fixed-width table (default) or as the same JSON
//! shape returned by `GET /admin/catalog`.
//!
//! `--strict` exits non-zero if any route alias has no upstream
//! metadata. Useful for CI:
//!
//! ```text
//! $ agent-shim show-catalog --config config/gateway.yaml --strict
//! ```
//!
//! Reuses `AppState::new` rather than calling each provider's
//! `list_models` ad-hoc because that's the only place that knows how to
//! build the provider registry from `UpstreamConfig::*` variants.

use anyhow::Result;
use std::path::Path;

use agent_shim_core::{FrontendKind, ModelRecord};

use crate::state::AppState;

pub async fn run(config_path: &Path, format: &str, strict: bool) -> Result<()> {
    let cfg = agent_shim_config::load_from_path(config_path)
        .map_err(|e| anyhow::anyhow!("Failed to load config: {}", e))?;
    agent_shim_config::validate(&cfg)
        .map_err(|e| anyhow::anyhow!("Config validation failed: {}", e))?;

    // AppState::new constructs the provider registry from cfg.upstreams
    // and runs discovery as part of building the ModelIndex. Reuse it so
    // we don't have to maintain a parallel "build providers" code path
    // here.
    let (state, _reload_rx) = AppState::new(cfg).await?;
    let catalog = state.core.resolver.list_catalog();

    let mut missing_metadata = false;
    match format {
        "json" => {
            println!("{}", serde_json::to_string_pretty(&catalog)?);
            missing_metadata = catalog.iter().any(|r| r.metadata.is_none());
        }
        _ => render_table(&catalog, &mut missing_metadata),
    }

    if strict && missing_metadata {
        anyhow::bail!(
            "--strict: one or more routes have no upstream metadata \
             (upstream did not surface the model, or model discovery failed)"
        );
    }
    Ok(())
}

fn render_table(catalog: &[ModelRecord], missing_metadata: &mut bool) {
    println!(
        "{:<22} {:<22} {:<10} {:<30} {:<22} {:>7} {:>7} {:<6} {:<6}",
        "Frontend", "Alias", "Provider", "Upstream model", "Family", "Ctx", "Out", "Vis", "Tool"
    );
    println!("{}", "-".repeat(140));
    for r in catalog {
        let frontends = r
            .frontends
            .iter()
            .copied()
            .map(frontend_label)
            .collect::<Vec<_>>()
            .join(",");
        let (fam, ctx, out, vis, tool) = match &r.metadata {
            Some(m) => (
                m.family.clone().unwrap_or_else(|| "?".into()),
                m.context_window_tokens
                    .map(short_num)
                    .unwrap_or_else(|| "?".into()),
                m.max_output_tokens
                    .map(short_num)
                    .unwrap_or_else(|| "?".into()),
                bool_label(m.supports.vision),
                bool_label(m.supports.tool_calls),
            ),
            None => {
                *missing_metadata = true;
                ("?".into(), "?".into(), "?".into(), "?".into(), "?".into())
            }
        };
        println!(
            "{:<22} {:<22} {:<10} {:<30} {:<22} {:>7} {:>7} {:<6} {:<6}",
            frontends, r.id, r.upstream_provider, r.upstream_model, fam, ctx, out, vis, tool
        );
    }
}

fn frontend_label(kind: FrontendKind) -> &'static str {
    match kind {
        FrontendKind::AnthropicMessages => "anthropic",
        FrontendKind::OpenAiChat => "openai_chat",
        FrontendKind::OpenAiResponses => "openai_resp",
    }
}

fn bool_label(v: Option<bool>) -> String {
    match v {
        Some(true) => "yes".to_string(),
        Some(false) => "no".to_string(),
        None => "?".to_string(),
    }
}

fn short_num(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        n.to_string()
    }
}
