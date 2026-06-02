pub mod lifecycle;
pub mod sse_fixtures;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use agent_shim_core::stream::{CanonicalStream, StreamEvent};
use agent_shim_core::{
    BackendTarget, CanonicalRequest, ContentBlock, ExtensionMap, FrontendInfo, FrontendKind,
    FrontendModel, GenerationOptions, Message, RequestId,
};
use agent_shim_providers::anthropic::AnthropicProvider;
use agent_shim_providers::gemini::GeminiProvider;
use agent_shim_providers::github_copilot::credential_store::StoredCredentials;
use agent_shim_providers::github_copilot::CopilotProvider;
use agent_shim_providers::openai_compatible::OpenAiCompatibleProvider;
use bytes::Bytes;
use futures_util::{stream, StreamExt};

use agent_shim_frontends::FrontendError;

/// Deterministic timestamp used by cross-protocol tests
/// (Sat Nov 14 2023 22:13:20 UTC).
pub const TEST_CLOCK: u64 = 1_700_000_000;

/// Resolve the path to a named fixture file under `fixtures/canonical/`.
pub fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("fixtures");
    p.push("canonical");
    p.push(name);
    p
}

/// Read a JSONL fixture file and replay it as a `CanonicalStream`.
/// Each non-empty line must deserialize as a `StreamEvent`.
/// `per_event_delay` is currently ignored (kept for API compatibility).
pub fn replay_jsonl(path: PathBuf, _per_event_delay: Option<Duration>) -> CanonicalStream {
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture {:?}: {}", path, e));

    let events: Vec<Result<StreamEvent, agent_shim_core::error::StreamError>> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<StreamEvent>(line)
                .map_err(|e| agent_shim_core::error::StreamError::Decode(e.to_string()))
        })
        .collect();

    Box::pin(stream::iter(events))
}

/// Collect all `Bytes` chunks from a boxed stream into a single contiguous `Bytes`.
pub async fn collect_sse(
    mut s: futures_util::stream::BoxStream<'static, Result<Bytes, FrontendError>>,
) -> Bytes {
    let mut out = Vec::new();
    while let Some(chunk) = s.next().await {
        match chunk {
            Ok(b) => out.extend_from_slice(&b),
            Err(e) => panic!("stream error: {}", e),
        }
    }
    Bytes::from(out)
}

/// Build an [`AnthropicProvider`] pointed at the given mockito base URL,
/// with the test API key, version, and a 30s timeout.
pub fn make_anthropic_provider(base_url: String) -> AnthropicProvider {
    AnthropicProvider::new(
        "anthropic",
        base_url,
        "test-key",
        "2023-06-01",
        Default::default(),
        30,
    )
    .expect("AnthropicProvider::new is infallible for these inputs")
}

/// Build a [`BackendTarget`] pointing at `claude-opus-4-7` on the
/// `anthropic` provider.
pub fn make_anthropic_target() -> BackendTarget {
    BackendTarget {
        provider: "anthropic".to_string(),
        model: "claude-opus-4-7".to_string(),
        policy: Default::default(),
    }
}

/// Build a [`GeminiProvider`] pointed at the given mockito base URL,
/// with the test API key, no default headers, and a 30s timeout.
pub fn make_gemini_provider(base_url: String) -> GeminiProvider {
    GeminiProvider::new("gemini", base_url, "test-key", BTreeMap::new(), 30)
        .expect("GeminiProvider::new is infallible for these inputs")
}

/// Build a [`BackendTarget`] pointing at `gemini-2.0-flash` on the
/// `gemini` provider.
pub fn make_gemini_target() -> BackendTarget {
    BackendTarget {
        provider: "gemini".to_string(),
        model: "gemini-2.0-flash".to_string(),
        policy: Default::default(),
    }
}

/// Build an [`OpenAiCompatibleProvider`] pointed at the given mockito
/// base URL, with the test API key, no default headers, and a 30s
/// timeout.
///
/// Used by Plan 04 T4 cross-protocol vision cells (and future
/// Responses-flavoured cells) to drive the OAI-compat upstream from a
/// `frontend.kind = OpenAiResponses` request, which routes through the
/// canonical Chat Completions encoder.
pub fn make_oai_compat_provider(base_url: String) -> OpenAiCompatibleProvider {
    OpenAiCompatibleProvider::new("openai", base_url, "test-key", BTreeMap::new(), 30)
        .expect("OpenAiCompatibleProvider::new is infallible for these inputs")
}

/// Build a [`BackendTarget`] pointing at `gpt-4o` on the `openai`
/// provider.
pub fn make_oai_compat_target() -> BackendTarget {
    BackendTarget {
        provider: "openai".to_string(),
        model: "gpt-4o".to_string(),
        policy: Default::default(),
    }
}

/// Build a [`CopilotProvider`] pointed at the given mockito base URL.
///
/// Copilot's `complete()` calls the token manager up-front, so tests
/// using this helper must also stand up a mockito mock for
/// `GET /copilot_internal/v2/token` that returns a token whose
/// `endpoints.api` points back at the SAME mockito server (so
/// downstream Chat / Responses calls land on the same mock).
///
/// The provider is constructed with `spawn_with_creds`, which preloads
/// dummy GitHub OAuth credentials and bypasses on-disk credential
/// loading entirely — only the upstream HTTP exchange is exercised.
pub fn make_copilot_provider(base_url: String) -> CopilotProvider {
    let creds = StoredCredentials {
        github_oauth_token: "gho_test".to_string(),
        created_at_unix: 0,
    };
    CopilotProvider::spawn_with_creds(creds, base_url)
        .expect("CopilotProvider::spawn_with_creds is infallible for these inputs")
}

/// Build a [`BackendTarget`] pointing at `gpt-4o` on the
/// `github_copilot` provider.
pub fn make_copilot_target() -> BackendTarget {
    BackendTarget {
        provider: "github_copilot".to_string(),
        model: "gpt-4o".to_string(),
        policy: Default::default(),
    }
}

/// Build a synthetic [`CanonicalRequest`] shaped as if it came from a
/// non-Anthropic frontend (single user message with `"hello"` text content).
pub fn make_canonical_request(stream: bool, frontend_kind: FrontendKind) -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: frontend_kind,
            requested_model: FrontendModel::from("gpt-4o"),
        },
        model: FrontendModel::from("gpt-4o"),
        system: vec![],
        messages: vec![Message::user(vec![ContentBlock::text("hello")])],
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
