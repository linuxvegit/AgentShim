//! Wire → canonical translation for Gemini Generate Content responses.
//!
//! Plan 03 T6. Two entry points:
//!
//! - [`parse_unary`] — turns a single [`super::wire::GenerateContentResponse`]
//!   into a [`CanonicalResponse`] for the non-streaming path.
//! - [`parse_streaming`] — wraps a
//!   [`super::stream::ResponseStream`] (T5) and translates each emitted
//!   wire response into one or more [`StreamEvent`]s.
//!
//! ## Per-block translation
//!
//! For each `Part` inside `candidates[0].content.parts`:
//!
//! | Wire shape                       | Canonical block          |
//! |----------------------------------|--------------------------|
//! | `text` set, `thought != true`    | `ContentBlock::Text`     |
//! | `text` set, `thought == true`    | `ContentBlock::Reasoning`|
//! | `inlineData { mimeType, data }`  | `ContentBlock::Image` (`BinarySource::Base64`)|
//! | `fileData { fileUri }`           | `ContentBlock::Image` (`BinarySource::Url`)   |
//! | `functionCall { name, args }`    | `ContentBlock::ToolCall` |
//! | `functionResponse { ... }`       | dropped (not expected on assistant turns) |
//!
//! ## Stop-reason mapping
//!
//! | Gemini `finishReason` | Canonical [`StopReason`] |
//! |-----------------------|--------------------------|
//! | `STOP`                | `EndTurn` (or `ToolUse` if any function call present) |
//! | `MAX_TOKENS`          | `MaxTokens`              |
//! | `SAFETY`              | `ContentFilter`          |
//! | `RECITATION`          | `ContentFilter`          |
//! | other / missing       | `EndTurn` (default)      |
//!
//! ## Provider-specific data (ADR-0002 frozen-core)
//!
//! Gemini ships data without a canonical home — it lands in `extensions`:
//!
//! - `safetyRatings` (per candidate) → assistant-message
//!   `extensions["gemini.safety_ratings"]`.
//! - `promptFeedback` (top-level) → currently dropped on the unary path
//!   and emitted as a `RawProviderEvent` on the streaming path so callers
//!   can surface it in tracing.
//! - `citationMetadata` (per candidate) → assistant-message
//!   `extensions["gemini.citation_metadata"]` when present.
//!
//! ## Streaming state machine
//!
//! Gemini chunks each turn as a sequence of partial `GenerateContentResponse`s.
//! Each one carries `candidates[0].content.parts` with the new text/parts
//! since the last chunk. We collapse them into per-block deltas:
//!
//! - The first response triggers `ResponseStart` + `MessageStart`.
//! - A run of consecutive `text` parts opens one `ContentBlockStart{Text}`,
//!   emits a `TextDelta` per part, and closes with `ContentBlockStop` when
//!   a non-text part or a `finishReason` arrives.
//! - A run of consecutive `thought == true` parts becomes a single
//!   `Reasoning` block in the same fashion (`ReasoningDelta`).
//! - A `functionCall` part becomes a complete tool block (we emit
//!   `ContentBlockStart{ToolCall}`, `ToolCallStart`, a single
//!   `ToolCallArgumentsDelta` with the entire stringified args, then
//!   `ToolCallStop` + `ContentBlockStop`). Gemini does NOT stream tool-call
//!   arguments — each chunk carries a complete `args` object.
//! - On `finishReason`, the parser closes any still-open block, emits
//!   `MessageStop`, then `ResponseStop` carrying any usage metadata that
//!   accompanied the final chunk.

use agent_shim_core::{
    content::{ImageBlock, ReasoningBlock, TextBlock},
    BinarySource, CanonicalResponse, CanonicalStream, ContentBlock, ContentBlockKind, ExtensionMap,
    MessageRole, ResponseId, StopReason, StreamError, StreamEvent, ToolCallArguments,
    ToolCallBlock, ToolCallId, Usage,
};
use bytes::Bytes;
use futures::StreamExt;

use super::stream::ResponseStream;
use super::wire::{Candidate, FunctionCall, GenerateContentResponse, Part, UsageMetadata};

// ---------------------------------------------------------------------------
// Unary
// ---------------------------------------------------------------------------

/// Translate a single (non-streaming) Gemini response into a canonical one.
///
/// `model` is passed in by the caller (the request-side model identifier,
/// since Gemini's response payload doesn't echo it back).
pub(crate) fn parse_unary(
    resp: GenerateContentResponse,
    model: impl Into<String>,
) -> CanonicalResponse {
    // Gemini may return zero candidates when the prompt was blocked — the
    // safest behaviour is to return an empty assistant message with the
    // mapped stop reason so callers don't have to special-case it.
    let candidate = resp.candidates.into_iter().next();

    let (content_blocks, has_tool_call, finish_reason, safety_ratings, citation_metadata) =
        match candidate {
            Some(c) => candidate_to_blocks(c),
            None => (Vec::new(), false, None, Vec::new(), None),
        };

    let stop_reason = map_finish_reason(finish_reason.as_deref(), has_tool_call);

    // Hoist provider-specific bits into the assistant block extensions when
    // there's at least one block to attach them to. If there are zero blocks
    // (e.g. blocked prompt), we drop them — the canonical response carries
    // no extension surface.
    let content = attach_extensions(content_blocks, safety_ratings, citation_metadata);

    let usage = resp.usage_metadata.map(usage_metadata_to_usage);

    CanonicalResponse {
        id: ResponseId::new(),
        model: model.into(),
        content,
        stop_reason,
        stop_sequence: None, // Gemini doesn't surface the matched stop sequence.
        usage,
    }
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

/// Wrap a Gemini response stream and emit canonical [`StreamEvent`]s.
///
/// This is the "real" parser the gateway will use on streaming requests.
/// `model` is captured at call time and embedded in the `ResponseStart`
/// event so downstream encoders can echo it back to the agent.
pub(crate) fn parse_streaming(stream: ResponseStream, model: String) -> CanonicalStream {
    // Same `Arc<Mutex<...>>` pattern as the byte-level Scanner: the state
    // is mutated by per-item processing AND by the post-EOF drain step.
    // Mutex is uncontended (the chain composes serially).
    let state = std::sync::Arc::new(std::sync::Mutex::new(StreamState::new(model)));
    let state_for_items = std::sync::Arc::clone(&state);
    let event_stream = stream.flat_map(move |item| {
        let events = {
            let mut s = state_for_items.lock().expect("state mutex poisoned");
            match item {
                Ok(resp) => s.handle(resp),
                Err(e) => vec![Err(e)],
            }
        };
        futures::stream::iter(events)
    });

    // After the upstream ends, drain any open block / unsent ResponseStop
    // so encoders see a well-formed close. Streams that never emitted a
    // ResponseStart (e.g. immediate upstream error) skip the drain — there's
    // nothing to close.
    let drain = futures::stream::once(async move {
        let mut s = state.lock().expect("state mutex poisoned");
        s.finalize()
    })
    .flat_map(futures::stream::iter);

    Box::pin(event_stream.chain(drain))
}

/// Per-stream state: tracks open blocks and bookkeeping needed to emit
/// well-formed canonical events.
struct StreamState {
    model: String,
    response_id: ResponseId,
    /// Have we emitted `ResponseStart` + `MessageStart` yet?
    started: bool,
    /// Have we emitted `ResponseStop`? Once true, all further events are
    /// suppressed — Gemini sometimes appends usage-only chunks after the
    /// finish_reason chunk and the gateway guarantees a single ResponseStop.
    stopped: bool,
    /// Index of the next content block to open. Monotonic across the whole
    /// stream — never reused.
    next_block_index: u32,
    /// State of the currently-open block, if any.
    open_block: Option<OpenBlock>,
    /// Have we observed at least one functionCall in this turn? Used to
    /// upgrade STOP → ToolUse on MessageStop.
    has_tool_call: bool,
    /// Latest usage_metadata seen — emitted on ResponseStop.
    last_usage: Option<UsageMetadata>,
}

#[derive(Debug, PartialEq, Eq)]
enum OpenBlock {
    Text { index: u32 },
    Reasoning { index: u32 },
}

impl StreamState {
    fn new(model: String) -> Self {
        Self {
            model,
            response_id: ResponseId::new(),
            started: false,
            stopped: false,
            next_block_index: 0,
            open_block: None,
            has_tool_call: false,
            last_usage: None,
        }
    }

    /// Process a single inbound response object.
    fn handle(&mut self, resp: GenerateContentResponse) -> Vec<Result<StreamEvent, StreamError>> {
        if self.stopped {
            return Vec::new();
        }

        let mut out: Vec<Result<StreamEvent, StreamError>> = Vec::new();

        if !self.started {
            self.started = true;
            out.push(Ok(StreamEvent::ResponseStart {
                id: self.response_id.clone(),
                model: self.model.clone(),
                created_at_unix: now_unix(),
            }));
            out.push(Ok(StreamEvent::MessageStart {
                role: MessageRole::Assistant,
            }));
        }

        // Capture the latest usage payload — emit on ResponseStop.
        if let Some(u) = resp.usage_metadata {
            self.last_usage = Some(u);
        }

        // Walk the parts of the first candidate. Gemini's API emits one
        // candidate at a time on the streaming endpoint; if multiple are
        // ever sent we honor the first and ignore the rest.
        let Some(candidate) = resp.candidates.into_iter().next() else {
            return out;
        };

        for part in candidate.content.parts {
            self.handle_part(part, &mut out);
        }

        // finishReason on this candidate signals end-of-turn.
        if let Some(reason) = candidate.finish_reason {
            self.close_open_block(&mut out);
            let stop = map_finish_reason(Some(reason.as_str()), self.has_tool_call);
            out.push(Ok(StreamEvent::MessageStop {
                stop_reason: stop,
                stop_sequence: None,
            }));
            out.push(Ok(StreamEvent::ResponseStop {
                usage: self.last_usage.take().map(usage_metadata_to_usage),
            }));
            self.stopped = true;
        }

        out
    }

    fn handle_part(&mut self, part: Part, out: &mut Vec<Result<StreamEvent, StreamError>>) {
        // Reasoning vs text vs function call — these change the open block.
        let is_thought = part.thought == Some(true);

        if let Some(fc) = part.function_call {
            // Tool calls are emitted as complete blocks (Gemini doesn't
            // stream args). Close any text/reasoning first.
            self.close_open_block(out);
            self.emit_tool_call(fc, out);
            return;
        }

        if let Some(text) = part.text {
            if is_thought {
                self.ensure_reasoning_block(out);
                if let Some(OpenBlock::Reasoning { index }) = self.open_block {
                    out.push(Ok(StreamEvent::ReasoningDelta { index, text }));
                }
            } else {
                self.ensure_text_block(out);
                if let Some(OpenBlock::Text { index }) = self.open_block {
                    out.push(Ok(StreamEvent::TextDelta { index, text }));
                }
            }
            // No `return` here — the function ends after this block. (The
            // clippy lint flags it as unneeded; we honour that.)
        }

        // Inline data / file data on a streaming response would represent
        // a model-emitted image (rare with current models). The canonical
        // streaming surface doesn't have a dedicated event for binary
        // payloads, so we drop them and rely on the unary path to surface
        // images. No encoder currently consumes these in streaming mode.
        // (If a future model needs it, this is where ImageDelta would go.)
    }

    fn ensure_text_block(&mut self, out: &mut Vec<Result<StreamEvent, StreamError>>) {
        match self.open_block {
            Some(OpenBlock::Text { .. }) => {}
            Some(OpenBlock::Reasoning { .. }) => {
                self.close_open_block(out);
                self.open_text_block(out);
            }
            None => self.open_text_block(out),
        }
    }

    fn ensure_reasoning_block(&mut self, out: &mut Vec<Result<StreamEvent, StreamError>>) {
        match self.open_block {
            Some(OpenBlock::Reasoning { .. }) => {}
            Some(OpenBlock::Text { .. }) => {
                self.close_open_block(out);
                self.open_reasoning_block(out);
            }
            None => self.open_reasoning_block(out),
        }
    }

    fn open_text_block(&mut self, out: &mut Vec<Result<StreamEvent, StreamError>>) {
        let index = self.alloc_index();
        out.push(Ok(StreamEvent::ContentBlockStart {
            index,
            kind: ContentBlockKind::Text,
        }));
        self.open_block = Some(OpenBlock::Text { index });
    }

    fn open_reasoning_block(&mut self, out: &mut Vec<Result<StreamEvent, StreamError>>) {
        let index = self.alloc_index();
        out.push(Ok(StreamEvent::ContentBlockStart {
            index,
            kind: ContentBlockKind::Reasoning,
        }));
        self.open_block = Some(OpenBlock::Reasoning { index });
    }

    fn close_open_block(&mut self, out: &mut Vec<Result<StreamEvent, StreamError>>) {
        if let Some(block) = self.open_block.take() {
            let index = match block {
                OpenBlock::Text { index } | OpenBlock::Reasoning { index } => index,
            };
            out.push(Ok(StreamEvent::ContentBlockStop { index }));
        }
    }

    fn emit_tool_call(
        &mut self,
        fc: FunctionCall,
        out: &mut Vec<Result<StreamEvent, StreamError>>,
    ) {
        self.has_tool_call = true;
        let index = self.alloc_index();
        // Gemini-side ids appear in some response shapes but aren't
        // guaranteed; mint a fresh canonical id when missing. Using
        // `unwrap_or_default()` here delegates to `ToolCallId::default()`
        // which is the same as `ToolCallId::new()` (uuid-prefixed).
        let id = fc.id.map(ToolCallId::from_provider).unwrap_or_default();

        out.push(Ok(StreamEvent::ContentBlockStart {
            index,
            kind: ContentBlockKind::ToolCall,
        }));
        out.push(Ok(StreamEvent::ToolCallStart {
            index,
            id,
            name: fc.name,
        }));
        // Args is a real JSON object on the wire; serialize once and ship
        // as a single complete fragment so encoders can pass it through.
        let args_str = match fc.args {
            Some(value) => value.to_string(),
            None => "{}".to_string(),
        };
        out.push(Ok(StreamEvent::ToolCallArgumentsDelta {
            index,
            json_fragment: args_str,
        }));
        out.push(Ok(StreamEvent::ToolCallStop { index }));
        out.push(Ok(StreamEvent::ContentBlockStop { index }));
    }

    fn alloc_index(&mut self) -> u32 {
        let i = self.next_block_index;
        self.next_block_index += 1;
        i
    }

    /// Drain at end-of-stream. If we never received a finishReason but the
    /// upstream closed cleanly, we still want to emit a ResponseStop (and
    /// close any open block) so encoders can finalize the SSE.
    fn finalize(&mut self) -> Vec<Result<StreamEvent, StreamError>> {
        if !self.started || self.stopped {
            return Vec::new();
        }
        let mut out: Vec<Result<StreamEvent, StreamError>> = Vec::new();
        self.close_open_block(&mut out);
        out.push(Ok(StreamEvent::MessageStop {
            stop_reason: if self.has_tool_call {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            stop_sequence: None,
        }));
        out.push(Ok(StreamEvent::ResponseStop {
            usage: self.last_usage.take().map(usage_metadata_to_usage),
        }));
        self.stopped = true;
        out
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Translate a candidate's parts into canonical content blocks.
///
/// Returns:
/// - the canonical content blocks,
/// - whether any block is a tool call (drives STOP → ToolUse upgrade),
/// - the candidate's finish_reason (string),
/// - safety ratings (for extensions),
/// - citation metadata (for extensions).
fn candidate_to_blocks(
    candidate: Candidate,
) -> (
    Vec<ContentBlock>,
    bool,
    Option<String>,
    Vec<super::wire::SafetyRating>,
    Option<serde_json::Value>,
) {
    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut has_tool_call = false;
    // Reasoning and text parts coalesce into one block per run.
    let mut acc_text: Option<String> = None;
    let mut acc_reasoning: Option<String> = None;

    let flush_text = |dst: &mut Vec<ContentBlock>, t: &mut Option<String>| {
        if let Some(text) = t.take() {
            dst.push(ContentBlock::Text(TextBlock {
                text,
                extensions: ExtensionMap::new(),
            }));
        }
    };
    let flush_reasoning = |dst: &mut Vec<ContentBlock>, t: &mut Option<String>| {
        if let Some(text) = t.take() {
            dst.push(ContentBlock::Reasoning(ReasoningBlock {
                text,
                extensions: ExtensionMap::new(),
            }));
        }
    };

    for part in candidate.content.parts {
        let is_thought = part.thought == Some(true);

        if let Some(fc) = part.function_call {
            // Flush any pending coalesced text/reasoning before emitting
            // the tool call, preserving order.
            flush_text(&mut blocks, &mut acc_text);
            flush_reasoning(&mut blocks, &mut acc_reasoning);
            has_tool_call = true;
            let id = fc.id.map(ToolCallId::from_provider).unwrap_or_default();
            blocks.push(ContentBlock::ToolCall(ToolCallBlock {
                id,
                name: fc.name,
                arguments: ToolCallArguments::Complete {
                    value: fc.args.unwrap_or(serde_json::json!({})),
                },
                extensions: ExtensionMap::new(),
            }));
            continue;
        }

        if let Some(text) = part.text {
            if is_thought {
                // If we were accumulating text, flush it before switching
                // to reasoning so the order stays stable.
                flush_text(&mut blocks, &mut acc_text);
                acc_reasoning
                    .get_or_insert_with(String::new)
                    .push_str(&text);
            } else {
                flush_reasoning(&mut blocks, &mut acc_reasoning);
                acc_text.get_or_insert_with(String::new).push_str(&text);
            }
            continue;
        }

        if let Some(inline) = part.inline_data {
            flush_text(&mut blocks, &mut acc_text);
            flush_reasoning(&mut blocks, &mut acc_reasoning);
            // Decode base64 → bytes for the canonical block. If decoding
            // fails (server bug), drop the part rather than crashing.
            use base64::{engine::general_purpose::STANDARD, Engine as _};
            if let Ok(decoded) = STANDARD.decode(inline.data.as_bytes()) {
                blocks.push(ContentBlock::Image(ImageBlock {
                    source: BinarySource::Base64 {
                        media_type: inline.mime_type,
                        data: Bytes::from(decoded),
                    },
                    extensions: ExtensionMap::new(),
                }));
            }
            continue;
        }

        if let Some(file) = part.file_data {
            flush_text(&mut blocks, &mut acc_text);
            flush_reasoning(&mut blocks, &mut acc_reasoning);
            blocks.push(ContentBlock::Image(ImageBlock {
                source: BinarySource::Url { url: file.file_uri },
                extensions: ExtensionMap::new(),
            }));
            continue;
        }
        // function_response on an assistant turn shouldn't happen — drop.
    }

    flush_text(&mut blocks, &mut acc_text);
    flush_reasoning(&mut blocks, &mut acc_reasoning);

    (
        blocks,
        has_tool_call,
        candidate.finish_reason,
        candidate.safety_ratings,
        candidate.citation_metadata,
    )
}

/// Attach Gemini-specific metadata to the FIRST canonical block in the
/// response, under `gemini.*` extension keys. Per ADR-0002, these stay
/// off the frozen-core surface and travel via extensions.
fn attach_extensions(
    mut blocks: Vec<ContentBlock>,
    safety_ratings: Vec<super::wire::SafetyRating>,
    citation_metadata: Option<serde_json::Value>,
) -> Vec<ContentBlock> {
    if blocks.is_empty() {
        return blocks;
    }
    if !safety_ratings.is_empty() {
        let value = serde_json::to_value(&safety_ratings).unwrap_or(serde_json::Value::Null);
        write_extension(&mut blocks[0], "gemini.safety_ratings", value);
    }
    if let Some(citations) = citation_metadata {
        write_extension(&mut blocks[0], "gemini.citation_metadata", citations);
    }
    blocks
}

fn write_extension(block: &mut ContentBlock, key: &str, value: serde_json::Value) {
    if let Some(ext) = block_extensions_mut(block) {
        ext.insert(key, value);
    }
    // Unsupported blocks have no extension surface; the metadata is
    // dropped silently. We never construct Unsupported in this module so
    // this branch is unreachable in practice.
}

fn block_extensions_mut(block: &mut ContentBlock) -> Option<&mut ExtensionMap> {
    match block {
        ContentBlock::Text(b) => Some(&mut b.extensions),
        ContentBlock::Image(b) => Some(&mut b.extensions),
        ContentBlock::Audio(b) => Some(&mut b.extensions),
        ContentBlock::File(b) => Some(&mut b.extensions),
        ContentBlock::ToolCall(b) => Some(&mut b.extensions),
        ContentBlock::ToolResult(b) => Some(&mut b.extensions),
        ContentBlock::Reasoning(b) => Some(&mut b.extensions),
        ContentBlock::RedactedReasoning(b) => Some(&mut b.extensions),
        ContentBlock::Unsupported(_) => None,
    }
}

fn map_finish_reason(reason: Option<&str>, has_tool_call: bool) -> StopReason {
    match (reason, has_tool_call) {
        (_, true) => StopReason::ToolUse, // tool calls always upgrade to ToolUse
        (Some("STOP"), false) => StopReason::EndTurn,
        (Some("MAX_TOKENS"), _) => StopReason::MaxTokens,
        (Some("SAFETY"), _) => StopReason::ContentFilter,
        (Some("RECITATION"), _) => StopReason::ContentFilter,
        (Some(other), _) => StopReason::Other(other.to_string()),
        (None, _) => StopReason::EndTurn,
    }
}

fn usage_metadata_to_usage(meta: UsageMetadata) -> Usage {
    Usage {
        input_tokens: meta.prompt_token_count.map(|n| n.max(0) as u32),
        output_tokens: meta.candidates_token_count.map(|n| n.max(0) as u32),
        reasoning_tokens: meta.thoughts_token_count.map(|n| n.max(0) as u32),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        estimated: false,
        provider_raw: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gemini::wire::{
        Content, FileData, FunctionCall, GenerateContentResponse, InlineData, Part, SafetyRating,
        UsageMetadata,
    };
    use serde_json::json;

    fn part_text(text: &str) -> Part {
        Part {
            text: Some(text.to_string()),
            ..Default::default()
        }
    }
    fn part_thought(text: &str) -> Part {
        Part {
            text: Some(text.to_string()),
            thought: Some(true),
            ..Default::default()
        }
    }
    fn part_call(name: &str, args: serde_json::Value) -> Part {
        Part {
            function_call: Some(FunctionCall {
                name: name.to_string(),
                args: Some(args),
                id: None,
            }),
            ..Default::default()
        }
    }

    fn response_with_parts(parts: Vec<Part>, finish: Option<&str>) -> GenerateContentResponse {
        GenerateContentResponse {
            candidates: vec![Candidate {
                content: Content {
                    role: "model".into(),
                    parts,
                },
                finish_reason: finish.map(String::from),
                safety_ratings: vec![],
                index: None,
                citation_metadata: None,
            }],
            prompt_feedback: None,
            usage_metadata: None,
        }
    }

    // ===== Unary path =====================================================

    #[test]
    fn unary_text_only_response() {
        let resp = response_with_parts(vec![part_text("Hello")], Some("STOP"));
        let canonical = parse_unary(resp, "gemini-2.0-flash");
        assert_eq!(canonical.model, "gemini-2.0-flash");
        assert_eq!(canonical.content.len(), 1);
        match &canonical.content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "Hello"),
            other => panic!("expected Text, got {other:?}"),
        }
        assert_eq!(canonical.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn unary_coalesces_consecutive_text_parts_into_one_block() {
        let resp = response_with_parts(
            vec![part_text("Hello"), part_text(", "), part_text("world!")],
            Some("STOP"),
        );
        let canonical = parse_unary(resp, "gemini-2.0-flash");
        assert_eq!(canonical.content.len(), 1);
        match &canonical.content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "Hello, world!"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn unary_separates_reasoning_and_text_blocks() {
        let resp = response_with_parts(
            vec![part_thought("thinking..."), part_text("the answer is 42")],
            Some("STOP"),
        );
        let canonical = parse_unary(resp, "gemini-2.0-flash");
        assert_eq!(canonical.content.len(), 2);
        match &canonical.content[0] {
            ContentBlock::Reasoning(r) => assert_eq!(r.text, "thinking..."),
            other => panic!("expected Reasoning, got {other:?}"),
        }
        match &canonical.content[1] {
            ContentBlock::Text(t) => assert_eq!(t.text, "the answer is 42"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn unary_function_call_yields_tool_call_block_and_tool_use_stop() {
        let resp = response_with_parts(
            vec![part_call("get_weather", json!({"city": "Paris"}))],
            Some("STOP"), // STOP gets upgraded to ToolUse because of the call
        );
        let canonical = parse_unary(resp, "gemini-2.0-flash");
        assert_eq!(canonical.content.len(), 1);
        match &canonical.content[0] {
            ContentBlock::ToolCall(tc) => {
                assert_eq!(tc.name, "get_weather");
                match &tc.arguments {
                    ToolCallArguments::Complete { value } => {
                        assert_eq!(value, &json!({"city": "Paris"}));
                    }
                    other => panic!("expected Complete args, got {other:?}"),
                }
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        assert_eq!(canonical.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn unary_inline_image_decodes_base64_into_canonical_block() {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let resp = response_with_parts(
            vec![Part {
                inline_data: Some(InlineData {
                    mime_type: "image/png".into(),
                    data: STANDARD.encode(b"\x89PNG\r\n"),
                }),
                ..Default::default()
            }],
            Some("STOP"),
        );
        let canonical = parse_unary(resp, "gemini-2.0-flash");
        match &canonical.content[0] {
            ContentBlock::Image(img) => match &img.source {
                BinarySource::Base64 { media_type, data } => {
                    assert_eq!(media_type, "image/png");
                    assert_eq!(data.as_ref(), b"\x89PNG\r\n");
                }
                other => panic!("expected Base64 source, got {other:?}"),
            },
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn unary_url_image_passes_through_as_url_source() {
        let resp = response_with_parts(
            vec![Part {
                file_data: Some(FileData {
                    mime_type: "image/png".into(),
                    file_uri: "https://example.com/cat.png".into(),
                }),
                ..Default::default()
            }],
            Some("STOP"),
        );
        let canonical = parse_unary(resp, "gemini-2.0-flash");
        match &canonical.content[0] {
            ContentBlock::Image(img) => match &img.source {
                BinarySource::Url { url } => {
                    assert_eq!(url, "https://example.com/cat.png");
                }
                other => panic!("expected Url source, got {other:?}"),
            },
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn unary_max_tokens_finish_maps_to_max_tokens_stop() {
        let resp = response_with_parts(vec![part_text("partial")], Some("MAX_TOKENS"));
        let canonical = parse_unary(resp, "gemini-2.0-flash");
        assert_eq!(canonical.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn unary_safety_finish_maps_to_content_filter() {
        let resp = response_with_parts(vec![], Some("SAFETY"));
        let canonical = parse_unary(resp, "gemini-2.0-flash");
        assert_eq!(canonical.stop_reason, StopReason::ContentFilter);
    }

    #[test]
    fn unary_unknown_finish_maps_to_other() {
        let resp = response_with_parts(vec![part_text("hi")], Some("WEIRD"));
        let canonical = parse_unary(resp, "gemini-2.0-flash");
        assert_eq!(canonical.stop_reason, StopReason::Other("WEIRD".into()));
    }

    #[test]
    fn unary_no_candidates_yields_empty_blocks_and_default_stop() {
        let resp = GenerateContentResponse {
            candidates: vec![],
            prompt_feedback: None,
            usage_metadata: None,
        };
        let canonical = parse_unary(resp, "gemini-2.0-flash");
        assert!(canonical.content.is_empty());
        assert_eq!(canonical.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn unary_usage_metadata_translates_to_canonical_usage() {
        let resp = GenerateContentResponse {
            candidates: vec![Candidate {
                content: Content {
                    role: "model".into(),
                    parts: vec![part_text("hi")],
                },
                finish_reason: Some("STOP".into()),
                safety_ratings: vec![],
                index: None,
                citation_metadata: None,
            }],
            prompt_feedback: None,
            usage_metadata: Some(UsageMetadata {
                prompt_token_count: Some(10),
                candidates_token_count: Some(5),
                thoughts_token_count: Some(3),
                total_token_count: Some(18),
            }),
        };
        let canonical = parse_unary(resp, "gemini-2.0-flash");
        let usage = canonical.usage.expect("usage present");
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(usage.reasoning_tokens, Some(3));
    }

    #[test]
    fn unary_safety_ratings_attach_to_first_block_extensions() {
        let resp = GenerateContentResponse {
            candidates: vec![Candidate {
                content: Content {
                    role: "model".into(),
                    parts: vec![part_text("hi")],
                },
                finish_reason: Some("STOP".into()),
                safety_ratings: vec![SafetyRating {
                    category: "HARM_CATEGORY_HARASSMENT".into(),
                    probability: "NEGLIGIBLE".into(),
                    blocked: None,
                }],
                index: None,
                citation_metadata: None,
            }],
            prompt_feedback: None,
            usage_metadata: None,
        };
        let canonical = parse_unary(resp, "gemini-2.0-flash");
        let block = &canonical.content[0];
        let ext = match block {
            ContentBlock::Text(b) => &b.extensions,
            _ => panic!("expected Text"),
        };
        let ratings = ext
            .get("gemini.safety_ratings")
            .expect("gemini.safety_ratings present");
        assert_eq!(
            ratings,
            &json!([{
                "category":"HARM_CATEGORY_HARASSMENT",
                "probability":"NEGLIGIBLE",
                "blocked": null
            }])
        );
    }

    // ===== Streaming path =================================================

    /// Helper: drive `parse_streaming` synchronously by collecting every
    /// emitted item from the resulting CanonicalStream.
    async fn collect_streaming(
        responses: Vec<GenerateContentResponse>,
    ) -> Vec<Result<StreamEvent, StreamError>> {
        let stream: ResponseStream = Box::pin(futures::stream::iter(
            responses.into_iter().map(Ok::<_, StreamError>),
        ));
        let canonical = parse_streaming(stream, "gemini-2.0-flash".to_string());
        canonical.collect::<Vec<_>>().await
    }

    #[tokio::test]
    async fn streaming_emits_response_start_then_message_start_then_text_deltas_then_stop() {
        let chunks = vec![
            response_with_parts(vec![part_text("Hello")], None),
            response_with_parts(vec![part_text(", world!")], Some("STOP")),
        ];
        let events = collect_streaming(chunks).await;

        // Must contain at minimum: ResponseStart, MessageStart,
        // ContentBlockStart{Text}, TextDelta, TextDelta, ContentBlockStop,
        // MessageStop, ResponseStop.
        let kinds: Vec<&str> = events
            .iter()
            .map(|r| match r.as_ref().expect("ok event") {
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
                "TextDelta",
                "ContentBlockStop",
                "MessageStop",
                "ResponseStop"
            ]
        );
    }

    #[tokio::test]
    async fn streaming_separates_reasoning_then_text_into_two_blocks() {
        let chunks = vec![
            response_with_parts(vec![part_thought("hmm")], None),
            response_with_parts(vec![part_text("answer")], Some("STOP")),
        ];
        let events = collect_streaming(chunks).await;
        // Indices: reasoning=0, text=1.
        let mut saw_reasoning = false;
        let mut saw_text = false;
        for ev in &events {
            match ev.as_ref().unwrap() {
                StreamEvent::ContentBlockStart {
                    index,
                    kind: ContentBlockKind::Reasoning,
                } => {
                    assert_eq!(*index, 0);
                    saw_reasoning = true;
                }
                StreamEvent::ContentBlockStart {
                    index,
                    kind: ContentBlockKind::Text,
                } => {
                    assert_eq!(*index, 1);
                    saw_text = true;
                }
                _ => {}
            }
        }
        assert!(saw_reasoning && saw_text);
    }

    #[tokio::test]
    async fn streaming_function_call_emits_full_tool_block_and_message_stop_is_tool_use() {
        let chunks = vec![response_with_parts(
            vec![part_call("get_weather", json!({"city": "Paris"}))],
            Some("STOP"),
        )];
        let events = collect_streaming(chunks).await;

        let mut saw_tool_start = false;
        let mut saw_args_delta = false;
        let mut saw_tool_stop = false;
        let mut stop_reason = None;
        for ev in &events {
            match ev.as_ref().unwrap() {
                StreamEvent::ToolCallStart { name, .. } => {
                    assert_eq!(name, "get_weather");
                    saw_tool_start = true;
                }
                StreamEvent::ToolCallArgumentsDelta { json_fragment, .. } => {
                    assert_eq!(json_fragment, r#"{"city":"Paris"}"#);
                    saw_args_delta = true;
                }
                StreamEvent::ToolCallStop { .. } => saw_tool_stop = true,
                StreamEvent::MessageStop { stop_reason: r, .. } => stop_reason = Some(r.clone()),
                _ => {}
            }
        }
        assert!(saw_tool_start && saw_args_delta && saw_tool_stop);
        assert_eq!(stop_reason, Some(StopReason::ToolUse));
    }

    #[tokio::test]
    async fn streaming_finalize_emits_stop_when_upstream_closes_without_finish_reason() {
        // No finish_reason on either chunk — the chain's drain must still
        // emit a MessageStop + ResponseStop so the encoder can close.
        let chunks = vec![response_with_parts(vec![part_text("Hi")], None)];
        let events = collect_streaming(chunks).await;
        let last_two = &events[events.len() - 2..];
        assert!(matches!(
            last_two[0].as_ref().unwrap(),
            StreamEvent::MessageStop { .. }
        ));
        assert!(matches!(
            last_two[1].as_ref().unwrap(),
            StreamEvent::ResponseStop { .. }
        ));
    }

    #[tokio::test]
    async fn streaming_usage_metadata_attaches_to_response_stop() {
        let chunks = vec![GenerateContentResponse {
            candidates: vec![Candidate {
                content: Content {
                    role: "model".into(),
                    parts: vec![part_text("ok")],
                },
                finish_reason: Some("STOP".into()),
                safety_ratings: vec![],
                index: None,
                citation_metadata: None,
            }],
            prompt_feedback: None,
            usage_metadata: Some(UsageMetadata {
                prompt_token_count: Some(7),
                candidates_token_count: Some(3),
                thoughts_token_count: None,
                total_token_count: Some(10),
            }),
        }];
        let events = collect_streaming(chunks).await;
        let last = events.last().unwrap().as_ref().unwrap();
        match last {
            StreamEvent::ResponseStop { usage: Some(u) } => {
                assert_eq!(u.input_tokens, Some(7));
                assert_eq!(u.output_tokens, Some(3));
            }
            other => panic!("expected ResponseStop with usage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn streaming_no_double_stop_when_post_finish_chunk_arrives() {
        // Some servers send a usage-only chunk after the finishReason chunk.
        // Our state must suppress further events once stopped.
        let chunks = vec![
            response_with_parts(vec![part_text("done")], Some("STOP")),
            // This trailing chunk should be silently ignored.
            response_with_parts(vec![part_text("ghost")], None),
        ];
        let events = collect_streaming(chunks).await;
        let stop_count = events
            .iter()
            .filter(|e| matches!(e.as_ref().unwrap(), StreamEvent::ResponseStop { .. }))
            .count();
        assert_eq!(stop_count, 1, "must emit exactly one ResponseStop");
    }

    #[tokio::test]
    async fn streaming_propagates_upstream_error_as_stream_error() {
        // Simulate the byte-stream layer surfacing an error item.
        let stream: ResponseStream = Box::pin(futures::stream::iter(vec![Err(
            StreamError::Upstream("upstream RST".into()),
        )]));
        let events = parse_streaming(stream, "gemini-2.0-flash".to_string())
            .collect::<Vec<_>>()
            .await;
        // The first event is the upstream error — no ResponseStart, no
        // drain (we never started). Both the original error and an empty
        // tail are acceptable; just confirm at least one error is present
        // and no Response*Start is emitted.
        assert!(events
            .iter()
            .any(|e| matches!(e, Err(StreamError::Upstream(_)))));
        assert!(!events
            .iter()
            .any(|e| matches!(e, Ok(StreamEvent::ResponseStart { .. }))));
    }
}
