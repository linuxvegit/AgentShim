//! Unit tests for the dialect-aware error envelope mapping introduced in
//! Plan 04 P02 T5.
//!
//! These cover `HandlerError::IntoResponse` for the four resilience
//! variants (`NoUpstreamSucceeded`, `TerminalUpstream`, `AllBreakersOpen`,
//! `RateLimited`). The contract under test is §5.2 + §6.3 of the Phase 4
//! design:
//!
//! - HTTP status: 503 for "no upstream available" cases (chain
//!   exhaustion, all breakers open), 429 for rate limits, pass-through
//!   for terminal upstream errors.
//! - OpenAI envelope: `{"error": {"message", "type", "code"}}` with
//!   `type` keyed to the HTTP class and `code` to the specific outcome.
//! - Anthropic envelope: `{"type": "error", "error": {"type", "message"}}`
//!   — note the absence of a `code` field.
//! - `Retry-After` header on 429 responses.
//!
//! The variants under test are not yet *produced* anywhere in the
//! gateway pipeline (P02 T6 wires that). Producing them here via direct
//! construction is the intended test surface — once T6 lands the
//! pipeline-driven integration tests will exercise the same envelope
//! code through the full request path.

use agent_shim_core::FrontendKind;
use agent_shim_gateway::handlers::HandlerError;
use agent_shim_providers::ProviderError;
use agent_shim_router::{RateLimitDimension, ResilienceError, TriedUpstream};
use axum::http::StatusCode;
use axum::response::IntoResponse;

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("collect body");
    serde_json::from_slice(&bytes).expect("body is valid json")
}

#[tokio::test]
async fn no_upstream_succeeded_maps_to_503_with_openai_envelope() {
    let err = HandlerError::NoUpstreamSucceeded {
        kind: FrontendKind::OpenAiChat,
        last_error: ProviderError::Upstream {
            status: 502,
            body: "x".into(),
        },
        tried_count: 3,
    };
    let response = err.into_response();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = body_json(response).await;
    assert_eq!(json["error"]["type"], "service_unavailable_error");
    assert_eq!(json["error"]["code"], "no_upstream_available");
    let message = json["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("3 upstreams failed"),
        "message should mention chain length, got: {message}"
    );
}

#[tokio::test]
async fn no_upstream_succeeded_anthropic_dialect_uses_overloaded_error() {
    let err = HandlerError::NoUpstreamSucceeded {
        kind: FrontendKind::AnthropicMessages,
        last_error: ProviderError::Network("conn reset".into()),
        tried_count: 2,
    };
    let response = err.into_response();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = body_json(response).await;
    assert_eq!(json["type"], "error");
    assert_eq!(json["error"]["type"], "overloaded_error");
    // Anthropic envelope has no `code` field.
    assert!(json["error"].get("code").is_none());
}

#[tokio::test]
async fn all_breakers_open_maps_to_503_openai_code() {
    let err = HandlerError::AllBreakersOpen {
        kind: FrontendKind::OpenAiResponses,
        tried_count: 4,
    };
    let response = err.into_response();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = body_json(response).await;
    assert_eq!(json["error"]["type"], "service_unavailable_error");
    assert_eq!(json["error"]["code"], "all_breakers_open");
}

#[tokio::test]
async fn rate_limited_per_key_anthropic_dialect_no_code_field() {
    let err = HandlerError::RateLimited {
        kind: FrontendKind::AnthropicMessages,
        dimension: RateLimitDimension::PerKey,
        retry_after_secs: 30,
    };
    let response = err.into_response();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after = response
        .headers()
        .get("retry-after")
        .expect("Retry-After header must be present on 429");
    assert_eq!(retry_after.to_str().unwrap(), "30");
    let json = body_json(response).await;
    assert_eq!(json["type"], "error");
    assert_eq!(json["error"]["type"], "rate_limit_error");
    assert!(
        json["error"].get("code").is_none(),
        "Anthropic dialect must not include a `code` field"
    );
}

#[tokio::test]
async fn rate_limited_openai_dialect_carries_dimension_in_code() {
    // Each dimension produces a distinct stable `code` so dashboards can
    // differentiate "per-route" vs "per-key" exhaustion without parsing
    // the message string.
    let cases = [
        (RateLimitDimension::PerKey, "rate_limited_per_key"),
        (RateLimitDimension::PerRoute, "rate_limited_per_route"),
        (RateLimitDimension::PerUpstream, "rate_limited_per_upstream"),
        (RateLimitDimension::PerIp, "rate_limited_per_ip"),
    ];
    for (dim, expected_code) in cases {
        let err = HandlerError::RateLimited {
            kind: FrontendKind::OpenAiChat,
            dimension: dim,
            retry_after_secs: 5,
        };
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get("retry-after").unwrap(), "5");
        let json = body_json(response).await;
        assert_eq!(json["error"]["type"], "rate_limit_error");
        assert_eq!(
            json["error"]["code"], expected_code,
            "dimension {dim:?} should produce code {expected_code}"
        );
    }
}

#[tokio::test]
async fn terminal_upstream_passes_through_status() {
    let err = HandlerError::TerminalUpstream {
        kind: FrontendKind::OpenAiChat,
        error: ProviderError::Upstream {
            status: 401,
            body: "auth".into(),
        },
    };
    let response = err.into_response();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // Generic body — TerminalUpstream does not get its own dialect
    // shaping (the underlying upstream's error is the operator-facing
    // detail).
    let json = body_json(response).await;
    assert!(json["error"]["message"].as_str().is_some());
}

#[tokio::test]
async fn terminal_upstream_non_upstream_falls_back_to_502() {
    // A terminal Decode error has no HTTP status to pass through, so
    // BAD_GATEWAY is the right "we can't fulfil" signal.
    let err = HandlerError::TerminalUpstream {
        kind: FrontendKind::OpenAiChat,
        error: ProviderError::Decode("bad json".into()),
    };
    let response = err.into_response();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn from_resilience_error_preserves_kind_and_counts() {
    // The bridge method is the only way pipeline code converts a
    // ResilienceError into a HandlerError; verify it threads the kind
    // through and surfaces tried.len() as tried_count.
    let res_err = ResilienceError::NoUpstreamSucceeded {
        tried: vec![
            TriedUpstream {
                provider: "openai".into(),
                model: "gpt-4o".into(),
                attempts: 3,
                last_error_tag: "upstream_5xx".into(),
                last_error_msg: "502".into(),
                elapsed_ms: 100,
            },
            TriedUpstream {
                provider: "anthropic".into(),
                model: "claude-3".into(),
                attempts: 2,
                last_error_tag: "network".into(),
                last_error_msg: "reset".into(),
                elapsed_ms: 50,
            },
        ],
        last_error: ProviderError::Network("reset".into()),
    };
    let handler_err = HandlerError::from_resilience_error(res_err, FrontendKind::AnthropicMessages);
    match handler_err {
        HandlerError::NoUpstreamSucceeded {
            kind, tried_count, ..
        } => {
            assert!(matches!(kind, FrontendKind::AnthropicMessages));
            assert_eq!(tried_count, 2);
        }
        other => panic!("expected NoUpstreamSucceeded, got {other:?}"),
    }
}

#[tokio::test]
async fn from_resilience_error_maps_rate_limited_dimension() {
    let res_err = ResilienceError::RateLimited {
        dimension: RateLimitDimension::PerRoute,
        retry_after_secs: 12,
    };
    let handler_err = HandlerError::from_resilience_error(res_err, FrontendKind::OpenAiChat);
    match handler_err {
        HandlerError::RateLimited {
            dimension,
            retry_after_secs,
            ..
        } => {
            assert!(matches!(dimension, RateLimitDimension::PerRoute));
            assert_eq!(retry_after_secs, 12);
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}
