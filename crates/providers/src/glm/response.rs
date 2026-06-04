//! Parse GLM `/chat/completions` responses into a `CanonicalStream`.
//!
//! GLM streams interleaved `reasoning_content` deltas in the same SSE shape
//! as DeepSeek, so this module re-exports the deepseek parsers verbatim.
//! The structured `provider=` field on the tracing events lets log triage
//! distinguish the two — the literal target string ("deepseek SSE event
//! received") is not renamed.
//!
//! If a third "OAI-Chat + reasoning_content" provider arrives later, factor
//! the parser into a shared `oai_chat_wire::reasoning_content_parser` module
//! and have both deepseek and glm re-export from there. YAGNI for now.

pub(crate) use crate::deepseek::response::{parse_stream, parse_unary};

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::CanonicalStream;

    /// Compile-time sanity: if the deepseek parser signatures ever change,
    /// the GLM re-export breaks at this test rather than at runtime.
    #[test]
    fn re_exported_parsers_resolve_to_deepseek_impl() {
        fn _assert_unary(_f: fn(&[u8]) -> CanonicalStream) {}
        _assert_unary(parse_unary);
        let _stream_fn: fn(
            futures::stream::BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
        ) -> CanonicalStream = parse_stream;
    }
}
