//! Plan 01 P01 T4: outbound HTTP requests carry a W3C `traceparent`
//! header injected by `ProviderHttpClient`.
//!
//! Test strategy: stand up a mockito server, configure the gateway with
//! an OpenAI-compatible upstream pointing at mockito, hit the public
//! port. The mockito matcher requires the outbound request to carry a
//! well-formed `traceparent` header.
//!
//! For `inject_context_into_headers` to actually emit a header, the
//! gateway's root span must carry a valid OTel `SpanContext`. That only
//! happens when a `tracing-opentelemetry` layer is installed in the
//! tracing subscriber — without it, `OpenTelemetrySpanExt::context()`
//! returns the empty default Context and the propagator writes nothing.
//! So we install a minimal `InMemorySpanExporter`-backed
//! `TracerProvider` + `tracing_opentelemetry::layer()` as the global
//! subscriber before spawning the gateway. Production wires the same
//! shape via `agent_shim_observability::init` when `otel.endpoint` is set;
//! the test uses an in-memory exporter instead to keep the test hermetic.
//!
//! Positive unit-level evidence that `inject_otel_headers` writes the
//! header when a span carries an OTel context lives in
//! `agent_shim_observability::otel::extract::tests::inject_writes_traceparent_when_parent_set`.
//! This integration test walks the whole stack — axum router → frontend
//! decode → resilience layer → `ProviderHttpClient`-wrapped reqwest
//! POST → mockito.
//!
//! NOTE: `upstreams.m.tier` was previously omitted; P03 T5 added it back
//! once the schema made it required.

use std::net::SocketAddr;
use std::sync::Once;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::TracerProvider;
use tracing_subscriber::layer::SubscriberExt;

/// Install a process-wide tracing subscriber that contains the
/// `tracing-opentelemetry` layer, so spans created inside the gateway
/// pipeline carry a real OTel `SpanContext` that the W3C propagator can
/// read. `set_global_default` panics on the second call, so we gate
/// behind a `Once`; multiple integration tests in the same binary share
/// the subscriber.
fn install_otel_subscriber() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_simple_exporter(exporter)
            .build();
        let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("test"));
        let subscriber = tracing_subscriber::registry().with(layer);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

async fn pick_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn outbound_request_carries_traceparent_header() {
    install_otel_subscriber();

    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .match_header(
            "traceparent",
            mockito::Matcher::Regex(r"^00-[0-9a-f]{32}-[0-9a-f]{16}-(00|01)$".to_string()),
        )
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("data: [DONE]\n\n")
        .create_async()
        .await;

    let public_port = pick_port().await;
    let yaml = format!(
        r#"
server: {{bind: 127.0.0.1, port: {public_port}}}
upstreams:
  m:
    type: open_ai_compatible
    base_url: {}
    api_key: dummy
    tier: standard
routes:
  - frontend: openai_chat
    model: x
    upstream: m
    upstream_model: x
"#,
        server.url()
    );
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(&yaml).unwrap();
    let (state, _reload_rx) = agent_shim_gateway::state::AppState::new(cfg).await.unwrap();
    let public_addr: SocketAddr = format!("127.0.0.1:{public_port}").parse().unwrap();
    let listener = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let app = agent_shim_gateway::server::build_router(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let _ = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .body(r#"{"model":"x","messages":[{"role":"user","content":"hi"}],"stream":true}"#)
        .send()
        .await;

    // mockito.assert_async() panics if the matcher didn't fire — i.e. if
    // the outbound request lacked a well-formed traceparent header.
    mock.assert_async().await;
}
