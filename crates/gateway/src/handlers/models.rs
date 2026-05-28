//! GET /v1/models — public OpenAI-shape model catalog.
//!
//! Returns the aliases AgentShim accepts on any frontend. The shape
//! mirrors OpenAI's `/v1/models` envelope (`object: "list"`, `data: [...]`)
//! with agent-shim-specific extensions for upstream provenance and
//! capability metadata. Filters: `?frontend=...`, `?capability=...`.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// Query params for `GET /v1/models`.
#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    pub frontend: Option<String>,
    pub capability: Option<String>,
}

#[derive(Debug, Serialize)]
struct ListResponse {
    object: &'static str,
    data: Vec<ModelDto>,
}

#[derive(Debug, Serialize)]
struct ModelDto {
    id: String,
    object: &'static str,
    owned_by: &'static str,
    frontends: Vec<&'static str>,
    upstream: UpstreamDto,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    upstreams: Vec<UpstreamDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capabilities: Option<CapabilitiesDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    long_context_variant: Option<String>,
}

#[derive(Debug, Serialize)]
struct UpstreamDto {
    provider: String,
    model: String,
}

#[derive(Debug, Serialize)]
struct CapabilitiesDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    supports: agent_shim_core::ModelSupports,
    #[serde(skip_serializing_if = "Option::is_none")]
    family: Option<String>,
}

fn frontend_kind_str(kind: agent_shim_core::FrontendKind) -> &'static str {
    match kind {
        agent_shim_core::FrontendKind::AnthropicMessages => "anthropic_messages",
        agent_shim_core::FrontendKind::OpenAiChat => "openai_chat",
        agent_shim_core::FrontendKind::OpenAiResponses => "openai_responses",
    }
}

fn record_to_dto(r: agent_shim_core::ModelRecord) -> ModelDto {
    let frontends: Vec<&'static str> = r.frontends.iter().copied().map(frontend_kind_str).collect();
    let upstream = UpstreamDto {
        provider: r.upstream_provider,
        model: r.upstream_model,
    };
    let upstreams = if r.upstreams_chain.len() > 1 {
        r.upstreams_chain
            .into_iter()
            .map(|u| UpstreamDto {
                provider: u.provider,
                model: u.model,
            })
            .collect()
    } else {
        Vec::new()
    };
    let capabilities = r.metadata.map(|m| CapabilitiesDto {
        context_window_tokens: m.context_window_tokens,
        max_output_tokens: m.max_output_tokens,
        supports: m.supports,
        family: m.family,
    });
    ModelDto {
        id: r.id,
        object: "model",
        owned_by: "agent-shim",
        frontends,
        upstream,
        upstreams,
        capabilities,
        long_context_variant: r.long_context_variant,
    }
}

fn capability_matches(meta: &agent_shim_core::ModelMetadata, capability: &str) -> bool {
    match capability {
        "vision" => meta.supports.vision == Some(true),
        "tool_calls" => meta.supports.tool_calls == Some(true),
        "streaming" => meta.supports.streaming == Some(true),
        "structured_outputs" => meta.supports.structured_outputs == Some(true),
        "reasoning_effort" => meta.supports.reasoning_effort == Some(true),
        // Unknown capability names filter everything out — clients passing
        // a typo see an empty list rather than a silently unfiltered one.
        _ => false,
    }
}

fn record_matches_filters(r: &agent_shim_core::ModelRecord, q: &ListQuery) -> bool {
    if let Some(frontend_filter) = q.frontend.as_deref() {
        let any = r
            .frontends
            .iter()
            .copied()
            .any(|fk| frontend_kind_str(fk) == frontend_filter);
        if !any {
            return false;
        }
    }
    if let Some(cap) = q.capability.as_deref() {
        match r.metadata.as_ref() {
            None => return false,
            Some(meta) => {
                if !capability_matches(meta, cap) {
                    return false;
                }
            }
        }
    }
    true
}

pub async fn list(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let catalog = state.core.resolver.list_catalog();
    let filtered: Vec<ModelDto> = catalog
        .into_iter()
        .filter(|r| record_matches_filters(r, &q))
        .map(record_to_dto)
        .collect();
    Json(ListResponse {
        object: "list",
        data: filtered,
    })
    .into_response()
}

pub async fn get_one(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let catalog = state.core.resolver.list_catalog();
    match catalog.into_iter().find(|r| r.id == id) {
        Some(r) => Json(record_to_dto(r)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": {
                    "message": format!("model not found: {id}"),
                    "type": "not_found_error",
                    "code": "model_not_found",
                }
            })),
        )
            .into_response(),
    }
}
