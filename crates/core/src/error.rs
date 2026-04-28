use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CoreError {
    #[error("invalid model: {0}")]
    InvalidModel(String),

    #[error("capability mismatch: {0}")]
    CapabilityMismatch(String),

    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StreamError {
    #[error("upstream error: {0}")]
    Upstream(String),

    #[error("decode error: {0}")]
    Decode(String),

    #[error("encode error: {0}")]
    Encode(String),

    #[error("client disconnected")]
    ClientDisconnected,

    #[error("stream timeout")]
    Timeout,
}

impl StreamError {
    /// Returns true if the error occurred before the first byte was sent
    /// and is safe to retry.
    pub fn is_retryable_pre_first_byte(&self) -> bool {
        matches!(self, StreamError::Upstream(_) | StreamError::Timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_is_retryable_pre_first_byte() {
        let e = StreamError::Upstream("503".into());
        assert!(e.is_retryable_pre_first_byte());
    }

    #[test]
    fn client_disconnected_is_not_retryable() {
        assert!(!StreamError::ClientDisconnected.is_retryable_pre_first_byte());
    }

    #[test]
    fn core_error_display() {
        let e = CoreError::InvalidModel("gpt-x".into());
        assert!(e.to_string().contains("gpt-x"));
    }
}
