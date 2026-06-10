//! `summarize_old_turns` strategy — call a configured upstream to compress
//! the oldest groups into a single user-role summary message. On any
//! failure (Err, timeout, empty text), fall back to drop_old_turns
//! behavior so the request always proceeds with a fit-able prompt.
//!
//! Spec §6.5.

#![cfg(feature = "prompt_compressor")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;

use agent_shim_core::{
    BackendTarget, CanonicalRequest, ContentBlock, Message, StreamEvent, TextBlock,
};
use agent_shim_providers::BackendProvider;

use super::groups::group_boundaries;
use super::CompressionResult;

const SUMMARY_PROMPT_PREFIX: &str = "Summarize the prior conversation between user and assistant in <= {N} tokens.\nPreserve key facts, decisions, and unresolved questions.\nOutput plain text only, no greetings.";

const SUMMARY_INJECTION_PREFIX: &str = "[Prior conversation summary]: ";

pub(super) async fn apply(
    messages: &[Message],
    keep_last_n: usize,
    provider: &Arc<dyn BackendProvider>,
    target: &BackendTarget,
    max_summary_tokens: u32,
    timeout: Duration,
) -> CompressionResult {
    let groups = group_boundaries(messages);
    if groups.len() <= keep_last_n + 1 {
        return CompressionResult {
            new_messages: messages.to_vec(),
            dropped: 0,
            action: "skipped",
        };
    }

    let drop_count = groups.len() - keep_last_n;
    let kept_start = groups[drop_count].start;
    let serialized = serialize_groups_to_text(&messages[..kept_start]);

    let summary_req = build_summary_request(target, &serialized, max_summary_tokens);

    let start = Instant::now();
    let result =
        tokio::time::timeout(timeout, provider.complete(summary_req, target.clone())).await;
    let elapsed_secs = start.elapsed().as_secs_f64();

    match result {
        Ok(Ok(stream)) => {
            let summary_text = drain_text_from_stream(stream).await;
            if summary_text.is_empty() {
                metrics::histogram!(
                    agent_shim_observability::metrics::catalog::PluginPromptCompressorSummaryDurationSeconds::NAME,
                    "outcome" => "fallback",
                )
                .record(elapsed_secs);
                tracing::warn!(
                    target: "agent_shim::prompt_compressor",
                    "summarize_old_turns: empty response; falling back to drop_old_turns"
                );
                return CompressionResult {
                    new_messages: messages[kept_start..].to_vec(),
                    dropped: kept_start,
                    action: "summary_fallback",
                };
            }
            metrics::histogram!(
                agent_shim_observability::metrics::catalog::PluginPromptCompressorSummaryDurationSeconds::NAME,
                "outcome" => "success",
            )
            .record(elapsed_secs);
            let mut new_messages = Vec::with_capacity(1 + (messages.len() - kept_start));
            new_messages.push(Message::user(vec![ContentBlock::Text(TextBlock {
                text: format!("{SUMMARY_INJECTION_PREFIX}{summary_text}"),
                extensions: Default::default(),
            })]));
            new_messages.extend_from_slice(&messages[kept_start..]);
            CompressionResult {
                new_messages,
                dropped: kept_start,
                action: "compressed",
            }
        }
        Err(_) | Ok(Err(_)) => {
            metrics::histogram!(
                agent_shim_observability::metrics::catalog::PluginPromptCompressorSummaryDurationSeconds::NAME,
                "outcome" => "fallback",
            )
            .record(elapsed_secs);
            tracing::warn!(
                target: "agent_shim::prompt_compressor",
                "summarize_old_turns: provider error or timeout; falling back to drop_old_turns"
            );
            CompressionResult {
                new_messages: messages[kept_start..].to_vec(),
                dropped: kept_start,
                action: "summary_fallback",
            }
        }
    }
}

fn build_summary_request(
    target: &BackendTarget,
    serialized_conversation: &str,
    max_tokens: u32,
) -> CanonicalRequest {
    use agent_shim_core::{
        ExtensionMap, FrontendInfo, FrontendKind, FrontendModel, GenerationOptions, RequestId,
        ResolvedPolicy,
    };
    let prompt = format!(
        "{}\n\nConversation:\n{serialized_conversation}",
        SUMMARY_PROMPT_PREFIX.replace("{N}", &max_tokens.to_string())
    );
    let generation = GenerationOptions {
        max_tokens: Some(max_tokens),
        ..Default::default()
    };
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::AnthropicMessages,
            requested_model: FrontendModel::from(target.model.as_str()),
        },
        model: FrontendModel::from(target.model.as_str()),
        system: vec![],
        messages: vec![Message::user(vec![ContentBlock::Text(TextBlock {
            text: prompt,
            extensions: Default::default(),
        })])],
        tools: vec![],
        tool_choice: Default::default(),
        generation,
        response_format: None,
        stream: false,
        metadata: Default::default(),
        inbound_anthropic_headers: vec![],
        resolved_policy: ResolvedPolicy::default(),
        extensions: ExtensionMap::new(),
    }
}

async fn drain_text_from_stream(mut stream: agent_shim_core::CanonicalStream) -> String {
    let mut acc = String::new();
    while let Some(item) = stream.next().await {
        if let Ok(StreamEvent::TextDelta { text, .. }) = item {
            acc.push_str(&text);
        }
    }
    acc
}

pub(super) fn serialize_groups_to_text(messages: &[Message]) -> String {
    use agent_shim_core::MessageRole;
    let mut out = String::new();
    for msg in messages {
        let role_label = match msg.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::Tool => "Tool",
            // Positional Message{role:System} entries are rendered as
            // "System" in the summarization transcript — they carry
            // mid-conversation instructions originally from
            // OpenAI-style `system`/`developer` roles.
            MessageRole::System => "System",
        };
        let mut text_parts: Vec<String> = Vec::new();
        let mut other_lines: Vec<String> = Vec::new();
        for block in &msg.content {
            match block {
                ContentBlock::Text(t) => text_parts.push(t.text.clone()),
                ContentBlock::Reasoning(r) => text_parts.push(r.text.clone()),
                ContentBlock::ToolCall(c) => {
                    let args_str = match &c.arguments {
                        agent_shim_core::ToolCallArguments::Complete { value } => {
                            serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
                        }
                        agent_shim_core::ToolCallArguments::Streaming { data } => data.clone(),
                    };
                    other_lines.push(format!("[tool_call: {}({})]", c.name, args_str));
                }
                ContentBlock::ToolResult(r) => {
                    let body = match &r.content {
                        serde_json::Value::String(s) => s.clone(),
                        v => serde_json::to_string(v).unwrap_or_else(|_| "<binary>".to_string()),
                    };
                    other_lines.push(format!("[tool_result: {body}]"));
                }
                ContentBlock::Image(_) => other_lines.push("[image omitted]".to_string()),
                ContentBlock::Audio(_) => other_lines.push("[audio omitted]".to_string()),
                ContentBlock::File(_) => other_lines.push("[file omitted]".to_string()),
                ContentBlock::RedactedReasoning(_) => {
                    other_lines.push("[redacted_reasoning omitted]".to_string());
                }
                ContentBlock::Unsupported(_) => {
                    other_lines.push("[unsupported omitted]".to_string());
                }
            }
        }
        let text_joined = text_parts.join("\n");
        out.push_str(role_label);
        out.push_str(": ");
        out.push_str(&text_joined);
        out.push('\n');
        for line in &other_lines {
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use agent_shim_core::{
        BackendTarget, CanonicalRequest, ContentBlock, Message, MessageRole, StopReason,
        StreamError, StreamEvent, TextBlock,
    };
    use agent_shim_providers::{BackendProvider, ProviderCapabilities, ProviderError};
    use async_trait::async_trait;
    use futures::stream;

    /// Test-only mock provider. Configurable behavior:
    /// - `delay`: sleep before emitting any stream events
    /// - `outcome`: Ok(text_chunks) -> emit one TextDelta per chunk; Err(_) -> return ProviderError
    struct MockProvider {
        delay: Duration,
        outcome: Result<Vec<String>, ()>,
        caps: ProviderCapabilities,
    }

    impl MockProvider {
        fn new(text_chunks: Vec<&str>) -> Self {
            Self {
                delay: Duration::ZERO,
                outcome: Ok(text_chunks.into_iter().map(|s| s.to_string()).collect()),
                caps: ProviderCapabilities::default(),
            }
        }
        fn err() -> Self {
            Self {
                delay: Duration::ZERO,
                outcome: Err(()),
                caps: ProviderCapabilities::default(),
            }
        }
        fn with_delay(mut self, d: Duration) -> Self {
            self.delay = d;
            self
        }
    }

    #[async_trait]
    impl BackendProvider for MockProvider {
        fn name(&self) -> &'static str {
            "mock"
        }
        fn capabilities(&self) -> &ProviderCapabilities {
            &self.caps
        }
        async fn complete(
            &self,
            _req: CanonicalRequest,
            _target: BackendTarget,
        ) -> Result<agent_shim_core::CanonicalStream, ProviderError> {
            tokio::time::sleep(self.delay).await;
            match &self.outcome {
                Err(()) => Err(ProviderError::Upstream {
                    status: 500,
                    body: "mock failure".to_string(),
                }),
                Ok(chunks) => {
                    let events: Vec<Result<StreamEvent, StreamError>> = chunks
                        .iter()
                        .enumerate()
                        .map(|(i, t)| {
                            Ok(StreamEvent::TextDelta {
                                index: i as u32,
                                text: t.clone(),
                            })
                        })
                        .chain(std::iter::once(Ok(StreamEvent::MessageStop {
                            stop_reason: StopReason::EndTurn,
                            stop_sequence: None,
                        })))
                        .collect();
                    let s: agent_shim_core::CanonicalStream = Box::pin(stream::iter(events));
                    Ok(s)
                }
            }
        }
    }

    fn user(s: &str) -> Message {
        Message::user(vec![ContentBlock::Text(TextBlock {
            text: s.to_string(),
            extensions: Default::default(),
        })])
    }
    fn assistant(s: &str) -> Message {
        Message::assistant(vec![ContentBlock::Text(TextBlock {
            text: s.to_string(),
            extensions: Default::default(),
        })])
    }

    fn target() -> BackendTarget {
        BackendTarget {
            provider: "mock".to_string(),
            model: "test-model".to_string(),
            policy: Default::default(),
        }
    }

    fn five_groups() -> Vec<Message> {
        vec![
            user("g1u"),
            assistant("g1a"),
            user("g2u"),
            assistant("g2a"),
            user("g3u"),
            assistant("g3a"),
            user("g4u"),
            assistant("g4a"),
            user("g5u"),
            assistant("g5a"),
        ]
    }

    #[tokio::test]
    async fn summarize_skipped_when_too_few_groups() {
        // 2 groups, keep_last_n=2 -> skipped (need at least keep + 2 = 4 groups).
        let messages = vec![user("a"), assistant("b"), user("c"), assistant("d")];
        let p: Arc<dyn BackendProvider> = Arc::new(MockProvider::new(vec!["never"]));
        let result = apply(&messages, 2, &p, &target(), 100, Duration::from_secs(1)).await;
        assert_eq!(result.action, "skipped");
    }

    #[tokio::test]
    async fn summarize_success_replaces_old_groups() {
        let messages = five_groups();
        let p: Arc<dyn BackendProvider> = Arc::new(MockProvider::new(vec!["SUMMARY"]));
        let result = apply(&messages, 2, &p, &target(), 100, Duration::from_secs(2)).await;
        assert_eq!(result.action, "compressed");
        // Kept = 2 groups × 2 messages = 4; plus 1 summary message = 5.
        assert_eq!(result.new_messages.len(), 5);
        assert_eq!(result.new_messages[0].role, MessageRole::User);
    }

    #[tokio::test]
    async fn summarize_summary_message_has_prefix() {
        let messages = five_groups();
        let p: Arc<dyn BackendProvider> = Arc::new(MockProvider::new(vec!["MY-SUMMARY"]));
        let result = apply(&messages, 2, &p, &target(), 100, Duration::from_secs(2)).await;
        match &result.new_messages[0].content[0] {
            ContentBlock::Text(t) => {
                assert!(
                    t.text.starts_with(SUMMARY_INJECTION_PREFIX),
                    "expected prefix, got: {}",
                    t.text
                );
                assert!(t.text.contains("MY-SUMMARY"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn summarize_provider_error_falls_back() {
        let messages = five_groups();
        let p: Arc<dyn BackendProvider> = Arc::new(MockProvider::err());
        let result = apply(&messages, 2, &p, &target(), 100, Duration::from_secs(2)).await;
        assert_eq!(result.action, "summary_fallback");
        // Kept = 2 groups × 2 messages = 4 (no summary message prepended).
        assert_eq!(result.new_messages.len(), 4);
    }

    #[tokio::test]
    async fn summarize_timeout_falls_back() {
        let messages = five_groups();
        let p: Arc<dyn BackendProvider> =
            Arc::new(MockProvider::new(vec!["never seen"]).with_delay(Duration::from_secs(10)));
        let result = apply(&messages, 2, &p, &target(), 100, Duration::from_millis(5)).await;
        assert_eq!(result.action, "summary_fallback");
        assert_eq!(result.new_messages.len(), 4);
    }

    #[tokio::test]
    async fn summarize_empty_response_falls_back() {
        let messages = five_groups();
        let p: Arc<dyn BackendProvider> = Arc::new(MockProvider::new(vec![]));
        let result = apply(&messages, 2, &p, &target(), 100, Duration::from_secs(2)).await;
        assert_eq!(result.action, "summary_fallback");
    }

    #[test]
    fn serialize_groups_to_text_format() {
        let messages = vec![user("hi"), assistant("hello there"), user("bye")];
        let out = serialize_groups_to_text(&messages);
        assert!(out.contains("User: hi"));
        assert!(out.contains("Assistant: hello there"));
        assert!(out.contains("User: bye"));
    }
}
