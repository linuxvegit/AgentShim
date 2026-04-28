use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A map of provider-specific extension fields, keyed by string.
/// Serializes as a plain JSON object (transparent newtype over BTreeMap).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExtensionMap(pub BTreeMap<String, serde_json::Value>);

impl ExtensionMap {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn insert(&mut self, key: impl Into<String>, value: serde_json::Value) {
        self.0.insert(key.into(), value);
    }

    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.0.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_extension_map_serializes_to_empty_object() {
        let m = ExtensionMap::new();
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, "{}");
    }

    #[test]
    fn extension_map_round_trips() {
        let mut m = ExtensionMap::new();
        m.insert("foo", serde_json::json!(42));
        m.insert("bar", serde_json::json!("hello"));
        let json = serde_json::to_string(&m).unwrap();
        let back: ExtensionMap = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.get("foo"), Some(&serde_json::json!(42)));
    }
}
