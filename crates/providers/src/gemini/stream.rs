//! Byte-level scanner that splits a Gemini `:streamGenerateContent` response
//! body — a single JSON array of `GenerateContentResponse` objects — into
//! per-object stream items.
//!
//! Plan 03 T5. Gemini AI Studio's streaming endpoint returns a *single* JSON
//! array, not SSE: the body is `[\n  { ... },\n  { ... }\n]\n` and the server
//! flushes each completed object as the model produces it. Clients are
//! expected to incrementally parse the array as bytes arrive.
//!
//! `eventsource-stream` (used by every other parser in this crate) doesn't
//! apply here, so this module ships a small custom state machine. The output
//! is a stream of fully-deserialized [`GenerateContentResponse`] values; the
//! wire→canonical translation is T6's job.
//!
//! ## Design
//!
//! A single `Scanner` instance keeps a small byte buffer plus four flags:
//!
//! - `started`: have we seen the leading `[` yet? (Skip leading whitespace.)
//! - `finished`: have we seen the trailing `]`? (Ignore everything after.)
//! - `depth`: current `{`/`}` nesting depth (0 = not in an object).
//! - `in_string` / `escape_pending`: track string-literal state so the `]`
//!   inside `"foo]bar"` doesn't terminate the array, and so the `}` inside
//!   `"foo}bar"` doesn't decrement `depth`.
//!
//! When `depth` transitions from 1 → 0 on a `}` outside a string, the buffered
//! bytes (from the matching `{` to the closing `}`, inclusive) are a complete
//! JSON object. We deserialize into [`GenerateContentResponse`] and yield it,
//! then clear the buffer.
//!
//! ## Robustness
//!
//! The scanner makes no assumption about TCP framing — it processes one byte
//! at a time and advances state per-byte. A chunk split anywhere (mid-object,
//! mid-string, mid-escape, between two objects, immediately before/after `[`
//! or `]`) is handled identically to a chunk split at an object boundary.
//!
//! UTF-8 multi-byte sequences pass through as opaque bytes: the JSON grammar's
//! structural characters (`{`, `}`, `[`, `]`, `,`, `"`, `\`, whitespace) are
//! all 7-bit ASCII, and string contents are forwarded verbatim into the
//! per-object byte buffer. `serde_json::from_slice` then validates UTF-8 when
//! the buffer is decoded.
//!
//! ## End-of-stream behavior
//!
//! - If the upstream closes after the trailing `]` (or in the trailing
//!   whitespace after it), `finalize()` is a no-op.
//! - If the upstream closes mid-object (`depth > 0`), `finalize()` returns a
//!   [`StreamError::Decode`] — the array is malformed.
//! - If the upstream closes after `[` and zero or more objects but before
//!   the trailing `]`, we treat that as a graceful EOF (it's the most common
//!   shape when the server simply stops sending and closes the TCP socket;
//!   raising an error here would mean every short stream gets reported as a
//!   decode failure). A future ADR may tighten this if a real upstream is
//!   observed sending `]` reliably enough to make the missing-`]` case worth
//!   flagging.
//!
//! ## Error propagation
//!
//! Errors from the underlying byte stream (`reqwest::Error`) are mapped to
//! [`StreamError::Upstream`]. Errors from `serde_json::from_slice` on a fully
//! framed object are mapped to [`StreamError::Decode`] and emitted on the
//! output stream — the scanner does NOT swallow them. After emitting an
//! error, the scanner continues processing subsequent objects, since one
//! malformed candidate shouldn't kill the whole turn.

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use agent_shim_core::StreamError;
use bytes::Bytes;
use futures::StreamExt;
use futures_core::Stream;

use super::wire::GenerateContentResponse;

/// Output stream type: a pinned, send-able stream of parsed responses or
/// per-object errors. Same shape the canonical `CanonicalStream` uses for
/// uniformity with the rest of the crate's streaming code.
pub(crate) type ResponseStream =
    Pin<Box<dyn Stream<Item = Result<GenerateContentResponse, StreamError>> + Send>>;

/// Adapt a Gemini `:streamGenerateContent` byte stream into a stream of
/// fully-deserialized [`GenerateContentResponse`] items.
///
/// The input is the same shape `reqwest::Response::bytes_stream()` produces.
/// Output items correspond 1:1 to the JSON objects in the upstream array, in
/// the order the server sent them.
pub(crate) fn into_response_stream<S>(byte_stream: S) -> ResponseStream
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    // The scanner is mutated by per-chunk processing AND by the post-EOF
    // finalize step. Wrapping in `Arc<Mutex<...>>` lets both closures share
    // it without futures-friendly `unfold` gymnastics. The mutex is
    // uncontended in practice (the chain composes serially), so the
    // overhead is two atomic ops per chunk.
    let scanner = Arc::new(Mutex::new(Scanner::new()));
    let scanner_for_chunks = Arc::clone(&scanner);

    let chunked = byte_stream.flat_map(move |chunk_result| {
        let items: Vec<Result<GenerateContentResponse, StreamError>> = match chunk_result {
            Err(e) => {
                tracing::warn!(error = %e, "gemini: byte-stream error");
                vec![Err(StreamError::Upstream(e.to_string()))]
            }
            Ok(bytes) => {
                let mut s = scanner_for_chunks.lock().expect("scanner mutex poisoned");
                s.feed(&bytes)
            }
        };
        futures::stream::iter(items)
    });

    // After the upstream byte stream ends, surface any unterminated-object
    // error from the scanner. We don't emit anything for a clean EOF.
    let tail = futures::stream::once(async move {
        let s = scanner.lock().expect("scanner mutex poisoned");
        s.finalize()
    })
    .filter_map(|maybe_err| async move { maybe_err.map(Err) });

    Box::pin(chunked.chain(tail))
}

/// Internal byte-level state machine. Public-in-module so unit tests can
/// drive it directly without spinning up a real `Stream`.
struct Scanner {
    /// Per-object buffer: filled from the matching `{` up to and including
    /// the closing `}`. Cleared after each successful emit.
    buf: Vec<u8>,
    /// Have we seen the leading `[` of the array?
    started: bool,
    /// Have we seen the trailing `]`? (Bytes after this are ignored.)
    finished: bool,
    /// Current brace nesting depth. 0 means "not currently inside an object".
    depth: u32,
    /// True iff the cursor is inside a JSON string literal.
    in_string: bool,
    /// True iff the previous byte inside a string was an unescaped `\` and
    /// the current byte should be treated as escaped (and not interpreted
    /// structurally even if it's `"` or `\`).
    escape_pending: bool,
}

impl Scanner {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(2048),
            started: false,
            finished: false,
            depth: 0,
            in_string: false,
            escape_pending: false,
        }
    }

    /// Feed a chunk and collect every fully-framed object that becomes
    /// available. Per-object decode errors are yielded inline; subsequent
    /// objects continue to be parsed.
    fn feed(&mut self, chunk: &[u8]) -> Vec<Result<GenerateContentResponse, StreamError>> {
        let mut out: Vec<Result<GenerateContentResponse, StreamError>> = Vec::new();

        for &byte in chunk {
            if self.finished {
                // Bytes after `]` are server slop (Gemini sometimes appends
                // a newline). Ignore them — emitting a decode error here
                // would be hostile to a perfectly well-formed stream.
                continue;
            }

            if !self.started {
                if is_json_whitespace(byte) {
                    continue;
                }
                if byte == b'[' {
                    self.started = true;
                    continue;
                }
                // Anything other than whitespace or `[` before the array
                // begins is malformed — surface it once and bail to the
                // finished state so we don't spam errors per byte.
                self.finished = true;
                out.push(Err(StreamError::Decode(format!(
                    "expected `[` at start of Gemini stream, got byte 0x{byte:02x}"
                ))));
                continue;
            }

            // We're past the leading `[` and not yet finished. Either we're
            // in the gap between objects (depth==0) or inside an object.

            if self.depth == 0 {
                // Between objects: skip whitespace and the inter-object `,`
                // until we hit `{` (next object) or `]` (end of array).
                match byte {
                    b'{' => {
                        self.depth = 1;
                        self.buf.push(byte);
                    }
                    b']' => {
                        self.finished = true;
                    }
                    b if is_json_whitespace(b) || b == b',' => {
                        // Tolerated: array indentation, item separator.
                    }
                    other => {
                        // Stray bytes between array elements are malformed.
                        out.push(Err(StreamError::Decode(format!(
                            "unexpected byte 0x{other:02x} between Gemini stream objects"
                        ))));
                        // Don't transition to finished — a single garbage
                        // byte shouldn't kill the rest of the stream if the
                        // server recovers (defensive; not observed live).
                    }
                }
                continue;
            }

            // depth >= 1: we're inside an object. Buffer every byte and
            // update structural state.
            self.buf.push(byte);

            if self.in_string {
                if self.escape_pending {
                    // Whatever this byte is, it's part of an escape sequence
                    // (\", \\, \/, \b, \f, \n, \r, \t, or the leading byte
                    // of a \uXXXX). For \uXXXX the four hex digits that
                    // follow are not escape-significant — they're just normal
                    // string bytes — so we don't track sub-state. The only
                    // byte that matters here is whether the *next* byte
                    // should be escape-pending again, and per JSON spec a
                    // single backslash escapes exactly one following byte.
                    self.escape_pending = false;
                    continue;
                }
                match byte {
                    b'\\' => {
                        self.escape_pending = true;
                    }
                    b'"' => {
                        self.in_string = false;
                    }
                    _ => {
                        // Plain string byte — buffer-only, no state change.
                    }
                }
                continue;
            }

            // Not in a string and depth >= 1.
            match byte {
                b'"' => {
                    self.in_string = true;
                }
                b'{' => {
                    self.depth += 1;
                }
                b'}' => {
                    self.depth -= 1;
                    if self.depth == 0 {
                        // Just closed the top-level object. Decode and emit.
                        match serde_json::from_slice::<GenerateContentResponse>(&self.buf) {
                            Ok(resp) => out.push(Ok(resp)),
                            Err(e) => out.push(Err(StreamError::Decode(format!(
                                "failed to decode Gemini stream object: {e}"
                            )))),
                        }
                        self.buf.clear();
                    }
                }
                _ => {
                    // Other structural bytes (`[`, `]`, `,`, `:`) and content
                    // bytes (digits, letters, etc.) are buffered but don't
                    // change scanner state. Note: `[` / `]` inside an object
                    // are array brackets in field values; they don't affect
                    // our object-level brace counter.
                }
            }
        }

        out
    }

    /// Called once after the upstream byte stream ends. Returns a decode
    /// error if we ended mid-object; otherwise `None`.
    fn finalize(&self) -> Option<StreamError> {
        if self.depth > 0 || self.in_string {
            Some(StreamError::Decode(format!(
                "Gemini stream ended mid-object (depth={}, in_string={})",
                self.depth, self.in_string
            )))
        } else {
            None
        }
    }
}

/// JSON's whitespace set per RFC 8259 §2: space, horizontal tab, line feed,
/// carriage return. Other Unicode whitespace is NOT tolerated.
fn is_json_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    /// Helper: feed an array body into a Scanner one byte at a time and
    /// collect every emitted result. Models the worst-case TCP framing where
    /// every packet contains exactly one byte.
    fn feed_one_byte_at_a_time(body: &[u8]) -> Vec<Result<GenerateContentResponse, StreamError>> {
        let mut scanner = Scanner::new();
        let mut out = Vec::new();
        for &b in body {
            out.extend(scanner.feed(&[b]));
        }
        if let Some(err) = scanner.finalize() {
            out.push(Err(err));
        }
        out
    }

    /// Helper: feed an array body into a Scanner as a single chunk. Models
    /// the best-case framing where the entire array arrives at once.
    fn feed_whole(body: &[u8]) -> Vec<Result<GenerateContentResponse, StreamError>> {
        let mut scanner = Scanner::new();
        let mut out = scanner.feed(body);
        if let Some(err) = scanner.finalize() {
            out.push(Err(err));
        }
        out
    }

    /// A minimal but complete Gemini stream response. We use this verbatim
    /// in several tests so the equivalent of "a full turn" is reused.
    const SAMPLE_BODY: &str = r#"[
{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]}}]},
{"candidates":[{"content":{"role":"model","parts":[{"text":" world"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3}}
]"#;

    // ------------------------------------------------------------------ //
    // Happy path
    // ------------------------------------------------------------------ //

    #[test]
    fn empty_array_yields_no_objects() {
        let out = feed_whole(b"[]");
        assert!(out.is_empty(), "got unexpected items: {out:?}");
    }

    #[test]
    fn empty_array_with_whitespace_yields_no_objects() {
        let out = feed_whole(b"  [\n  \n  ]\n");
        assert!(out.is_empty(), "got unexpected items: {out:?}");
    }

    #[test]
    fn single_object_array_yields_one_response() {
        let body = br#"[{"candidates":[]}]"#;
        let out = feed_whole(body);
        assert_eq!(out.len(), 1);
        let resp = out[0].as_ref().expect("ok response");
        assert!(resp.candidates.is_empty());
    }

    #[test]
    fn two_object_array_yields_two_responses_in_order() {
        let out = feed_whole(SAMPLE_BODY.as_bytes());
        assert_eq!(out.len(), 2);
        let first = out[0].as_ref().expect("first ok");
        let second = out[1].as_ref().expect("second ok");
        assert_eq!(
            first.candidates[0].content.parts[0].text.as_deref(),
            Some("Hello")
        );
        assert_eq!(
            second.candidates[0].content.parts[0].text.as_deref(),
            Some(" world")
        );
        // The second has usage metadata.
        assert_eq!(
            second.usage_metadata.as_ref().unwrap().total_token_count,
            Some(3)
        );
    }

    // ------------------------------------------------------------------ //
    // Chunk-boundary fuzzing — the whole point of T5
    // ------------------------------------------------------------------ //

    #[test]
    fn one_byte_at_a_time_is_equivalent_to_whole_chunk() {
        let whole = feed_whole(SAMPLE_BODY.as_bytes());
        let drip = feed_one_byte_at_a_time(SAMPLE_BODY.as_bytes());
        assert_eq!(whole.len(), 2);
        assert_eq!(drip.len(), 2);

        // Both runs must succeed and produce the same text content.
        for (a, b) in whole.iter().zip(drip.iter()) {
            let a = a.as_ref().expect("ok in whole");
            let b = b.as_ref().expect("ok in drip");
            assert_eq!(
                a.candidates[0].content.parts[0].text,
                b.candidates[0].content.parts[0].text
            );
        }
    }

    #[test]
    fn split_at_every_position_yields_same_two_objects() {
        // For each possible split position, feed [0..i], then [i..], and
        // confirm we still get exactly the two expected objects. This is
        // the strongest possible chunk-boundary regression test.
        let body = SAMPLE_BODY.as_bytes();
        for i in 0..body.len() {
            let mut scanner = Scanner::new();
            let mut out = scanner.feed(&body[..i]);
            out.extend(scanner.feed(&body[i..]));
            assert!(scanner.finalize().is_none(), "finalize error at split {i}");
            assert_eq!(out.len(), 2, "wrong count at split {i}: {out:?}");
            assert!(
                out[0].is_ok() && out[1].is_ok(),
                "non-ok result at split {i}: {out:?}"
            );
        }
    }

    #[test]
    fn split_inside_string_does_not_misinterpret_brackets() {
        // The text field contains structural characters (`{`, `}`, `[`, `]`,
        // `,`, `"`) — if the scanner treats them as structural while inside
        // a string, brace counting goes haywire. Feed one byte at a time to
        // exercise every possible cut point including mid-string.
        let body = br#"[{"candidates":[{"content":{"role":"model","parts":[{"text":"weird ]}{,\"chars\""}]}}]}]"#;
        let out = feed_one_byte_at_a_time(body);
        assert_eq!(out.len(), 1, "{out:?}");
        let resp = out[0].as_ref().expect("ok");
        assert_eq!(
            resp.candidates[0].content.parts[0].text.as_deref(),
            Some(r#"weird ]}{,"chars""#)
        );
    }

    #[test]
    fn split_inside_escape_sequence_is_safe() {
        // `\"` is a two-byte escape; if the scanner's chunk boundary lands
        // exactly between `\` and `"`, the `"` must not be interpreted as a
        // string-closing quote.
        let body = br#"[{"candidates":[{"content":{"role":"model","parts":[{"text":"a\"b"}]}}]}]"#;
        // Exhaustively split at every position including the escape boundary.
        for i in 0..body.len() {
            let mut scanner = Scanner::new();
            let mut out = scanner.feed(&body[..i]);
            out.extend(scanner.feed(&body[i..]));
            assert!(scanner.finalize().is_none(), "finalize error at split {i}");
            assert_eq!(out.len(), 1, "wrong count at split {i}");
            let resp = out[0].as_ref().expect("ok at split {i}");
            assert_eq!(
                resp.candidates[0].content.parts[0].text.as_deref(),
                Some(r#"a"b"#),
                "mismatch at split {i}"
            );
        }
    }

    #[test]
    fn split_inside_unicode_escape_is_safe() {
        // A = 'A'. The four hex digits must NOT trigger any structural
        // state — a `}` digit can't appear, but the test confirms the literal
        // hex bytes pass through cleanly under any chunk split.
        let body = br#"[{"candidates":[{"content":{"role":"model","parts":[{"text":"A}"}]}}]}]"#;
        // } = '}' — if scanner is buggy, this would break brace counting.
        for i in 0..body.len() {
            let mut scanner = Scanner::new();
            let mut out = scanner.feed(&body[..i]);
            out.extend(scanner.feed(&body[i..]));
            assert!(scanner.finalize().is_none(), "finalize at split {i}");
            assert_eq!(out.len(), 1, "wrong count at split {i}");
            let resp = out[0].as_ref().expect("ok at split {i}");
            // } is an actual `}` character once decoded.
            assert_eq!(
                resp.candidates[0].content.parts[0].text.as_deref(),
                Some("A}"),
                "mismatch at split {i}"
            );
        }
    }

    #[test]
    fn nested_objects_in_function_call_args_count_braces_correctly() {
        // functionCall.args is a real nested object — depth goes 1→2→3→2→1→0.
        let body = br#"[{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"f","args":{"a":{"b":1}}}}]}}]}]"#;
        let out = feed_one_byte_at_a_time(body);
        assert_eq!(out.len(), 1, "{out:?}");
        let resp = out[0].as_ref().expect("ok");
        let fc = resp.candidates[0].content.parts[0]
            .function_call
            .as_ref()
            .expect("functionCall present");
        assert_eq!(fc.name, "f");
        assert_eq!(fc.args, Some(serde_json::json!({"a":{"b":1}})));
    }

    // ------------------------------------------------------------------ //
    // Whitespace and formatting tolerance
    // ------------------------------------------------------------------ //

    #[test]
    fn tolerates_no_whitespace_compact_array() {
        let body = br#"[{"candidates":[]},{"candidates":[]}]"#;
        let out = feed_whole(body);
        assert_eq!(out.len(), 2);
        assert!(out[0].is_ok() && out[1].is_ok());
    }

    #[test]
    fn tolerates_lots_of_whitespace_between_objects() {
        let body = b"[\n\n\t  {\"candidates\":[]}\n  ,\r\n\n  {\"candidates\":[]}  \n  ]\n";
        let out = feed_whole(body);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn ignores_trailing_newline_after_closing_bracket() {
        // Servers commonly append a trailing `\n` after `]`. Must NOT emit
        // a decode error.
        let body = b"[{\"candidates\":[]}]\n\n";
        let out = feed_whole(body);
        assert_eq!(out.len(), 1);
        assert!(out[0].is_ok());
    }

    // ------------------------------------------------------------------ //
    // Error paths
    // ------------------------------------------------------------------ //

    #[test]
    fn malformed_first_byte_yields_decode_error() {
        let out = feed_whole(b"oops");
        assert_eq!(out.len(), 1);
        let err = out[0].as_ref().expect_err("expected decode error");
        assert!(matches!(err, StreamError::Decode(_)));
    }

    #[test]
    fn truncated_mid_object_yields_decode_error_on_finalize() {
        // No closing `}` — finalize should report.
        let body = br#"[{"candidates":[{"content":{"role":"model""#;
        let out = feed_whole(body);
        assert_eq!(out.len(), 1);
        let err = out[0].as_ref().expect_err("expected decode error");
        assert!(matches!(err, StreamError::Decode(msg) if msg.contains("ended mid-object")));
    }

    #[test]
    fn truncated_mid_string_yields_decode_error_on_finalize() {
        // Object incomplete: open quote with no closing quote.
        let body = br#"[{"candidates":"unterminat"#;
        let out = feed_whole(body);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Err(StreamError::Decode(_))));
    }

    #[test]
    fn truncated_after_open_bracket_is_graceful() {
        // Lots of upstream sockets just close after sending `[` and zero
        // objects. We treat that as an empty array, NOT an error.
        let out = feed_whole(b"[");
        assert!(out.is_empty(), "expected silent EOF, got {out:?}");
    }

    #[test]
    fn truncated_between_objects_is_graceful() {
        // Server flushed one object then the connection died — we keep what
        // we successfully parsed and don't fabricate an error.
        let body = br#"[{"candidates":[]},"#;
        let out = feed_whole(body);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].is_ok());
    }

    #[test]
    fn one_bad_object_does_not_kill_the_stream() {
        // Object 1: valid. Object 2: structurally framed (matched braces) but
        // not a valid GenerateContentResponse — `candidates` should be an
        // array. Object 3: valid. The middle one yields Err but the third
        // object MUST still be emitted as Ok.
        let body = br#"[{"candidates":[]},{"candidates":"not-an-array"},{"candidates":[]}]"#;
        let out = feed_whole(body);
        assert_eq!(out.len(), 3);
        assert!(out[0].is_ok());
        assert!(matches!(&out[1], Err(StreamError::Decode(_))));
        assert!(out[2].is_ok());
    }

    // ------------------------------------------------------------------ //
    // End-to-end through the actual Stream pipeline
    // ------------------------------------------------------------------ //

    /// Build a `Stream<Item = Result<Bytes, reqwest::Error>>` from a static
    /// list of `Bytes` chunks (Ok variants only). We can't construct a real
    /// `reqwest::Error` outside the crate, so the upstream-error case is
    /// covered by inspection of the code path rather than a fake error.
    fn ok_bytes_stream(
        chunks: Vec<Vec<u8>>,
    ) -> impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static {
        stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, reqwest::Error>(Bytes::from(c))),
        )
    }

    #[tokio::test]
    async fn into_response_stream_emits_two_objects_for_sample_body() {
        let body = SAMPLE_BODY.as_bytes().to_vec();
        // Split into two chunks at a deliberately awkward position: mid-text.
        let mid = body.len() / 2;
        let chunks = vec![body[..mid].to_vec(), body[mid..].to_vec()];
        let mut stream = into_response_stream(ok_bytes_stream(chunks));

        let first = stream.next().await.expect("first item").expect("ok");
        let second = stream.next().await.expect("second item").expect("ok");
        assert!(stream.next().await.is_none());

        assert_eq!(
            first.candidates[0].content.parts[0].text.as_deref(),
            Some("Hello")
        );
        assert_eq!(
            second.candidates[0].content.parts[0].text.as_deref(),
            Some(" world")
        );
    }

    #[tokio::test]
    async fn into_response_stream_yields_finalize_error_on_truncation() {
        // Stream ends mid-object — final `flat_map` chunk is empty, then the
        // tail surfaces the finalize error.
        let chunks = vec![br#"[{"candidates":["#.to_vec()];
        let mut stream = into_response_stream(ok_bytes_stream(chunks));

        let item = stream.next().await.expect("an item");
        assert!(matches!(item, Err(StreamError::Decode(_))));
    }

    #[tokio::test]
    async fn into_response_stream_drips_one_byte_per_chunk() {
        // Strongest TCP-framing case: every byte its own chunk.
        let chunks: Vec<Vec<u8>> = SAMPLE_BODY.bytes().map(|b| vec![b]).collect();
        let mut stream = into_response_stream(ok_bytes_stream(chunks));

        let first = stream.next().await.expect("first").expect("ok");
        let second = stream.next().await.expect("second").expect("ok");
        assert!(stream.next().await.is_none());
        assert_eq!(
            first.candidates[0].content.parts[0].text.as_deref(),
            Some("Hello")
        );
        assert_eq!(
            second.candidates[0].content.parts[0].text.as_deref(),
            Some(" world")
        );
    }
}
