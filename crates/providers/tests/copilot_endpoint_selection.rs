//! Capability-driven endpoint selection: an Anthropic-Messages frontend
//! targeting a responses-only Copilot model (e.g. `gpt-5.5`) must reach the
//! Responses API (`/v1/responses`), NOT `/chat/completions`. Regression guard
//! for the bug where endpoint choice keyed solely on the inbound frontend kind.

use agent_shim_core::{
    policy::ResolvedPolicy, BackendTarget, CanonicalRequest, ContentBlock, ExtensionMap,
    FrontendInfo, FrontendKind, FrontendModel, GenerationOptions, Message, RequestId, StreamEvent,
};
use agent_shim_providers::{
    github_copilot::{credential_store::StoredCredentials, CopilotProvider},
    BackendProvider,
};
use futures::StreamExt;

/// Build a token-exchange JSON whose `endpoints.api` points back at the mock
/// server, so the provider's subsequent upstream call lands on the same mock.
fn token_exchange_body(api_base: &str) -> String {
    format!(
        r#"{{"token":"tid_test","expires_at":9999999999,"refresh_in":3600,"endpoints":{{"api":"{api_base}"}}}}"#
    )
}

fn anthropic_req_for(model: &str, endpoints: Option<Vec<String>>) -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::AnthropicMessages,
            requested_model: FrontendModel::from(model),
        },
        model: FrontendModel::from(model),
        system: vec![],
        messages: vec![Message::user(vec![ContentBlock::text("hello")])],
        tools: vec![],
        tool_choice: Default::default(),
        generation: GenerationOptions::default(),
        response_format: None,
        stream: false,
        metadata: Default::default(),
        inbound_anthropic_headers: vec![],
        // The gateway pipeline normally fills this from discovered catalog
        // metadata; here we seed it directly to exercise the provider in
        // isolation.
        resolved_policy: ResolvedPolicy {
            supported_endpoints: endpoints,
            ..Default::default()
        },
        extensions: ExtensionMap::new(),
    }
}

#[tokio::test]
async fn anthropic_frontend_responses_only_model_hits_responses_api() {
    let mut server = mockito::Server::new_async().await;

    let token_mock = server
        .mock("GET", "/copilot_internal/v2/token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(token_exchange_body(&server.url()))
        .create_async()
        .await;

    // The fix: this endpoint MUST be hit. We also assert the hardened
    // `stream: true` is present in the body even though the client request is
    // non-streaming.
    let responses_mock = server
        .mock("POST", "/v1/responses")
        .match_body(mockito::Matcher::PartialJson(serde_json::json!({
            "model": "gpt-5.5",
            "stream": true,
        })))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(concat!(
            "event: response.created\n",
            "data: {\"id\":\"resp_1\",\"model\":\"gpt-5.5\",\"created_at\":1700000000}\n\n",
            "event: response.completed\n",
            "data: {\"id\":\"resp_1\"}\n\n",
        ))
        .create_async()
        .await;

    // If the bug regressed, the provider would POST here instead. Leaving it
    // un-expected (no `.expect(...)`) means a hit just serves 500; the real
    // guard is `responses_mock.assert_async()` below.
    let chat_mock = server
        .mock("POST", "/chat/completions")
        .with_status(500)
        .with_body("should not be called")
        .create_async()
        .await;

    let creds = StoredCredentials {
        github_oauth_token: "gho_test".to_string(),
        created_at_unix: 0,
    };
    let provider =
        CopilotProvider::spawn_with_creds(creds, server.url()).expect("provider should build");

    let target = BackendTarget {
        provider: "github_copilot".to_string(),
        model: "gpt-5.5".to_string(),
        policy: Default::default(),
    };

    let req = anthropic_req_for(
        "gpt-5.5",
        Some(vec!["/responses".to_string(), "ws:/responses".to_string()]),
    );

    let mut stream = provider.complete(req, target).await.expect("complete ok");
    let mut events = vec![];
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("stream event ok"));
    }

    // A well-formed canonical envelope came back through the Responses parser.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::ResponseStart { .. })),
        "expected ResponseStart, got: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::ResponseStop { .. })),
        "expected ResponseStop, got: {events:?}"
    );

    token_mock.assert_async().await;
    responses_mock.assert_async().await;
    // The chat endpoint must NOT have been called.
    assert!(
        !chat_mock.matched_async().await,
        "/chat/completions must not be called for a responses-only model"
    );
}

#[tokio::test]
async fn anthropic_frontend_chat_only_model_hits_chat_api() {
    // Inverse / no-regression: a chat-only model (no `/responses`) from an
    // Anthropic frontend still uses `/chat/completions`.
    let mut server = mockito::Server::new_async().await;

    let token_mock = server
        .mock("GET", "/copilot_internal/v2/token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(token_exchange_body(&server.url()))
        .create_async()
        .await;

    let chat_mock = server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "created": 1700000000,
                "model": "claude-opus-4.7",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "hi back"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            }"#,
        )
        .create_async()
        .await;

    let responses_mock = server
        .mock("POST", "/v1/responses")
        .with_status(500)
        .with_body("should not be called")
        .create_async()
        .await;

    let creds = StoredCredentials {
        github_oauth_token: "gho_test".to_string(),
        created_at_unix: 0,
    };
    let provider =
        CopilotProvider::spawn_with_creds(creds, server.url()).expect("provider should build");

    let target = BackendTarget {
        provider: "github_copilot".to_string(),
        model: "claude-opus-4.7".to_string(),
        policy: Default::default(),
    };

    let req = anthropic_req_for(
        "claude-opus-4.7",
        Some(vec![
            "/v1/messages".to_string(),
            "/chat/completions".to_string(),
        ]),
    );

    let mut stream = provider.complete(req, target).await.expect("complete ok");
    let mut events = vec![];
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("stream event ok"));
    }

    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { text, .. } if text == "hi back")),
        "expected text delta, got: {events:?}"
    );

    token_mock.assert_async().await;
    chat_mock.assert_async().await;
    assert!(
        !responses_mock.matched_async().await,
        "/v1/responses must not be called for a chat-only model"
    );
}
