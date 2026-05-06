//! Shared OpenAI-Chat-shape primitives composed by both `openai_compatible/`
//! and any provider that speaks "OpenAI chat completions with quirks"
//! (e.g. DeepSeek, future Kimi/Qwen). Sibling provider modules use these
//! crate-internal helpers instead of cross-importing each other.

pub(crate) mod canonical_to_chat;
pub(crate) mod chat_sse_parser;
pub(crate) mod chat_unary_parser;
// Consumed by `deepseek/response.rs` (Plan 02 T4) and slated for use by the
// Gemini provider in Plan 03. The `#[allow(dead_code)]` gate that lived here
// before T4 was removed once the DeepSeek SSE parser became the first
// consumer of `DeltaKind` and `ReasoningInterleaver`.
pub(crate) mod interleaved_reasoning;
pub(crate) mod wire;
