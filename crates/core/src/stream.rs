use std::pin::Pin;

use futures_core::Stream;
use serde::{Deserialize, Serialize};

use crate::error::StreamError;
use crate::ids::{ResponseId, ToolCallId};
use crate::message::MessageRole;
use crate::usage::{StopReason, Usage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentBlockKind {
    Text,
    ToolCall,
    Reasoning,
    RedactedReasoning,
    Image,
    Audio,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawProviderEvent {
    pub provider: String,
    pub event_name: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    ResponseStart { id: ResponseId, model: String, created_at_unix: u64 },
    MessageStart { role: MessageRole },
    ContentBlockStart { index: u32, kind: ContentBlockKind },
    TextDelta { index: u32, text: String },
    ReasoningDelta { index: u32, text: String },
    ToolCallStart { index: u32, id: ToolCallId, name: String },
    ToolCallArgumentsDelta { index: u32, json_fragment: String },
    ToolCallStop { index: u32 },
    ContentBlockStop { index: u32 },
    UsageDelta { usage: Usage },
    MessageStop { stop_reason: StopReason, stop_sequence: Option<String> },
    ResponseStop { usage: Option<Usage> },
    Error { message: String },
    RawProviderEvent(RawProviderEvent),
}

pub type CanonicalStream =
    Pin<Box<dyn Stream<Item = Result<StreamEvent, StreamError>> + Send>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_delta_serializes_with_type_tag() {
        let e = StreamEvent::TextDelta { index: 0, text: "hi".into() };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"type\":\"text_delta\""));
    }

    #[test]
    fn round_trip_preserves_variant() {
        let e = StreamEvent::ToolCallArgumentsDelta { index: 1, json_fragment: r#"{"a":"#.into() };
        let s = serde_json::to_string(&e).unwrap();
        let back: StreamEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }
}
