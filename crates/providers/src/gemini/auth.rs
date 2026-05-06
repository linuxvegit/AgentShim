//! Authentication for Google AI Studio (`generativelanguage.googleapis.com`).
//!
//! AI Studio authenticates via an API key supplied as the `?key=` query
//! parameter on every request — there is no `Authorization` header. The key
//! lives in config as `Secret<String>`; the provider is expected to call
//! `cfg.api_key.expose().to_string()` once at construction time and pass the
//! exposed key here so this module never depends on `agent-shim-config`.
//!
//! [`AiStudioAuth::apply`] augments a `reqwest::RequestBuilder` with the key
//! query parameter using `.query(&[("key", ...)])`. Letting reqwest do the
//! URL-encoding (rather than concatenating into the URL string ourselves)
//! handles keys that contain reserved URL characters correctly.
//!
//! T7's `BackendProvider` impl will use this struct; T3 introduces the type
//! and unit tests so T7 has a stable surface to depend on.

use reqwest::RequestBuilder;

/// Holder for an exposed AI Studio API key, with a single helper to attach it
/// to outgoing HTTP requests as the `?key=` query parameter.
///
/// The `api_key` is the already-exposed string value (e.g.
/// `secret.expose().to_string()` at the provider boundary). This module
/// intentionally does not import `Secret` so it can stay free of the
/// `agent-shim-config` dependency.
#[derive(Debug, Clone)]
pub(crate) struct AiStudioAuth {
    pub(crate) api_key: String,
}

impl AiStudioAuth {
    /// Attach the API key to a request builder as a `key=...` query
    /// parameter. reqwest URL-encodes the value, so keys that contain
    /// reserved URL characters (`&`, `=`, etc.) are passed through safely.
    pub(crate) fn apply(&self, builder: RequestBuilder) -> RequestBuilder {
        builder.query(&[("key", &self.api_key)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_adds_key_query_param() {
        let auth = AiStudioAuth {
            api_key: "AIza-test".into(),
        };
        let client = reqwest::Client::new();
        let req = auth
            .apply(client.get("https://example.com/v1beta/models/test:generateContent"))
            .build()
            .expect("request builds");
        let url = req.url().to_string();
        assert!(
            url.contains("key=AIza-test"),
            "expected key=AIza-test in URL, got {url}"
        );
    }

    #[test]
    fn apply_url_encodes_special_characters() {
        let auth = AiStudioAuth {
            api_key: "abc&def=ghi".into(),
        };
        let client = reqwest::Client::new();
        let req = auth
            .apply(client.get("https://example.com/test"))
            .build()
            .expect("request builds");
        let url = req.url().to_string();
        // `&` and `=` inside the value must be percent-encoded so reqwest
        // doesn't read them as query separators when the upstream parses the
        // URL. `%26` = `&`, `%3D` = `=`.
        assert!(
            url.contains("key=abc%26def%3Dghi"),
            "expected URL-encoded key in URL, got {url}"
        );
    }

    #[test]
    fn apply_preserves_pre_existing_query_params() {
        // Sanity check: a `?key=` is appended without clobbering an existing
        // query string. We don't currently use other query params on the
        // Gemini endpoints (D5: streaming uses JSON-array, not `?alt=sse`),
        // but if T5 ever adds one, the auth helper needs to compose with it.
        let auth = AiStudioAuth {
            api_key: "AIza-test".into(),
        };
        let client = reqwest::Client::new();
        let req = auth
            .apply(client.get("https://example.com/test?foo=bar"))
            .build()
            .expect("request builds");
        let url = req.url().to_string();
        assert!(url.contains("foo=bar"), "lost pre-existing query: {url}");
        assert!(url.contains("key=AIza-test"), "lost auth key: {url}");
    }
}
