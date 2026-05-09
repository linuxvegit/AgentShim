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
pub fn extract_identity_from_headers(
    authorization: Option<&str>,
    x_api_key: Option<&str>,
) -> AgentIdentity {
    if let Some(auth) = authorization {
        if let Some(key) = auth.strip_prefix("Bearer ") {
            let key = key.trim();
            if !key.is_empty() {
                return AgentIdentity::KeyHash(hash_key(key));
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
}
