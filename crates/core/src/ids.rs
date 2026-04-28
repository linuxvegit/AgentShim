use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub String);

impl RequestId {
    pub fn new() -> Self {
        Self(format!("req_{}", Uuid::new_v4().simple()))
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResponseId(pub String);

impl ResponseId {
    pub fn new() -> Self {
        Self(format!("resp_{}", Uuid::new_v4().simple()))
    }
}

impl Default for ResponseId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolCallId(pub String);

impl ToolCallId {
    pub fn new() -> Self {
        Self(format!("call_{}", Uuid::new_v4().simple()))
    }

    pub fn from_provider(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl Default for ToolCallId {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_has_prefix_and_is_unique() {
        let a = RequestId::new();
        let b = RequestId::new();
        assert!(a.0.starts_with("req_"));
        assert_ne!(a, b);
    }

    #[test]
    fn tool_call_id_round_trips_as_string() {
        let id = ToolCallId::from_provider("call_abc");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"call_abc\"");
        let back: ToolCallId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }
}
