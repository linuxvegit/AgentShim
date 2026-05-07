//! Unit tests that lock in the `proxy_raw` short-circuit on the Anthropic
//! provider: only `FrontendKind::AnthropicMessages` enters the passthrough
//! path; every other inbound frontend kind must return `Ok(None)` so the
//! gateway falls through to the canonical (`complete()`) path.
//!
//! See `AnthropicProvider::proxy_raw` for the source of truth this test
//! pins down.

use agent_shim_core::{BackendTarget, FrontendKind, RoutePolicy};
use agent_shim_providers::{anthropic::AnthropicProvider, BackendProvider};
use bytes::Bytes;

/// Build a minimal `AnthropicProvider`. The short-circuit branches under
/// test return before any field of `self` is touched, so the base URL is
/// intentionally pointed at an unbound loopback port to make any accidental
/// network call fail fast and loudly.
fn make_provider() -> AnthropicProvider {
    AnthropicProvider::new(
        "anthropic",
        "http://127.0.0.1:1",
        "test-key",
        "2023-06-01",
        Default::default(),
        5,
    )
    .expect("provider construction is infallible for valid inputs")
}

fn make_target() -> BackendTarget {
    BackendTarget {
        provider: "anthropic".to_string(),
        model: "claude-opus-4-7".to_string(),
        policy: RoutePolicy::default(),
    }
}

#[tokio::test]
async fn proxy_raw_returns_none_for_openai_responses_frontend() {
    let provider = make_provider();

    let result = provider
        .proxy_raw(Bytes::new(), make_target(), FrontendKind::OpenAiResponses)
        .await
        .expect("short-circuit must not return an error");

    assert!(
        result.is_none(),
        "OpenAiResponses inbound must short-circuit to Ok(None) so the \
         gateway uses the canonical path; got Some(_)"
    );
}

#[tokio::test]
async fn proxy_raw_returns_none_for_openai_chat_frontend() {
    let provider = make_provider();

    let result = provider
        .proxy_raw(Bytes::new(), make_target(), FrontendKind::OpenAiChat)
        .await
        .expect("short-circuit must not return an error");

    assert!(
        result.is_none(),
        "OpenAiChat inbound must short-circuit to Ok(None) so the gateway \
         uses the canonical path; got Some(_)"
    );
}

/// Positive control: `AnthropicMessages` inbound must NOT short-circuit;
/// it must attempt `passthrough::send`. Because the provider is pointed at
/// an unbound loopback port, the attempt fails with a network error — that
/// failure mode is exactly the proof we want: the call left `proxy_raw`
/// and tried to reach the upstream rather than returning `Ok(None)`.
#[tokio::test]
async fn proxy_raw_attempts_passthrough_for_anthropic_messages() {
    let provider = make_provider();

    let result = provider
        .proxy_raw(
            // Minimal valid Anthropic Messages JSON so `rewrite_model` parses
            // successfully and we reach the actual HTTP send call.
            Bytes::from_static(
                br#"{"model":"alias","messages":[{"role":"user","content":"hi"}],"max_tokens":1}"#,
            ),
            make_target(),
            FrontendKind::AnthropicMessages,
        )
        .await;

    assert!(
        !matches!(result, Ok(None)),
        "AnthropicMessages inbound must NOT short-circuit; it should enter \
         passthrough::send. Observed: Ok(None)"
    );
}
