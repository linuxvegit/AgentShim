use std::pin::Pin;

use futures_core::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::ContentBlock;
use crate::error::StreamError;
use crate::ids::ResponseId;
use crate::usage::{StopReason, Usage};

/// The type of content block being opened in a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentBlockKind {
    Text,
    ToolUse,
    Reasoning,
    RedactedReasoning,
    Image,
    Audio,
    File,
    Unsupported,
}

/// A raw provider-specific SSE event, preserved before canonicalisation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawProviderEvent {
    /// The SSE `event:` type string.
    pub event_type: String,
    /// The parsed JSON data payload.
    pub data: Value,
}

/// The canonical stream event produced by all frontend/backend adapters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// The response has started; carries the response ID.
    MessageStart { id: ResponseId },

    /// A new content block is being opened.
    ContentBlockStart {
        index: usize,
        kind: ContentBlockKind,
        /// For tool_use blocks, the tool name.
        tool_name: Option<String>,
        /// For tool_use blocks, the tool call ID.
        tool_call_id: Option<String>,
    },

    /// A text delta for the current content block.
    TextDelta { index: usize, text: String },

    /// A partial JSON delta for a tool_use argument block.
    InputJsonDelta { index: usize, partial_json: String },

    /// A reasoning text delta.
    ThinkingDelta { index: usize, thinking: String },

    /// A signature for a reasoning block (Anthropic extended thinking).
    SignatureDelta { index: usize, signature: String },

    /// The current content block has finished.
    ContentBlockStop { index: usize },

    /// Final usage statistics and stop reason.
    MessageDelta { stop_reason: StopReason, usage: Usage },

    /// The entire message stream has completed.
    MessageStop,

    /// A complete, pre-assembled content block (used when not streaming).
    Block { index: usize, block: ContentBlock },

    /// Periodic keepalive ping.
    Ping,

    /// An error occurred mid-stream.
    Error { error: StreamError },

    /// A raw provider event that could not be mapped to a canonical event.
    Unknown(RawProviderEvent),
}

/// The canonical async stream of events produced by the gateway.
pub type CanonicalStream =
    Pin<Box<dyn Stream<Item = Result<StreamEvent, StreamError>> + Send + 'static>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_event_message_start_round_trips() {
        let id = ResponseId::new();
        let ev = StreamEvent::MessageStart { id: id.clone() };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"type\":\"message_start\""));
        let back: StreamEvent = serde_json::from_str(&json).unwrap();
        if let StreamEvent::MessageStart { id: back_id } = back {
            assert_eq!(back_id, id);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn text_delta_round_trips() {
        let ev = StreamEvent::TextDelta { index: 0, text: "hello".into() };
        let json = serde_json::to_string(&ev).unwrap();
        let back: StreamEvent = serde_json::from_str(&json).unwrap();
        if let StreamEvent::TextDelta { text, .. } = back {
            assert_eq!(text, "hello");
        } else {
            panic!("wrong variant");
        }
    }
}
