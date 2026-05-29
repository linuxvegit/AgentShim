use std::collections::BTreeMap;
use std::path::PathBuf;

use agent_shim_core::ModelMetadata;
use agent_shim_providers::github_copilot::{credential_store, models, token_manager};

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

    match format {
        "json" => render_json(&model_map)?,
        "table" => render_table(&token.api_base, &model_map),
        other => anyhow::bail!("unknown --format {other:?}; expected `table` or `json`"),
    }

    Ok(())
}

fn render_json(models: &BTreeMap<String, ModelMetadata>) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(models)?);
    Ok(())
}

fn render_table(api_base: &str, models: &BTreeMap<String, ModelMetadata>) {
    println!("Copilot API base: {api_base}");
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
}
