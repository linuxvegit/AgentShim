//! Shared SSE fixture bodies for OpenAI-Chat-shape upstreams. These are the
//! exact byte sequences that pre-existing per-provider smoke tests fed into
//! their mocked upstreams (see `crates/providers/tests/openai_compatible_smoke.rs`).
//!
//! Centralised here so the lifecycle-validator integration tests in
//! `crates/protocol-tests/tests/chat_sse_parser_lifecycle.rs` can exercise the
//! same wire shapes the smoke tests rely on, without forcing both test
//! crates to duplicate the SSE payloads.

/// Two-chunk streaming completion ending in `finish_reason=stop` plus
/// `[DONE]`. Lifted from `streaming_yields_text_deltas` in
/// `openai_compatible_smoke.rs`.
pub const STREAM_TWO_TEXT_DELTAS_THEN_DONE: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// Streaming completion that never sends a `finish_reason` chunk and goes
/// straight to `[DONE]` with no content emitted at all. Models the corner
/// case where the upstream hangs up after acknowledging the request with a
/// `role: "assistant"` chunk but before producing any output.
pub const STREAM_DONE_BEFORE_ANY_DELTA: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
    "data: [DONE]\n\n",
);

/// Streaming completion that emits text content but never a `finish_reason`
/// or `[DONE]` — the upstream just drops the connection mid-stream. The
/// parser must emit a clean lifecycle close even without an explicit
/// terminator, because the H7 plugin hook and the streaming usage logger
/// both rely on a single `ResponseStop` per stream.
pub const STREAM_CONTENT_THEN_DROP: &str =
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":null}]}\n\n";

/// Streaming completion with one tool call followed by `finish_reason=tool_calls`
/// and `[DONE]`. The tool block must open at canonical index 1 (since text
/// uses index 0 even though no text was emitted) and close with the
/// `ToolCallStop → ContentBlockStop` pair the canonical lifecycle requires.
pub const STREAM_SINGLE_TOOL_CALL_THEN_DONE: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\\\"SF\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: [DONE]\n\n",
);
