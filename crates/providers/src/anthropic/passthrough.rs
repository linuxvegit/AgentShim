//! Passthrough path: forward raw inbound bytes (Anthropic Messages shape)
//! to api.anthropic.com unchanged. Rewrites the top-level `model` field on
//! the body to `target.model` so route-level upstream-model overrides apply.

use bytes::Bytes;
use reqwest::header::CONTENT_TYPE;
use tracing::warn;

use agent_shim_core::BackendTarget;

use super::AnthropicProvider;
use crate::{BackendProvider, ProviderError, RawByteStream};

pub(crate) async fn send(
    provider: &AnthropicProvider,
    body: Bytes,
    target: BackendTarget,
) -> Result<(String, RawByteStream), ProviderError> {
    let body = rewrite_model(body, &target.model)?;

    let mut request_builder = provider
        .client
        .post(provider.messages_url())
        .header("x-api-key", provider.api_key.as_str())
        .header("anthropic-version", provider.anthropic_version.as_str())
        .header(CONTENT_TYPE, "application/json")
        .body(body);

    // Configured default headers (e.g. an organization-level operator override).
    for (k, v) in &provider.default_headers {
        request_builder = request_builder.header(k, v);
    }

    // Per-route default `anthropic-beta` if configured. proxy_raw runs before
    // decode, so per-request inbound headers aren't merged here — the inbound
    // request's anthropic-beta will be replayed on the wire by the original
    // bytes (since this is a byte passthrough). This default only kicks in
    // when the inbound request didn't supply one.
    if let Some(beta) = &target.policy.default_anthropic_beta {
        request_builder = request_builder.header("anthropic-beta", beta.as_str());
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
            provider = provider.name(),
            status = status.as_u16(),
            body = %body_text,
            "anthropic upstream returned error"
        );
        return Err(ProviderError::Upstream {
            status: status.as_u16(),
            body: body_text,
        });
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/event-stream")
        .to_string();

    Ok((content_type, Box::pin(response.bytes_stream())))
}

/// Replace the top-level `model` field in the JSON request body with the
/// route's upstream model name.
///
/// Two paths:
///
/// 1. **Fast path** (the overwhelmingly common case): scan the bytes
///    in-place to locate the top-level `"model": "..."` field's value span,
///    then splice in `head ++ new_value ++ tail`. Allocates exactly one
///    `Vec<u8>` of size `body.len() - old_value_len + new_value_len`.
///    Avoids the full `from_slice` -> mutate `Value` tree -> `to_vec`
///    round-trip that the previous implementation paid on every passthrough
///    request (~30 KB scanned + ~50 KB tree alloc + ~30 KB re-serialised
///    for a typical body, all to swap one string).
///
/// 2. **Slow path** (anything weird the fast scanner can't confidently
///    handle: `model` missing at top level, body isn't a JSON object,
///    upstream model needs non-trivial JSON escaping, malformed JSON):
///    fall back to the original full `serde_json::Value` round-trip. This
///    keeps the correctness floor exactly at what the round-trip path
///    already guaranteed.
fn rewrite_model(body: Bytes, upstream_model: &str) -> Result<Bytes, ProviderError> {
    // Fast path: scan for the top-level "model" field span and splice.
    if needs_no_json_escape(upstream_model) {
        if let Some(span) = find_top_level_string_value_span(&body, "model") {
            let mut out =
                Vec::with_capacity(body.len() - (span.end - span.start) + upstream_model.len() + 2);
            out.extend_from_slice(&body[..span.start]);
            out.push(b'"');
            out.extend_from_slice(upstream_model.as_bytes());
            out.push(b'"');
            out.extend_from_slice(&body[span.end..]);
            return Ok(Bytes::from(out));
        }
    }

    // Slow path: original round-trip. Covers (a) model field absent at
    // top level, (b) upstream_model contains characters that need JSON
    // escaping, (c) malformed JSON (the from_slice error surfaces here).
    let mut value: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| ProviderError::Decode(e.to_string()))?;
    let Some(object) = value.as_object_mut() else {
        return Err(ProviderError::Decode(
            "Anthropic Messages request body must be a JSON object".to_string(),
        ));
    };
    object.insert(
        "model".to_string(),
        serde_json::Value::String(upstream_model.to_string()),
    );
    serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|e| ProviderError::Encode(e.to_string()))
}

/// True if `s` can be emitted between JSON `"` quotes without escaping any
/// character. Conservative -- only accepts the printable ASCII subset that
/// is safe under all JSON profiles. Model names in production traffic
/// (`gpt-4o`, `claude-opus-4-7`, `gemini-2.0-flash-exp`) all qualify.
fn needs_no_json_escape(s: &str) -> bool {
    s.bytes().all(|b| {
        // Printable ASCII excluding `"` (0x22) and `\` (0x5C). U+0020..=U+007E.
        (0x20..=0x7E).contains(&b) && b != b'"' && b != b'\\'
    })
}

#[derive(Debug, PartialEq, Eq)]
struct Span {
    /// Byte offset of the opening `"` of the string value.
    start: usize,
    /// Byte offset one past the closing `"` of the string value.
    end: usize,
}

/// Locate the value span of a top-level string field in a JSON object body.
///
/// Returns `Some(span)` only when:
///   - The body parses as a JSON object at the top level.
///   - The named field exists at the top level (not nested).
///   - Its value is a string.
///
/// Returns `None` for anything else, including malformed JSON, the field
/// being absent, the value being non-string (number, object, array, null,
/// bool), the field appearing only inside a nested object/array, or any
/// shape the scanner can't confidently handle. Callers fall back to the
/// full `serde_json` round-trip in those cases.
///
/// Why a hand-rolled scanner instead of `serde_json::RawValue`: `RawValue`
/// still needs `Map<String, &RawValue>` allocation for the whole top
/// level, which is most of what we're trying to avoid.
fn find_top_level_string_value_span(body: &[u8], field: &str) -> Option<Span> {
    let mut i = skip_whitespace(body, 0);
    if body.get(i)? != &b'{' {
        return None;
    }
    i += 1;

    loop {
        i = skip_whitespace(body, i);
        match body.get(i)? {
            b'}' => return None, // End of object, field not found.
            b'"' => {}
            _ => return None, // Malformed.
        }

        // Read the key (raw, no escape handling -- only matched against
        // ASCII identifiers like "model").
        let key_start = i + 1;
        let key_end = scan_string_end(body, key_start)?;
        let key = body.get(key_start..key_end)?;
        i = key_end + 1; // past the closing "

        i = skip_whitespace(body, i);
        if body.get(i)? != &b':' {
            return None;
        }
        i += 1;
        i = skip_whitespace(body, i);

        let value_start = i;
        let value_end = scan_value_end(body, value_start)?;

        if key == field.as_bytes() {
            // Confirm it's a string value (starts with `"`).
            if body.get(value_start) == Some(&b'"') {
                return Some(Span {
                    start: value_start,
                    end: value_end,
                });
            }
            return None;
        }

        i = value_end;
        i = skip_whitespace(body, i);
        match body.get(i)? {
            b',' => {
                i += 1;
            }
            b'}' => return None, // Field not found before end of object.
            _ => return None,    // Malformed.
        }
    }
}

fn skip_whitespace(body: &[u8], mut i: usize) -> usize {
    while let Some(&b) = body.get(i) {
        if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
            i += 1;
        } else {
            break;
        }
    }
    i
}

/// Given `i` pointing at the byte *after* an opening `"`, returns the
/// offset of the closing `"`. Handles `\"` and `\\` escape sequences.
/// Returns `None` if the string is unterminated.
fn scan_string_end(body: &[u8], mut i: usize) -> Option<usize> {
    while let Some(&b) = body.get(i) {
        match b {
            b'\\' => {
                // Skip the escape byte and whatever follows.
                i += 2;
            }
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Given `i` pointing at the first byte of a JSON value, returns the
/// offset one past the value's last byte. Handles strings (with escapes),
/// objects, arrays, numbers, booleans, null. Returns `None` if the value
/// shape is unrecognised or unterminated.
fn scan_value_end(body: &[u8], i: usize) -> Option<usize> {
    let &first = body.get(i)?;
    match first {
        b'"' => {
            let end = scan_string_end(body, i + 1)?;
            Some(end + 1)
        }
        b'{' => scan_balanced(body, i, b'{', b'}'),
        b'[' => scan_balanced(body, i, b'[', b']'),
        b't' | b'f' | b'n' => {
            // true / false / null
            let len = match first {
                b't' | b'n' => 4,
                b'f' => 5,
                _ => return None,
            };
            Some(i + len)
        }
        b'-' | b'0'..=b'9' => {
            let mut j = i;
            while let Some(&b) = body.get(j) {
                if matches!(b, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
                    j += 1;
                } else {
                    break;
                }
            }
            Some(j)
        }
        _ => None,
    }
}

/// Walks past a balanced container (object or array), starting at the
/// opening bracket. Respects nested strings (their contents must not
/// confuse the counter). Returns the offset one past the closing bracket.
fn scan_balanced(body: &[u8], start: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth: u32 = 0;
    let mut i = start;
    while let Some(&b) = body.get(i) {
        match b {
            x if x == open => {
                depth += 1;
                i += 1;
            }
            x if x == close => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            b'"' => {
                let end = scan_string_end(body, i + 1)?;
                i = end + 1;
            }
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_model_replaces_top_level_field() {
        let body = Bytes::from(r#"{"model":"claude-3-5-sonnet-20241022","messages":[]}"#);
        let rewritten = rewrite_model(body, "claude-opus-4-7").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(v["model"], "claude-opus-4-7");
    }

    #[test]
    fn rewrite_model_preserves_other_fields() {
        let body = Bytes::from(
            r#"{"model":"claude-3-5-sonnet-20241022","messages":[{"role":"user","content":"hi"}],"max_tokens":1024}"#,
        );
        let rewritten = rewrite_model(body, "claude-opus-4-7").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(v["model"], "claude-opus-4-7");
        assert_eq!(v["max_tokens"], 1024);
        assert_eq!(v["messages"][0]["role"], "user");
    }

    #[test]
    fn rewrite_model_rejects_non_object_body() {
        let body = Bytes::from(r#"["not","an","object"]"#);
        assert!(rewrite_model(body, "claude-opus-4-7").is_err());
    }

    /// The fast-path scanner must locate `model` even when it isn't first.
    #[test]
    fn rewrite_model_handles_model_not_first() {
        let body = Bytes::from(
            r#"{"max_tokens":1024,"model":"claude-3-5-sonnet-20241022","messages":[]}"#,
        );
        let rewritten = rewrite_model(body, "claude-opus-4-7").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(v["model"], "claude-opus-4-7");
        assert_eq!(v["max_tokens"], 1024);
    }

    /// `model` inside a nested object (e.g. a metadata blob that happens to
    /// contain the word) must NOT be rewritten. Only the top-level field
    /// is the route's model identifier.
    #[test]
    fn rewrite_model_does_not_touch_nested_model_field() {
        let body = Bytes::from(
            r#"{"model":"claude-3-5-sonnet-20241022","metadata":{"model":"nested-should-not-change"}}"#,
        );
        let rewritten = rewrite_model(body, "claude-opus-4-7").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(v["model"], "claude-opus-4-7");
        assert_eq!(v["metadata"]["model"], "nested-should-not-change");
    }

    /// Strings containing the literal `model` byte sequence (e.g. a system
    /// prompt that mentions the word) must not confuse the scanner.
    #[test]
    fn rewrite_model_ignores_model_substring_inside_other_string() {
        let body =
            Bytes::from(r#"{"system":"you are a model","model":"claude-3-5-sonnet-20241022"}"#);
        let rewritten = rewrite_model(body, "claude-opus-4-7").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(v["model"], "claude-opus-4-7");
        assert_eq!(v["system"], "you are a model");
    }

    /// Whitespace between tokens (`"model" : "..."`) must be tolerated --
    /// JSON allows it and some clients emit it.
    #[test]
    fn rewrite_model_handles_whitespace_around_colon() {
        let body = Bytes::from(r#"{  "model"  :  "old"  ,  "max_tokens": 1024 }"#);
        let rewritten = rewrite_model(body, "new").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(v["model"], "new");
        assert_eq!(v["max_tokens"], 1024);
    }

    /// Escaped quotes inside other string values must not confuse the
    /// scanner.
    #[test]
    fn rewrite_model_handles_escaped_quotes_in_other_strings() {
        let body = Bytes::from(r#"{"system":"He said \"hi\".","model":"old"}"#);
        let rewritten = rewrite_model(body, "new").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(v["model"], "new");
        assert_eq!(v["system"], r#"He said "hi"."#);
    }

    /// When `model` is absent at top level the slow path must insert it
    /// (preserving the legacy behaviour of `object.insert`).
    #[test]
    fn rewrite_model_inserts_when_field_absent() {
        let body = Bytes::from(r#"{"messages":[]}"#);
        let rewritten = rewrite_model(body, "new-model").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(v["model"], "new-model");
    }

    /// When the upstream model contains characters that need JSON
    /// escaping the fast path must defer to the slow path so the escape
    /// is applied correctly.
    #[test]
    fn rewrite_model_escapes_special_chars_via_slow_path() {
        let body = Bytes::from(r#"{"model":"old"}"#);
        // A backslash forces the slow path.
        let rewritten = rewrite_model(body, "weird\\name").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(v["model"], "weird\\name");
    }

    #[test]
    fn needs_no_json_escape_recognises_common_model_names() {
        assert!(needs_no_json_escape("claude-3-5-sonnet-20241022"));
        assert!(needs_no_json_escape("gpt-4o"));
        assert!(needs_no_json_escape("gemini-2.0-flash-exp"));
        assert!(!needs_no_json_escape("contains\"quote"));
        assert!(!needs_no_json_escape("contains\\backslash"));
        assert!(!needs_no_json_escape("contains\nnewline"));
    }
}
