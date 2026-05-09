//! Authentication header extraction (Plan 04 P04 T1).
//!
//! Plaintext API keys come in via `Authorization: Bearer <key>` or
//! `x-api-key: <key>`. The gateway hashes them with SHA-256 and looks up
//! the hash in `auth.keys.<sha256:hex>`. Plaintext is never stored or logged.

use sha2::{Digest, Sha256};

/// Identity tied to a request, used for per-key rate limiting and audit logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentIdentity {
    /// No API key was presented (or `auth.enabled = false`).
    Anonymous,
    /// `sha256:<hex>` of the presented key. Look this up in `auth.keys`.
    KeyHash(String),
}

impl AgentIdentity {
    /// String identifier used in operator logs. Plaintext keys never appear
    /// here — only the hash. `"anonymous"` for the Anonymous variant.
    pub fn log_id(&self) -> &str {
        match self {
            AgentIdentity::Anonymous => "anonymous",
            AgentIdentity::KeyHash(h) => h.as_str(),
        }
    }
}

/// Hash a plaintext key into `sha256:<hex>` form. Caller passes the bare
/// plaintext; this fn does NOT strip a `Bearer ` prefix (that's the parser's
/// job).
pub fn hash_key(plaintext: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    let bytes = hasher.finalize();
    let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    format!("sha256:{}", hex)
}

/// Extract identity from request headers. Checks `Authorization: Bearer ...`
/// first, then `x-api-key: ...`. Returns `Anonymous` if neither is present
/// or if the value is empty.
///
/// `Bearer` is matched case-insensitively per RFC 6750 §2.1 (the `Bearer`
/// auth scheme is case-insensitive). Real-world clients sometimes send
/// `bearer foo` or `BEARER foo`; we accept all forms.
pub fn extract_identity_from_headers(
    authorization: Option<&str>,
    x_api_key: Option<&str>,
) -> AgentIdentity {
    if let Some(auth) = authorization {
        if let Some((scheme, rest)) = auth.split_once(' ') {
            if scheme.eq_ignore_ascii_case("Bearer") {
                let key = rest.trim();
                if !key.is_empty() {
                    return AgentIdentity::KeyHash(hash_key(key));
                }
            }
        }
    }
    if let Some(key) = x_api_key {
        let key = key.trim();
        if !key.is_empty() {
            return AgentIdentity::KeyHash(hash_key(key));
        }
    }
    AgentIdentity::Anonymous
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_key_produces_deterministic_sha256() {
        let h = hash_key("hello");
        // SHA-256("hello") is a well-known fixture.
        assert_eq!(
            h,
            "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn extract_from_bearer_header() {
        let id = extract_identity_from_headers(Some("Bearer my-secret"), None);
        match id {
            AgentIdentity::KeyHash(h) => {
                assert!(h.starts_with("sha256:"));
                assert_eq!(h.len(), "sha256:".len() + 64);
            }
            _ => panic!("expected KeyHash"),
        }
    }

    #[test]
    fn extract_from_x_api_key() {
        let id = extract_identity_from_headers(None, Some("my-secret"));
        assert!(matches!(id, AgentIdentity::KeyHash(_)));
    }

    #[test]
    fn empty_headers_produce_anonymous() {
        assert_eq!(
            extract_identity_from_headers(None, None),
            AgentIdentity::Anonymous
        );
        assert_eq!(
            extract_identity_from_headers(Some(""), None),
            AgentIdentity::Anonymous
        );
        assert_eq!(
            extract_identity_from_headers(Some("Bearer "), None),
            AgentIdentity::Anonymous
        );
        assert_eq!(
            extract_identity_from_headers(None, Some("")),
            AgentIdentity::Anonymous
        );
    }

    #[test]
    fn authorization_takes_precedence_over_x_api_key() {
        let id = extract_identity_from_headers(Some("Bearer abc"), Some("def"));
        let abc_hash = hash_key("abc");
        match id {
            AgentIdentity::KeyHash(h) => assert_eq!(h, abc_hash),
            _ => panic!("expected KeyHash"),
        }
    }

    #[test]
    fn log_id_never_leaks_plaintext() {
        let id = AgentIdentity::KeyHash(hash_key("supersecret"));
        let s = id.log_id();
        assert!(s.starts_with("sha256:"));
        assert!(!s.contains("supersecret"));
    }

    #[test]
    fn bearer_scheme_is_case_insensitive() {
        // RFC 6750 §2.1: the Bearer auth scheme is case-insensitive.
        // Real-world clients (curl examples, some SDKs) sometimes send
        // lowercase; reject would be a footgun.
        let h = hash_key("abc");
        for prefix in &["Bearer abc", "bearer abc", "BEARER abc", "BeArEr abc"] {
            match extract_identity_from_headers(Some(prefix), None) {
                AgentIdentity::KeyHash(got) => assert_eq!(got, h, "for prefix {prefix:?}"),
                AgentIdentity::Anonymous => panic!("rejected case variant: {prefix:?}"),
            }
        }
    }

    #[test]
    fn empty_bearer_falls_through_to_x_api_key() {
        // `Authorization: Bearer ` with whitespace-only payload should NOT
        // win; the fallthrough to x-api-key must yield its hash. Without
        // this test, a regression that returns Anonymous instead of
        // attempting x-api-key would slip past the precedence test (which
        // pairs a non-empty Bearer with a populated x-api-key).
        let realkey_hash = hash_key("realkey");
        match extract_identity_from_headers(Some("Bearer "), Some("realkey")) {
            AgentIdentity::KeyHash(h) => assert_eq!(h, realkey_hash),
            AgentIdentity::Anonymous => panic!("expected fallthrough to x-api-key"),
        }
    }

    #[test]
    fn whitespace_only_x_api_key_is_anonymous() {
        // `.trim()` should yield empty → Anonymous. Without this assertion,
        // a regression that skipped the trim would hash the literal "   "
        // — a valid (if useless) identity that bucketed all whitespace
        // requests into a single key.
        assert_eq!(
            extract_identity_from_headers(None, Some("   ")),
            AgentIdentity::Anonymous
        );
    }
}
