//! Shared OpenAI-Chat-shape primitives composed by both `openai_compatible/`
//! and any provider that speaks "OpenAI chat completions with quirks"
//! (e.g. DeepSeek, future Kimi/Qwen). Sibling provider modules use these
//! crate-internal helpers instead of cross-importing each other.

pub(crate) mod canonical_to_chat;
pub(crate) mod chat_sse_parser;
pub(crate) mod chat_unary_parser;
// Consumed by sibling provider modules in Plan 02 T4 (DeepSeek) and Plan 03
// (Gemini). Until those land, the public items are only exercised by tests.
#[allow(dead_code)]
pub(crate) mod interleaved_reasoning;
pub(crate) mod wire;
