//! Phase 7 P07 micro-benchmark.
//!
//! Measures the latency of `PluginRegistry::empty().run_on_decoded_request()`
//! — the hot path for production deployments that don't configure any
//! plugins. The intent is to protect the "zero-overhead empty registry"
//! claim made in CHANGELOG v0.7.0.
//!
//! Target: < 1 µs / call on a 2024-era developer laptop.
//!
//! Why not an end-to-end bench: a full HTTP + serde + axum + provider-mock
//! request has a per-iteration cost in the hundreds of microseconds; the
//! plugin-hook delta is ~3 orders of magnitude smaller than that noise
//! floor. A direct call on the registry surface gives a clean signal.
//!
//! The "zero-overhead" claim ultimately rests on:
//!   1. `lookup()` returns `None` for an empty registry (one hashmap miss).
//!   2. The early-return path in `run_on_decoded_request` does not allocate
//!      and does not clone the request.

use std::sync::Arc;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use agent_shim_core::request::RequestMetadata;
use agent_shim_core::{
    CanonicalRequest, ContentBlock, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel,
    GenerationOptions, Message, RequestId, ResolvedPolicy, TextBlock,
};
use agent_shim_plugins::{PluginContext, PluginRegistry};

fn make_request() -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::AnthropicMessages,
            requested_model: FrontendModel::from("bench-model"),
        },
        model: FrontendModel::from("bench-model"),
        system: vec![],
        messages: vec![Message::user(vec![ContentBlock::Text(TextBlock {
            text: "bench prompt".to_string(),
            extensions: ExtensionMap::new(),
        })])],
        tools: vec![],
        tool_choice: Default::default(),
        generation: GenerationOptions::default(),
        response_format: None,
        stream: false,
        metadata: RequestMetadata::default(),
        inbound_anthropic_headers: vec![],
        resolved_policy: ResolvedPolicy::default(),
        extensions: ExtensionMap::new(),
    }
}

fn make_ctx() -> PluginContext {
    PluginContext::new(
        RequestId::new(),
        FrontendKind::AnthropicMessages,
        "anthropic_messages/bench-model".to_string(),
    )
}

fn bench_empty_registry_h2(c: &mut Criterion) {
    let registry = Arc::new(PluginRegistry::empty());
    let ctx = make_ctx();
    let req = make_request();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("empty_registry");
    group.bench_function(BenchmarkId::new("run_on_decoded_request", "empty"), |b| {
        b.iter(|| {
            let registry = registry.clone();
            let ctx = ctx.clone();
            let req = req.clone();
            rt.block_on(async move {
                registry
                    .run_on_decoded_request(
                        black_box((FrontendKind::AnthropicMessages, "bench-model")),
                        black_box(&ctx),
                        req,
                    )
                    .await
                    .unwrap()
            });
        });
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .measurement_time(Duration::from_secs(3))
        .sample_size(50);
    targets = bench_empty_registry_h2
}
criterion_main!(benches);
