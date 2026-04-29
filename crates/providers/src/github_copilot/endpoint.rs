use serde::Deserialize;

use crate::ProviderError;

#[derive(Debug, Clone, Deserialize)]
pub struct Endpoints {
    pub api: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenExchangeResponse {
    pub token: String,
    pub expires_at: i64,
    pub refresh_in: Option<i64>,
    pub endpoints: Endpoints,
}

/// Validate that `url` is HTTPS and return it with any trailing slash stripped.
pub fn validate_api_base(url: &str) -> Result<String, ProviderError> {
    let parsed =
        url::Url::parse(url).map_err(|e| ProviderError::Encode(format!("invalid URL: {e}")))?;
    if parsed.scheme() != "https" {
        return Err(ProviderError::Encode(format!(
            "API base must use HTTPS, got: {}",
            parsed.scheme()
        )));
    }
    Ok(url.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_http() {
        let err = validate_api_base("http://api.example.com").unwrap_err();
        assert!(err.to_string().contains("HTTPS"));
    }

    #[test]
    fn accept_https_strips_slash() {
        let result = validate_api_base("https://api.example.com/").unwrap();
        assert_eq!(result, "https://api.example.com");
    }

    #[test]
    fn accept_https_no_slash() {
        let result = validate_api_base("https://api.example.com").unwrap();
        assert_eq!(result, "https://api.example.com");
    }
}
