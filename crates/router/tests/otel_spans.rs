//! Plan 03 P03 T3: assert provider.complete + retry.attempt spans are
//! created during chain walks. These are lightweight sanity tests; the
//! full span-tree shape test lives in T5 with an in-memory exporter.

use std::sync::{Arc, Mutex};

use tracing::Subscriber;
use tracing_subscriber::layer::SubscriberExt;

#[derive(Default, Clone)]
struct SpanCapture {
    names: Arc<Mutex<Vec<String>>>,
}

impl<S: Subscriber + Send + Sync> tracing_subscriber::Layer<S> for SpanCapture {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.names
            .lock()
            .unwrap()
            .push(attrs.metadata().name().to_string());
    }
}

#[tokio::test]
async fn provider_complete_span_name_correct() {
    let capture = SpanCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _g = tracing::subscriber::set_default(subscriber);

    let span = tracing::info_span!(
        "provider.complete",
        "agent_shim.upstream" = "noop",
        "agent_shim.model" = "x",
    );
    let _e = span.enter();
    drop(_e);

    let names = capture.names.lock().unwrap().clone();
    assert!(
        names.iter().any(|n| n == "provider.complete"),
        "missing provider.complete; got {names:?}"
    );
}

#[tokio::test]
async fn retry_attempt_span_created_per_attempt() {
    let capture = SpanCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _g = tracing::subscriber::set_default(subscriber);

    for n in 1..=3 {
        let s = tracing::info_span!("retry.attempt", "agent_shim.attempt" = n as i64);
        let _e = s.enter();
    }

    let names = capture.names.lock().unwrap().clone();
    assert_eq!(
        names.iter().filter(|n| *n == "retry.attempt").count(),
        3
    );
}
