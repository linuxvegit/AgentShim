use serde::{Deserialize, Serialize};
use std::fmt;

/// A string value that is redacted in Debug and Display output.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[REDACTED]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_value() {
        let s = Secret::new("super-secret-key");
        assert_eq!(format!("{:?}", s), "[REDACTED]");
        assert_eq!(format!("{}", s), "[REDACTED]");
    }

    #[test]
    fn expose_returns_value() {
        let s = Secret::new("my-key");
        assert_eq!(s.expose(), "my-key");
    }

    #[test]
    fn deserializes_from_plain_string() {
        let s: Secret = serde_json::from_str("\"my-api-key\"").unwrap();
        assert_eq!(s.expose(), "my-api-key");
    }

    #[test]
    fn serializes_as_plain_string() {
        let s = Secret::new("my-api-key");
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"my-api-key\"");
    }
}
