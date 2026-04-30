use agent_shim_core::{
    BackendTarget, CanonicalRequest, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel,
    GenerationOptions, RequestId, StreamEvent,
};
use agent_shim_providers::{openai_compatible::OpenAiCompatibleProvider, BackendProvider};
use futures::StreamExt;

fn make_req(stream: bool) -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::OpenAiChat,
            requested_model: FrontendModel::from("gpt-4o"),
        },
        model: FrontendModel::from("gpt-4o"),
        system: vec![],
        messages: vec![agent_shim_core::Message::user(vec![
            agent_shim_core::ContentBlock::text("hello"),
        ])],
        tools: vec![],
        tool_choice: Default::default(),
        generation: GenerationOptions::default(),
        response_format: None,
        stream,
        metadata: Default::default(),
        inbound_anthropic_headers: vec![],
        resolved_policy: Default::default(),
        extensions: ExtensionMap::new(),
    }
}

fn make_target() -> BackendTarget {
    BackendTarget {
        provider: "openai".to_string(),
        model: "gpt-4o".to_string(),
        policy: Default::default(),
    }
}

#[tokio::test]
async fn unary_returns_correct_event_sequence() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id": "chatcmpl-123",
                "object": "chat.completion",
                "created": 1700000000,
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Hello, world!"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 5,
                    "total_tokens": 15
                }
            }"#,
        )
        .create_async()
        .await;

    let provider =
        OpenAiCompatibleProvider::new("openai", server.url(), "test-key", Default::default(), 30)
            .unwrap();

    let mut stream = provider
        .complete(make_req(false), make_target())
        .await
        .unwrap();

    let mut events = vec![];
    while let Some(ev) = stream.next().await {
        events.push(ev.unwrap());
    }

    // Should have ResponseStart, MessageStart, ContentBlockStart, TextDelta, ContentBlockStop,
    // MessageStop, ResponseStop
    assert!(events
        .iter()
        .any(|e| matches!(e, StreamEvent::ResponseStart { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, StreamEvent::TextDelta { text, .. } if text == "Hello, world!")));
    assert!(events
        .iter()
        .any(|e| matches!(e, StreamEvent::MessageStop { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, StreamEvent::ResponseStop { .. })));

    mock.assert_async().await;
}

#[tokio::test]
async fn proxy_raw_responses_posts_to_responses_and_rewrites_model() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/responses")
        .match_header("authorization", "Bearer test-key")
        .match_body(r#"{"model":"gpt-upstream","input":"Hello","stream":true}"#)
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("event: response.completed\ndata: {\"id\":\"resp_1\"}\n\n")
        .create_async()
        .await;

    let provider =
        OpenAiCompatibleProvider::new("openai", server.url(), "test-key", Default::default(), 30)
            .unwrap();
    let target = BackendTarget {
        provider: "openai".to_string(),
        model: "gpt-upstream".to_string(),
        policy: Default::default(),
    };

    let Some((content_type, mut stream)) = provider
        .proxy_raw(
            bytes::Bytes::from_static(br#"{"model":"alias","input":"Hello","stream":true}"#),
            target,
            FrontendKind::OpenAiResponses,
        )
        .await
        .unwrap()
    else {
        panic!("expected raw proxy support");
    };

    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk.unwrap());
    }

    assert_eq!(content_type, "text/event-stream");
    assert_eq!(
        std::str::from_utf8(&body).unwrap(),
        "event: response.completed\ndata: {\"id\":\"resp_1\"}\n\n"
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn streaming_yields_text_deltas() {
    let mut server = mockito::Server::new_async().await;

    // Two SSE data events + [DONE]
    let sse_body = concat!(
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(sse_body)
        .create_async()
        .await;

    let provider =
        OpenAiCompatibleProvider::new("openai", server.url(), "test-key", Default::default(), 30)
            .unwrap();

    let mut stream = provider
        .complete(make_req(true), make_target())
        .await
        .unwrap();

    let mut text_deltas: Vec<String> = vec![];
    let mut got_stop = false;
    let mut got_done = false;

    while let Some(ev) = stream.next().await {
        match ev.unwrap() {
            StreamEvent::TextDelta { text, .. } => text_deltas.push(text),
            StreamEvent::MessageStop { .. } => got_stop = true,
            StreamEvent::ResponseStop { .. } => got_done = true,
            _ => {}
        }
    }

    assert_eq!(text_deltas, vec!["Hello", " world"]);
    assert!(got_stop, "expected MessageStop");
    assert!(got_done, "expected ResponseStop");

    mock.assert_async().await;
}
