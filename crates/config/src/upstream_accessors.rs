//! Per-variant accessors for `UpstreamConfig`. Plan v0.6.1 P01 (M-3).
//!
//! Both the validation rules in `crate::validation` and the cost-filter
//! pass in `crates/router/src/cost_filter.rs` need to read `tier`,
//! `cost`, and `p95_latency_budget_ms` off any `UpstreamConfig` variant.
//! Without these helpers each call-site reimplements the five-way match.
//!
//! `UpstreamConfig` is owned by this crate, so the accessors naturally
//! live here. Both consumers import via `agent_shim_config::{upstream_cost,
//! upstream_tier, upstream_latency_budget}` (re-exported from the crate
//! root in `crate::lib`).

use crate::schema::{Tier, UpstreamConfig, UpstreamCost};

/// Tier label of `u`. Every upstream variant carries a non-optional tier.
pub fn upstream_tier(u: &UpstreamConfig) -> Tier {
    match u {
        UpstreamConfig::OpenAiCompatible(c) => c.tier,
        UpstreamConfig::GithubCopilot(c) => c.tier,
        UpstreamConfig::Anthropic(c) => c.tier,
        UpstreamConfig::Deepseek(c) => c.tier,
        UpstreamConfig::Gemini(c) => c.tier,
    }
}

/// Optional cost schedule. `None` means the upstream is subscription-
/// priced (Copilot) or otherwise has no per-token billing the gateway
/// can apply a cap against.
pub fn upstream_cost(u: &UpstreamConfig) -> Option<&UpstreamCost> {
    match u {
        UpstreamConfig::OpenAiCompatible(c) => c.cost.as_ref(),
        UpstreamConfig::GithubCopilot(c) => c.cost.as_ref(),
        UpstreamConfig::Anthropic(c) => c.cost.as_ref(),
        UpstreamConfig::Deepseek(c) => c.cost.as_ref(),
        UpstreamConfig::Gemini(c) => c.cost.as_ref(),
    }
}

/// Optional p95 latency budget in milliseconds. `None` means the latency
/// axis does not gate this upstream.
pub fn upstream_latency_budget(u: &UpstreamConfig) -> Option<u64> {
    match u {
        UpstreamConfig::OpenAiCompatible(c) => c.p95_latency_budget_ms,
        UpstreamConfig::GithubCopilot(c) => c.p95_latency_budget_ms,
        UpstreamConfig::Anthropic(c) => c.p95_latency_budget_ms,
        UpstreamConfig::Deepseek(c) => c.p95_latency_budget_ms,
        UpstreamConfig::Gemini(c) => c.p95_latency_budget_ms,
    }
}
