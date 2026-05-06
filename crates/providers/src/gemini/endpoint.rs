//! URL builders for the Gemini Generate Content API
//! (`generativelanguage.googleapis.com/v1beta`).
//!
//! Two endpoints are used:
//!
//! - **Streaming**: `{base_url}/models/{model}:streamGenerateContent`
//! - **Unary**:     `{base_url}/models/{model}:generateContent`
//!
//! ## D5 — JSON-array streaming, no `?alt=sse`
//!
//! AI Studio supports SSE on the streaming endpoint via `?alt=sse`, but Plan
//! 03 D5 deliberately keeps the default form (newline-delimited JSON-array
//! framing). Adding `?alt=sse` here would change the wire format and break
//! T5's parser. If a future ADR opts into SSE, the choice belongs on
//! `BackendTarget` / `RoutePolicy`, not silently in this URL helper.
//!
//! Trailing slashes on the base URL are tolerated — operators copy-pasting
//! `https://generativelanguage.googleapis.com/v1beta/` shouldn't get
//! `//models/...` in their request URLs.

/// Build the streaming endpoint URL.
///
/// Returns `{base_url}/models/{model}:streamGenerateContent`.
pub(crate) fn streaming_url(base_url: &str, model: &str) -> String {
    let base = base_url.trim_end_matches('/');
    format!("{base}/models/{model}:streamGenerateContent")
}

/// Build the unary endpoint URL.
///
/// Returns `{base_url}/models/{model}:generateContent`.
pub(crate) fn unary_url(base_url: &str, model: &str) -> String {
    let base = base_url.trim_end_matches('/');
    format!("{base}/models/{model}:generateContent")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

    #[test]
    fn streaming_url_format() {
        assert_eq!(
            streaming_url(BASE, "gemini-2.0-flash"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:streamGenerateContent"
        );
    }

    #[test]
    fn unary_url_format() {
        assert_eq!(
            unary_url(BASE, "gemini-2.0-flash"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent"
        );
    }

    #[test]
    fn streaming_url_trims_trailing_slash_from_base() {
        assert_eq!(
            streaming_url("https://x.example.com/v1beta/", "gemini-pro"),
            "https://x.example.com/v1beta/models/gemini-pro:streamGenerateContent"
        );
    }

    #[test]
    fn unary_url_trims_trailing_slash_from_base() {
        assert_eq!(
            unary_url("https://x.example.com/v1beta/", "gemini-pro"),
            "https://x.example.com/v1beta/models/gemini-pro:generateContent"
        );
    }

    #[test]
    fn streaming_url_does_not_append_alt_sse() {
        // D5 lock-in: the default streaming wire format is JSON-array framing,
        // not SSE. If this assertion ever flips, T5's streaming parser needs
        // to be reworked first.
        let url = streaming_url(BASE, "gemini-2.0-flash");
        assert!(!url.contains("alt=sse"), "URL must not opt into SSE: {url}");
        assert!(!url.contains('?'), "URL must not carry query params: {url}");
    }

    #[test]
    fn model_name_passes_through_untouched() {
        // We don't URL-encode the model name — Gemini model identifiers use
        // simple `[a-z0-9-]` characters. If a future model name needs
        // encoding, this test will catch the regression by failing.
        let url = unary_url(BASE, "gemini-1.5-pro-002");
        assert!(url.ends_with("/models/gemini-1.5-pro-002:generateContent"));
    }
}
