/// Headers that contain sensitive values and should be redacted in logs.
pub static SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "api-key",
    "cookie",
    "set-cookie",
    "proxy-authorization",
    "x-auth-token",
    "x-secret",
];

/// Returns true if the header name is considered sensitive (case-insensitive).
pub fn is_sensitive(header: &str) -> bool {
    SENSITIVE_HEADERS
        .iter()
        .any(|&s| s.eq_ignore_ascii_case(header))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_sensitive_headers_case_insensitively() {
        assert!(is_sensitive("Authorization"));
        assert!(is_sensitive("AUTHORIZATION"));
        assert!(is_sensitive("authorization"));
        assert!(is_sensitive("X-Api-Key"));
        assert!(is_sensitive("x-api-key"));
        assert!(is_sensitive("Cookie"));
    }

    #[test]
    fn non_sensitive_headers_return_false() {
        assert!(!is_sensitive("content-type"));
        assert!(!is_sensitive("Accept"));
        assert!(!is_sensitive("x-request-id"));
    }
}
