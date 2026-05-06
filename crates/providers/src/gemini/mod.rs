//! Gemini provider (`generativelanguage.googleapis.com/v1beta`).
//!
//! Plan 03 lands this module incrementally:
//!
//! - **T2** introduced the wire DTOs in [`wire`].
//! - **T3** added [`auth`] + [`endpoint`] URL helpers.
//! - **T4** added the canonical → wire encoder in [`request`].
//! - **T5** added the streaming JSON-array reader in [`stream`].
//! - **T6** added the wire → canonical parser in [`response`].
//! - **T7 (this commit)** wires everything into the
//!   [`crate::BackendProvider`] trait via the [`GeminiProvider`] struct
//!   below. T8 will register it in `gateway::providers::registry::build`.
//!
//! Submodules stay `pub(crate)` — the only public surface is
//! [`GeminiProvider`] + [`from_config`].

pub(crate) mod auth;
pub(crate) mod endpoint;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod stream;
pub(crate) mod wire;

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream as futures_stream;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use tracing::{debug, warn};

use agent_shim_core::{
    BackendTarget, CanonicalRequest, CanonicalResponse, CanonicalStream, ContentBlock,
    ContentBlockKind, MessageRole, ResponseId, StreamError, StreamEvent, ToolCallArguments,
};

use self::auth::AiStudioAuth;
use self::wire::GenerateContentResponse;
use crate::{http_client, BackendProvider, ProviderCapabilities, ProviderError};

/// Native Gemini AI Studio provider.
///
/// One instance per upstream config block. Holds the API key, base URL,
/// any operator-supplied default headers, and a `reqwest::Client` built
/// via the shared [`crate::http_client`] helper so streaming requests
/// pick up the same `read_timeout`-based per-read budget the other
/// providers use (rather than a total request timeout that would kill
/// otherwise-healthy long streams).
pub struct GeminiProvider {
    name: &'static str,
    base_url: String,
    auth: AiStudioAuth,
    default_headers: HeaderMap,
    _timeout: Duration,
    client: reqwest::Client,
    capabilities: ProviderCapabilities,
}

// Manual Debug — we never want to leak the API key in logs, so the auth
// holder is rendered as the redacted placeholder. Other fields are safe.
impl std::fmt::Debug for GeminiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiProvider")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("auth", &"<redacted>")
            .field("default_headers", &self.default_headers)
            .field("timeout", &self._timeout)
            .field("capabilities", &self.capabilities)
            .finish()
    }
}

impl GeminiProvider {
    pub fn new(
        name: &'static str,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        default_headers: BTreeMap<String, String>,
        timeout_secs: u64,
    ) -> Result<Self, ProviderError> {
        let mut headers = HeaderMap::new();
        for (k, v) in &default_headers {
            let header_name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| ProviderError::Encode(format!("invalid header name: {e}")))?;
            let val = HeaderValue::from_str(v)
                .map_err(|e| ProviderError::Encode(format!("invalid header value: {e}")))?;
            headers.insert(header_name, val);
        }

        let client = http_client::build(Duration::from_secs(timeout_secs))?;

        Ok(Self {
            name,
            base_url: base_url.into(),
            auth: AiStudioAuth {
                api_key: api_key.into(),
            },
            default_headers: headers,
            _timeout: Duration::from_secs(timeout_secs),
            client,
            // Gemini supports streaming, tools, vision, and JSON output.
            // No json_object-only mode without schema, so json_mode is true
            // (the encoder maps both JsonObject and JsonSchema to
            // application/json).
            capabilities: ProviderCapabilities {
                streaming: true,
                tool_use: true,
                vision: true,
                json_mode: true,
            },
        })
    }
}

#[async_trait]
impl BackendProvider for GeminiProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    async fn complete(
        &self,
        req: CanonicalRequest,
        target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError> {
        let body = request::build(&req, &target);
        let is_stream = req.stream;
        let model = target.model.clone();

        let url = if is_stream {
            endpoint::streaming_url(&self.base_url, &model)
        } else {
            endpoint::unary_url(&self.base_url, &model)
        };

        debug!(
            provider = self.name,
            model = %model,
            stream = is_stream,
            "sending request to gemini"
        );

        // Build the request — `apply` adds the `?key=...` query param.
        let mut request_builder = self
            .auth
            .apply(self.client.post(&url))
            .header(CONTENT_TYPE, "application/json")
            .json(&body);

        // Apply operator-supplied default headers (e.g. trace correlation).
        for (k, v) in &self.default_headers {
            request_builder = request_builder.header(k, v);
        }

        let response = request_builder
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_string());
            warn!(
                provider = self.name,
                status = status.as_u16(),
                body = %body_text,
                "gemini upstream returned error"
            );
            return Err(ProviderError::Upstream {
                status: status.as_u16(),
                body: body_text,
            });
        }

        if is_stream {
            // Streaming: bytes -> per-object stream (T5) -> canonical events (T6).
            let byte_stream = response.bytes_stream();
            let object_stream = stream::into_response_stream(byte_stream);
            Ok(response::parse_streaming(object_stream, model))
        } else {
            // Unary: collect the full body, deserialize one
            // GenerateContentResponse, run T6's parse_unary to get a
            // CanonicalResponse, then synthesize a small canonical event
            // stream so we honour the BackendProvider::complete contract.
            let bytes = response
                .bytes()
                .await
                .map_err(|e| ProviderError::Network(e.to_string()))?;
            let stream = unary_bytes_to_canonical_stream(&bytes, &model);
            Ok(stream)
        }
    }

    // No `list_models` — AI Studio's Models API exists but we don't surface
    // it today; defaulting to None matches the deepseek/openai-compatible
    // behaviour and lets the gateway fall back to the configured aliases.
}

// ---------------------------------------------------------------------------
// Unary → CanonicalStream adapter
// ---------------------------------------------------------------------------

/// Convert a unary Gemini response body into a self-contained
/// `CanonicalStream`. The stream emits a single shot of
/// `ResponseStart` → `MessageStart` → per-block events → `MessageStop` →
/// `ResponseStop`, mirroring what the streaming path produces over many
/// chunks.
///
/// On JSON-decode failure, the stream emits a single
/// `StreamError::Decode` so the gateway can surface it cleanly without
/// dropping the request.
fn unary_bytes_to_canonical_stream(body: &[u8], model: &str) -> CanonicalStream {
    let events: Vec<Result<StreamEvent, StreamError>> =
        match serde_json::from_slice::<GenerateContentResponse>(body) {
            Ok(resp) => {
                let canonical = response::parse_unary(resp, model);
                canonical_response_to_events(canonical)
                    .into_iter()
                    .map(Ok)
                    .collect()
            }
            Err(e) => vec![Err(StreamError::Decode(format!(
                "failed to decode Gemini unary response: {e}"
            )))],
        };
    Box::pin(futures_stream::iter(events))
}

/// Synthesize a flat list of `StreamEvent`s that recreates a full
/// `CanonicalResponse`. Used by the unary-to-stream adapter; tested
/// independently below.
fn canonical_response_to_events(resp: CanonicalResponse) -> Vec<StreamEvent> {
    let mut events: Vec<StreamEvent> = Vec::new();

    events.push(StreamEvent::ResponseStart {
        id: ResponseId::new(),
        model: resp.model,
        created_at_unix: now_unix(),
    });
    events.push(StreamEvent::MessageStart {
        role: MessageRole::Assistant,
    });

    for (index, block) in resp.content.into_iter().enumerate() {
        let index = index as u32;
        match block {
            ContentBlock::Text(t) => {
                events.push(StreamEvent::ContentBlockStart {
                    index,
                    kind: ContentBlockKind::Text,
                });
                if !t.text.is_empty() {
                    events.push(StreamEvent::TextDelta {
                        index,
                        text: t.text,
                    });
                }
                events.push(StreamEvent::ContentBlockStop { index });
            }
            ContentBlock::Reasoning(r) => {
                events.push(StreamEvent::ContentBlockStart {
                    index,
                    kind: ContentBlockKind::Reasoning,
                });
                if !r.text.is_empty() {
                    events.push(StreamEvent::ReasoningDelta {
                        index,
                        text: r.text,
                    });
                }
                events.push(StreamEvent::ContentBlockStop { index });
            }
            ContentBlock::ToolCall(tc) => {
                events.push(StreamEvent::ContentBlockStart {
                    index,
                    kind: ContentBlockKind::ToolCall,
                });
                events.push(StreamEvent::ToolCallStart {
                    index,
                    id: tc.id,
                    name: tc.name,
                });
                let args_str = match tc.arguments {
                    ToolCallArguments::Complete { value } => value.to_string(),
                    ToolCallArguments::Streaming { data } => data,
                };
                events.push(StreamEvent::ToolCallArgumentsDelta {
                    index,
                    json_fragment: args_str,
                });
                events.push(StreamEvent::ToolCallStop { index });
                events.push(StreamEvent::ContentBlockStop { index });
            }
            // Image / Audio / File on a unary response have no canonical
            // streaming representation — drop them. The unary `CanonicalResponse`
            // surface is still the place to read them.
            ContentBlock::Image(_)
            | ContentBlock::Audio(_)
            | ContentBlock::File(_)
            | ContentBlock::ToolResult(_)
            | ContentBlock::RedactedReasoning(_)
            | ContentBlock::Unsupported(_) => {
                // Intentionally skipped on the streaming surface.
            }
        }
    }

    events.push(StreamEvent::MessageStop {
        stop_reason: resp.stop_reason,
        stop_sequence: resp.stop_sequence,
    });
    events.push(StreamEvent::ResponseStop { usage: resp.usage });

    events
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// from_config factory
// ---------------------------------------------------------------------------

/// Build a [`GeminiProvider`] from a gateway config upstream block.
///
/// The `upstream_name` ends up as the provider's [`BackendProvider::name`]
/// — leaked once at startup so it satisfies the `&'static str` slot.
/// One leak per upstream entry is bounded by the config file size.
pub fn from_config(
    upstream_name: &str,
    cfg: &agent_shim_config::GeminiUpstream,
) -> Result<GeminiProvider, ProviderError> {
    let leaked: &'static str = Box::leak(upstream_name.to_string().into_boxed_str());
    GeminiProvider::new(
        leaked,
        &cfg.base_url,
        cfg.api_key.expose(),
        cfg.default_headers.clone(),
        cfg.request_timeout_secs,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        content::TextBlock, ContentBlock, ExtensionMap, ResponseId, StopReason, ToolCallBlock,
        ToolCallId, Usage,
    };
    use futures::StreamExt;
    use serde_json::json;

    fn provider() -> GeminiProvider {
        GeminiProvider::new(
            "gemini",
            "https://generativelanguage.googleapis.com/v1beta",
            "test-key",
            BTreeMap::new(),
            30,
        )
        .expect("provider builds")
    }

    // ----- Construction & capabilities ------------------------------------

    #[test]
    fn provider_constructs_with_expected_capabilities() {
        let p = provider();
        assert_eq!(p.name(), "gemini");
        let caps = p.capabilities();
        assert!(caps.streaming);
        assert!(caps.tool_use);
        assert!(
            caps.vision,
            "Gemini supports inline_data + file_data images"
        );
        assert!(caps.json_mode);
    }

    #[test]
    fn provider_construction_rejects_invalid_header_name() {
        let mut headers = BTreeMap::new();
        headers.insert("bad header!".to_string(), "value".to_string());
        let err = GeminiProvider::new("gemini", "https://example.com", "key", headers, 30)
            .expect_err("must reject invalid header name");
        assert!(matches!(err, ProviderError::Encode(_)));
    }

    #[test]
    fn provider_construction_rejects_invalid_header_value() {
        let mut headers = BTreeMap::new();
        // Newline is rejected by HeaderValue::from_str.
        headers.insert("x-trace".to_string(), "bad\nvalue".to_string());
        let err = GeminiProvider::new("gemini", "https://example.com", "key", headers, 30)
            .expect_err("must reject invalid header value");
        assert!(matches!(err, ProviderError::Encode(_)));
    }

    // ----- Unary → canonical stream adapter -------------------------------

    fn text_response(text: &str) -> CanonicalResponse {
        CanonicalResponse {
            id: ResponseId::new(),
            model: "gemini-2.0-flash".into(),
            content: vec![ContentBlock::Text(TextBlock {
                text: text.into(),
                extensions: ExtensionMap::new(),
            })],
            stop_reason: StopReason::EndTurn,
            stop_sequence: None,
            usage: Some(Usage {
                input_tokens: Some(7),
                output_tokens: Some(3),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn canonical_response_to_events_emits_well_formed_text_sequence() {
        let events = canonical_response_to_events(text_response("hello"));
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                StreamEvent::ResponseStart { .. } => "ResponseStart",
                StreamEvent::MessageStart { .. } => "MessageStart",
                StreamEvent::ContentBlockStart { .. } => "ContentBlockStart",
                StreamEvent::TextDelta { .. } => "TextDelta",
                StreamEvent::ContentBlockStop { .. } => "ContentBlockStop",
                StreamEvent::MessageStop { .. } => "MessageStop",
                StreamEvent::ResponseStop { .. } => "ResponseStop",
                _ => "Other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "ResponseStart",
                "MessageStart",
                "ContentBlockStart",
                "TextDelta",
                "ContentBlockStop",
                "MessageStop",
                "ResponseStop",
            ]
        );
    }

    #[test]
    fn canonical_response_to_events_omits_text_delta_for_empty_string() {
        let events = canonical_response_to_events(text_response(""));
        assert!(!events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { .. })));
        // Block start/stop still bracket the empty block so the encoder
        // sees a balanced lifecycle.
        let starts = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ContentBlockStart { .. }))
            .count();
        let stops = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ContentBlockStop { .. }))
            .count();
        assert_eq!(starts, 1);
        assert_eq!(stops, 1);
    }

    #[test]
    fn canonical_response_to_events_emits_tool_call_block_in_order() {
        let resp = CanonicalResponse {
            id: ResponseId::new(),
            model: "gemini-2.0-flash".into(),
            content: vec![ContentBlock::ToolCall(ToolCallBlock {
                id: ToolCallId::from_provider("call_1"),
                name: "get_weather".into(),
                arguments: ToolCallArguments::Complete {
                    value: json!({"city": "Paris"}),
                },
                extensions: ExtensionMap::new(),
            })],
            stop_reason: StopReason::ToolUse,
            stop_sequence: None,
            usage: None,
        };
        let events = canonical_response_to_events(resp);

        // Find the args delta and confirm it matches the JSON.
        let args = events
            .iter()
            .find_map(|e| {
                if let StreamEvent::ToolCallArgumentsDelta { json_fragment, .. } = e {
                    Some(json_fragment.as_str())
                } else {
                    None
                }
            })
            .expect("args delta present");
        assert_eq!(args, r#"{"city":"Paris"}"#);

        // ToolCallStop must come before ContentBlockStop.
        let tool_stop_idx = events
            .iter()
            .position(|e| matches!(e, StreamEvent::ToolCallStop { .. }))
            .expect("tool call stop");
        let block_stop_idx = events
            .iter()
            .position(|e| matches!(e, StreamEvent::ContentBlockStop { .. }))
            .expect("content block stop");
        assert!(tool_stop_idx < block_stop_idx);
    }

    #[test]
    fn canonical_response_to_events_propagates_stop_reason_and_usage() {
        let resp = text_response("ok");
        let events = canonical_response_to_events(resp);
        // MessageStop carries the stop reason; ResponseStop carries usage.
        let mut saw_stop_reason = None;
        let mut saw_usage = None;
        for e in events {
            match e {
                StreamEvent::MessageStop { stop_reason, .. } => saw_stop_reason = Some(stop_reason),
                StreamEvent::ResponseStop { usage } => saw_usage = usage,
                _ => {}
            }
        }
        assert_eq!(saw_stop_reason, Some(StopReason::EndTurn));
        let usage = saw_usage.expect("usage present");
        assert_eq!(usage.input_tokens, Some(7));
        assert_eq!(usage.output_tokens, Some(3));
    }

    #[test]
    fn canonical_response_to_events_drops_image_blocks_silently() {
        use agent_shim_core::{content::ImageBlock, BinarySource};
        let resp = CanonicalResponse {
            id: ResponseId::new(),
            model: "gemini-2.0-flash".into(),
            content: vec![
                ContentBlock::Text(TextBlock {
                    text: "a".into(),
                    extensions: ExtensionMap::new(),
                }),
                ContentBlock::Image(ImageBlock {
                    source: BinarySource::Url {
                        url: "https://example.com/x.png".into(),
                    },
                    extensions: ExtensionMap::new(),
                }),
                ContentBlock::Text(TextBlock {
                    text: "b".into(),
                    extensions: ExtensionMap::new(),
                }),
            ],
            stop_reason: StopReason::EndTurn,
            stop_sequence: None,
            usage: None,
        };
        let events = canonical_response_to_events(resp);
        // Two text blocks emitted, image silently dropped.
        let block_starts = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ContentBlockStart { .. }))
            .count();
        assert_eq!(block_starts, 2);
    }

    // ----- Unary bytes → CanonicalStream end-to-end -----------------------

    #[tokio::test]
    async fn unary_bytes_to_canonical_stream_decodes_and_emits_events() {
        let body = br#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#;
        let stream = unary_bytes_to_canonical_stream(body, "gemini-2.0-flash");
        let collected: Vec<_> = stream.collect().await;
        assert!(collected.iter().all(|r| r.is_ok()));
        // Find the TextDelta and confirm content.
        let text = collected
            .iter()
            .find_map(|r| match r.as_ref().unwrap() {
                StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .expect("TextDelta present");
        assert_eq!(text, "hi");
    }

    #[tokio::test]
    async fn unary_bytes_to_canonical_stream_emits_decode_error_for_invalid_json() {
        let body = b"not json at all";
        let stream = unary_bytes_to_canonical_stream(body, "gemini-2.0-flash");
        let collected: Vec<_> = stream.collect().await;
        assert_eq!(collected.len(), 1);
        assert!(matches!(&collected[0], Err(StreamError::Decode(_))));
    }

    // ----- Mockito integration: full HTTP round-trip ----------------------

    fn provider_with_base(base: &str) -> GeminiProvider {
        GeminiProvider::new("gemini", base.to_string(), "test-key", BTreeMap::new(), 30)
            .expect("provider builds")
    }

    fn canonical_request(model: &str, stream: bool) -> CanonicalRequest {
        use agent_shim_core::{
            request::RequestMetadata, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel,
            GenerationOptions, Message, RequestId, ResolvedPolicy,
        };
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: FrontendModel::from(model),
            },
            model: FrontendModel::from(model),
            system: vec![],
            messages: vec![Message::user(vec![ContentBlock::text("hi")])],
            tools: vec![],
            tool_choice: Default::default(),
            generation: GenerationOptions::default(),
            response_format: None,
            stream,
            metadata: RequestMetadata::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: ResolvedPolicy::default(),
            extensions: ExtensionMap::new(),
        }
    }

    fn target(model: &str) -> BackendTarget {
        BackendTarget {
            provider: "gemini".into(),
            model: model.into(),
            policy: Default::default(),
        }
    }

    #[tokio::test]
    async fn complete_unary_round_trips_through_mock_server() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", mockito::Matcher::Regex(
                r"/models/gemini-2\.0-flash:generateContent.*key=test-key".into(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hello"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#)
            .create_async()
            .await;

        let provider = provider_with_base(&server.url());
        let stream = provider
            .complete(
                canonical_request("gemini-2.0-flash", false),
                target("gemini-2.0-flash"),
            )
            .await
            .expect("complete ok");
        let events: Vec<_> = stream.collect().await;
        let text = events
            .iter()
            .find_map(|r| match r.as_ref().unwrap() {
                StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .expect("text delta");
        assert_eq!(text, "hello");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn complete_streaming_uses_streaming_endpoint_and_parses_json_array() {
        let mut server = mockito::Server::new_async().await;
        // Two-object stream body — verifies the URL switch to
        // `:streamGenerateContent` AND end-to-end parsing through T5+T6.
        let body = r#"[
{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]}}]},
{"candidates":[{"content":{"role":"model","parts":[{"text":" world"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3}}
]"#;
        let mock = server
            .mock(
                "POST",
                mockito::Matcher::Regex(
                    r"/models/gemini-2\.0-flash:streamGenerateContent.*key=test-key".into(),
                ),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .create_async()
            .await;

        let provider = provider_with_base(&server.url());
        let stream = provider
            .complete(
                canonical_request("gemini-2.0-flash", true),
                target("gemini-2.0-flash"),
            )
            .await
            .expect("complete ok");

        let events: Vec<_> = stream.collect().await;
        // Concatenate every TextDelta — should be "Hello world".
        let combined: String = events
            .iter()
            .filter_map(|r| match r.as_ref().unwrap() {
                StreamEvent::TextDelta { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(combined, "Hello world");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn complete_returns_upstream_error_on_400() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", mockito::Matcher::Any)
            .with_status(400)
            .with_body(r#"{"error":{"message":"bad request"}}"#)
            .create_async()
            .await;

        let provider = provider_with_base(&server.url());
        // `CanonicalStream` is not Debug, so `expect_err` doesn't work;
        // pattern-match instead.
        let result = provider
            .complete(
                canonical_request("gemini-2.0-flash", false),
                target("gemini-2.0-flash"),
            )
            .await;
        match result {
            Ok(_) => panic!("expected ProviderError::Upstream, got Ok"),
            Err(ProviderError::Upstream { status, body }) => {
                assert_eq!(status, 400);
                assert!(body.contains("bad request"));
            }
            Err(other) => panic!("expected Upstream, got {other:?}"),
        }
        mock.assert_async().await;
    }

    // ----- from_config factory --------------------------------------------

    #[test]
    fn from_config_constructs_provider_from_upstream_block() {
        use agent_shim_config::{GeminiUpstream, Secret};
        let cfg = GeminiUpstream {
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            api_key: Secret::new("test-key"),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
        };
        let provider = from_config("my-gemini", &cfg).expect("from_config ok");
        assert_eq!(provider.name(), "my-gemini");
    }
}
