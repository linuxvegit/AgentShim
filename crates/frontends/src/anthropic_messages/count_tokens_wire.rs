//! Wire types for `/v1/messages/count_tokens`.
//!
//! Mirrors `MessagesRequest` from `wire.rs` but makes `max_tokens` optional
//! (the count_tokens API does not require it). `into_messages_request` injects
//! `max_tokens: 0` so the existing `decode::decode` path is reused unchanged.

use serde::Deserialize;
use serde_json::Value;

use super::wire::{
    InboundMessage, InboundTool, InboundToolChoice, MessagesRequest, SystemField, ThinkingConfig,
};

#[derive(Debug, Clone, Deserialize)]
pub struct CountTokensRequest {
    pub model: String,
    pub messages: Vec<InboundMessage>,
    #[serde(default)]
    pub system: Option<SystemField>,
    #[serde(default)]
    pub tools: Option<Vec<InboundTool>>,
    #[serde(default)]
    pub tool_choice: Option<InboundToolChoice>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
}

impl CountTokensRequest {
    /// Convert to `MessagesRequest` for re-use of the existing decoder.
    /// `max_tokens` is filled with 0 when absent — count_tokens never reads it.
    pub fn into_messages_request(self) -> MessagesRequest {
        MessagesRequest {
            model: self.model,
            messages: self.messages,
            system: self.system,
            tools: self.tools,
            tool_choice: self.tool_choice,
            max_tokens: self.max_tokens.unwrap_or(0),
            temperature: self.temperature,
            top_p: self.top_p,
            top_k: self.top_k,
            stop_sequences: self.stop_sequences,
            stream: None,
            metadata: self.metadata,
            thinking: self.thinking,
            output_config: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_without_max_tokens() {
        let body = br#"{
            "model": "claude-opus-4-7",
            "messages": [{"role":"user","content":"hi"}]
        }"#;
        let req: CountTokensRequest = serde_json::from_slice(body).unwrap();
        assert_eq!(req.model, "claude-opus-4-7");
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn decodes_with_max_tokens_present() {
        let body = br#"{
            "model": "m",
            "max_tokens": 1024,
            "messages": [{"role":"user","content":"hi"}]
        }"#;
        let req: CountTokensRequest = serde_json::from_slice(body).unwrap();
        assert_eq!(req.max_tokens, Some(1024));
    }

    #[test]
    fn into_messages_request_injects_zero_when_absent() {
        let body = br#"{
            "model": "m",
            "messages": [{"role":"user","content":"hi"}]
        }"#;
        let req: CountTokensRequest = serde_json::from_slice(body).unwrap();
        let msg = req.into_messages_request();
        assert_eq!(msg.max_tokens, 0);
    }

    #[test]
    fn into_messages_request_preserves_existing_max_tokens() {
        let body = br#"{
            "model": "m",
            "max_tokens": 512,
            "messages": [{"role":"user","content":"hi"}]
        }"#;
        let req: CountTokensRequest = serde_json::from_slice(body).unwrap();
        let msg = req.into_messages_request();
        assert_eq!(msg.max_tokens, 512);
    }
}
