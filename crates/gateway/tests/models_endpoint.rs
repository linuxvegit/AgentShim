//! GET /v1/models endpoint integration tests.
//!
//! Builds the gateway via `AppState::new` + `build_router` against a
//! synthetic upstream config (no live network calls — model discovery
//! gracefully degrades to "no upstream metadata" because the dummy
//! base URL is unreachable). The catalog still surfaces the explicit
//! aliases from `routes`, which is what we're asserting.

use std::collections::BTreeMap;

use agent_shim_config::{
    schema::{LoggingConfig, RouteEntry, ServerConfig, UpstreamConfig},
    GatewayConfig,
};
use agent_shim_gateway::{server::build_router, state::AppState};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tower::util::ServiceExt;

/// Build a config with the given routes; every referenced upstream is a
/// dummy openai_compatible pointing at an unreachable port. Model
/// discovery will silently fail at startup, leaving the catalog metadata
/// fields as `None` — that's fine for testing the route enumeration path.
fn cfg_with_routes(specs: &[(&str, &str, &str, &str)]) -> GatewayConfig {
    use agent_shim_config::schema::OpenAiCompatibleUpstream;
    use agent_shim_config::Tier;

    let mut upstreams: BTreeMap<String, UpstreamConfig> = BTreeMap::new();
    for (_, _, upstream_name, _) in specs {
        upstreams
            .entry((*upstream_name).to_string())
            .or_insert_with(|| {
                UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
                    base_url: "http://127.0.0.1:65535/v1".to_string(),
                    api_key: agent_shim_config::Secret::new("dummy"),
                    default_headers: BTreeMap::new(),
                    request_timeout_secs: 30,
                    tier: Tier::Standard,
                    cost: None,
                    p95_latency_budget_ms: None,
                })
            });
    }
    let routes: Vec<RouteEntry> = specs
        .iter()
        .map(|(frontend, alias, upstream, upstream_model)| {
            RouteEntry::singular(
                frontend.to_string(),
                alias.to_string(),
                upstream.to_string(),
                upstream_model.to_string(),
            )
        })
        .collect();
    GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams,
        routes,
        plugins: ::std::collections::BTreeMap::new(),
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
        shutdown: Default::default(),
        validation: Default::default(),
    }
}

async fn parse_body(resp: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).expect("response body is JSON")
}

#[tokio::test]
async fn list_returns_openai_shape_envelope() {
    let cfg = cfg_with_routes(&[(
        "anthropic_messages",
        "claude-opus-4-7",
        "u",
        "claude-opus-4.7",
    )]);
    let (state, _rx) = AppState::new(cfg).await.unwrap();
    let app = build_router(state);

    let req = Request::builder()
        .uri("/v1/models")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = parse_body(resp).await;

    assert_eq!(v["object"], "list");
    let data = v["data"].as_array().expect("data is array");
    assert_eq!(data.len(), 1);
    let entry = &data[0];
    assert_eq!(entry["id"], "claude-opus-4-7");
    assert_eq!(entry["object"], "model");
    assert_eq!(entry["owned_by"], "agent-shim");
    assert_eq!(entry["upstream"]["provider"], "u");
    assert_eq!(entry["upstream"]["model"], "claude-opus-4.7");
    let frontends = entry["frontends"].as_array().expect("frontends is array");
    assert_eq!(frontends.len(), 1);
    assert_eq!(frontends[0], "anthropic_messages");
}

#[tokio::test]
async fn get_one_returns_known_alias() {
    let cfg = cfg_with_routes(&[(
        "openai_chat",
        "gpt-5.5",
        "openai",
        "gpt-5.5",
    )]);
    let (state, _rx) = AppState::new(cfg).await.unwrap();
    let app = build_router(state);

    let req = Request::builder()
        .uri("/v1/models/gpt-5.5")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = parse_body(resp).await;
    assert_eq!(v["id"], "gpt-5.5");
    assert_eq!(v["upstream"]["provider"], "openai");
}

#[tokio::test]
async fn get_one_returns_404_for_unknown_alias() {
    let cfg = cfg_with_routes(&[]);
    let (state, _rx) = AppState::new(cfg).await.unwrap();
    let app = build_router(state);

    let req = Request::builder()
        .uri("/v1/models/does-not-exist")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = parse_body(resp).await;
    assert_eq!(v["error"]["code"], "model_not_found");
}

#[tokio::test]
async fn frontend_filter_narrows_results() {
    let cfg = cfg_with_routes(&[
        ("anthropic_messages", "claude", "u1", "claude-opus-4.7"),
        ("openai_chat", "gpt", "u2", "gpt-5.5"),
    ]);
    let (state, _rx) = AppState::new(cfg).await.unwrap();
    let app = build_router(state);

    let req = Request::builder()
        .uri("/v1/models?frontend=openai_chat")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = parse_body(resp).await;
    let data = v["data"].as_array().expect("data is array");
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["id"], "gpt");
}

#[tokio::test]
async fn capability_filter_with_no_metadata_yields_empty() {
    // Discovery fails against the unreachable upstream → metadata is None
    // → ?capability=vision filters out every record (no metadata can satisfy).
    let cfg = cfg_with_routes(&[(
        "openai_chat",
        "gpt-5.5",
        "openai",
        "gpt-5.5",
    )]);
    let (state, _rx) = AppState::new(cfg).await.unwrap();
    let app = build_router(state);

    let req = Request::builder()
        .uri("/v1/models?capability=vision")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = parse_body(resp).await;
    let data = v["data"].as_array().expect("data is array");
    assert!(
        data.is_empty(),
        "capability filter must drop entries lacking metadata"
    );
}

#[tokio::test]
async fn admin_catalog_returns_routes_array() {
    let cfg = cfg_with_routes(&[(
        "anthropic_messages",
        "claude-opus-4-7",
        "u",
        "claude-opus-4.7",
    )]);
    let (state, _rx) = AppState::new(cfg).await.unwrap();
    let app = agent_shim_gateway::admin::build_router(state);

    let req = Request::builder()
        .uri("/admin/catalog")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = parse_body(resp).await;
    let routes = v["routes"].as_array().expect("routes is array");
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0]["id"], "claude-opus-4-7");
    assert_eq!(routes[0]["upstream_provider"], "u");
}

#[tokio::test]
async fn admin_discover_returns_501_stub() {
    let cfg = cfg_with_routes(&[]);
    let (state, _rx) = AppState::new(cfg).await.unwrap();
    let app = agent_shim_gateway::admin::build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/admin/discover")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    let v = parse_body(resp).await;
    assert_eq!(v["error"]["code"], "discover_unimplemented");
}
