//! Plan 01 P01 T5: outbound `traceparent` inherits the inbound trace_id.
//!
//! The W3C trace_id is the 32-hex segment immediately after the version.
//! When an inbound request carries `traceparent: 00-<id>-<span>-<flags>`,
//! the outbound request to the upstream MUST carry the same `<id>` —
//! that's what makes it the same distributed trace.
//!
//! Capture mechanism deviation from the plan template: the template's
//! `match_request(|req| { tokio::spawn(...) })` shape is unsafe because
//! mockito's matcher runs on a worker thread with no tokio runtime and
//! `tokio::spawn` would panic at runtime. Instead, the matcher closure
//! synchronously writes the inbound `traceparent` to a `std::sync::Mutex`
//! that the test thread reads after `mock.assert_async()`. No runtime
//! dependency in the matcher; no race because mockito serializes matcher
//! invocations per mock.
//!
//! See `outbound_traceparent.rs` for the rationale behind the OTel
//! subscriber install.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Once};

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::TracerProvider;
use tracing_subscriber::layer::SubscriberExt;

const INBOUND_TRACE_ID: &str = "0af7651916cd43dd8448eb211c80319c";

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
async fn outbound_trace_id_matches_inbound() {
    install_otel_subscriber();

    // Shared capture slot. The matcher closure runs synchronously on a
    // mockito worker thread, so we use a plain `std::sync::Mutex` (no
    // tokio runtime needed inside the closure). The test thread reads
    // the captured value after `mock.assert_async()`.
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured_for_matcher = captured.clone();

    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .match_request(move |req| {
            if let Some(v) = req.header("traceparent").first() {
                if let Ok(s) = v.to_str() {
                    if let Ok(mut slot) = captured_for_matcher.lock() {
                        *slot = Some(s.to_string());
                    }
                }
            }
            true
        })
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

    let inbound_tp = format!("00-{INBOUND_TRACE_ID}-b7ad6b7169203331-01");
    let _ = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .header("traceparent", inbound_tp)
        .body(r#"{"model":"x","messages":[{"role":"user","content":"hi"}],"stream":true}"#)
        .send()
        .await;

    mock.assert_async().await;

    let outbound_tp = captured
        .lock()
        .unwrap()
        .clone()
        .expect("outbound traceparent must have been captured");
    assert!(
        outbound_tp.contains(INBOUND_TRACE_ID),
        "outbound trace_id must match inbound; inbound={INBOUND_TRACE_ID} outbound={outbound_tp}",
    );
}
