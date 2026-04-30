//! Parse DeepSeek chat-completions responses into a `CanonicalStream`.
//!
//! Today both functions delegate to the shared OAI-Chat parsers. Plan 02 T4
//! will replace `parse_stream` with a DeepSeek-aware parser that extracts
//! `reasoning_content` deltas (interleaved with `content`) using the
//! `oai_chat_wire::interleaved_reasoning` state machine. Plan 02 T5 will
//! extend `parse_unary` to map DeepSeek's cache-token usage fields onto the
//! canonical `Usage` struct.
//!
//! Keeping the wrappers in place now sets the call-site shape so the later
//! tasks become focused inner-function changes, not refactors of `mod.rs`.

use agent_shim_core::CanonicalStream;
use bytes::Bytes;
use futures_core::Stream;

/// Parse a streaming SSE response from DeepSeek into a `CanonicalStream`.
///
/// T3: delegates to the shared OAI-Chat SSE parser.
/// T4: will swap in a parser that extracts `reasoning_content` deltas.
pub(crate) fn parse_stream<S>(byte_stream: S) -> CanonicalStream
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    crate::oai_chat_wire::chat_sse_parser::parse(byte_stream)
}

/// Parse a non-streaming JSON response from DeepSeek into a `CanonicalStream`.
///
/// T3: delegates to the shared OAI-Chat unary parser.
/// T5: will add a cache-usage mapping step on top of the canonical events.
pub(crate) fn parse_unary(body: &[u8]) -> CanonicalStream {
    crate::oai_chat_wire::chat_unary_parser::parse(body)
}
