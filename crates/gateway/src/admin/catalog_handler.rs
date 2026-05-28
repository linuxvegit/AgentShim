//! GET /admin/catalog — operator catalog with policy and metadata.
//!
//! Same underlying [`ModelCatalog`] as `/v1/models` but rendered with
//! operator-only extensions: per-route reasoning effort policy (default
//! effort, mapping rules), the resolved upstreams chain, and the full
//! metadata blob (including `version`, supports-array) rather than the
//! trimmed public capability shape.
//!
//! Not auth-gated separately — the admin listener already has its own
//! firewall / network policy story (Plan 01 spec D10).

use axum::{extract::State, response::IntoResponse, Json};
use serde::Serialize;

use crate::state::AppState;

#[derive(Debug, Serialize)]
struct AdminCatalogResponse {
    routes: Vec<AdminRecord>,
}

#[derive(Debug, Serialize)]
struct AdminRecord {
    id: String,
    frontends: Vec<&'static str>,
    upstream_provider: String,
    upstream_model: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    upstreams_chain: Vec<UpstreamRefDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<agent_shim_core::ModelMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    long_context_variant: Option<String>,
    /// Surfaced from the route's resolved [`RoutePolicy`]. Plan A
    /// (`2026-05-28-reasoning-effort-mapping`) has landed, so this is
    /// populated whenever the route configured a `reasoning_mapping` block.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reasoning_mapping: Vec<MappingDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_reasoning_effort: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct UpstreamRefDto {
    provider: String,
    model: String,
}

#[derive(Debug, Serialize)]
struct MappingDto {
    r#match: &'static str,
    set: &'static str,
}

fn frontend_kind_str(kind: agent_shim_core::FrontendKind) -> &'static str {
    match kind {
        agent_shim_core::FrontendKind::AnthropicMessages => "anthropic_messages",
        agent_shim_core::FrontendKind::OpenAiChat => "openai_chat",
        agent_shim_core::FrontendKind::OpenAiResponses => "openai_responses",
    }
}

pub async fn handle(State(state): State<AppState>) -> impl IntoResponse {
    let catalog = state.core.resolver.list_catalog();
    let routes = catalog
        .into_iter()
        .map(|r| {
            // Recover policy by re-resolving the alias on its first frontend.
            // The chain head's `policy` carries `default_reasoning_effort`
            // and `reasoning_mapping`. If the alias somehow can't resolve
            // (shouldn't happen — it came from list_routes), drop the
            // policy extras gracefully.
            let first_frontend = r.frontends.first().copied();
            let policy = first_frontend
                .and_then(|f| state.core.resolver.resolve(f, &r.id).ok())
                .and_then(|chain| chain.into_iter().next().map(|t| t.policy));
            let (mapping, default_effort) = match policy {
                Some(p) => (
                    p.reasoning_mapping
                        .iter()
                        .map(|m| MappingDto {
                            r#match: m.r#match.as_str(),
                            set: m.set.as_str(),
                        })
                        .collect(),
                    p.default_reasoning_effort.map(|e| e.as_str()),
                ),
                None => (Vec::new(), None),
            };
            let upstreams_chain = if r.upstreams_chain.len() > 1 {
                r.upstreams_chain
                    .into_iter()
                    .map(|u| UpstreamRefDto {
                        provider: u.provider,
                        model: u.model,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            AdminRecord {
                id: r.id,
                frontends: r.frontends.iter().copied().map(frontend_kind_str).collect(),
                upstream_provider: r.upstream_provider,
                upstream_model: r.upstream_model,
                upstreams_chain,
                metadata: r.metadata,
                long_context_variant: r.long_context_variant,
                reasoning_mapping: mapping,
                default_reasoning_effort: default_effort,
            }
        })
        .collect();
    Json(AdminCatalogResponse { routes })
}
