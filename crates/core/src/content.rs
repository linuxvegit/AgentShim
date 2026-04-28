use serde::{Deserialize, Serialize};

use crate::media::BinarySource;
use crate::tool::{ToolCallBlock, ToolResultBlock};

/// A plain text content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextBlock {
    pub text: String,
}

/// An image content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageBlock {
    pub source: BinarySource,
    pub alt_text: Option<String>,
}

/// An audio content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioBlock {
    pub source: BinarySource,
    pub transcript: Option<String>,
}

/// A file content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileBlock {
    pub source: BinarySource,
    pub filename: Option<String>,
    pub media_type: Option<String>,
}

/// A chain-of-thought reasoning block (Anthropic extended thinking).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningBlock {
    pub thinking: String,
    pub signature: Option<String>,
}

/// A redacted reasoning block (opaque to the caller).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactedReasoningBlock {
    pub data: String,
}

/// A block with a content type that this version of the gateway does not recognise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsupportedBlock {
    /// The raw `type` string from the provider.
    pub type_name: String,
    /// Full raw JSON of the original block, preserved for pass-through.
    pub raw: serde_json::Value,
}

/// The canonical union of all content block variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text(TextBlock),
    Image(ImageBlock),
    Audio(AudioBlock),
    File(FileBlock),
    ToolUse(ToolCallBlock),
    ToolResult(ToolResultBlock),
    Reasoning(ReasoningBlock),
    RedactedReasoning(RedactedReasoningBlock),
    Unsupported(UnsupportedBlock),
}

impl From<TextBlock> for ContentBlock {
    fn from(b: TextBlock) -> Self {
        ContentBlock::Text(b)
    }
}

impl From<ToolCallBlock> for ContentBlock {
    fn from(b: ToolCallBlock) -> Self {
        ContentBlock::ToolUse(b)
    }
}

impl From<ToolResultBlock> for ContentBlock {
    fn from(b: ToolResultBlock) -> Self {
        ContentBlock::ToolResult(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_block_round_trips() {
        let block = ContentBlock::Text(TextBlock {
            text: "hello world".into(),
        });
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"text\""));
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, block);
    }

    #[test]
    fn unsupported_block_preserves_raw() {
        let block = ContentBlock::Unsupported(UnsupportedBlock {
            type_name: "exotic_type".into(),
            raw: serde_json::json!({"type": "exotic_type", "data": 42}),
        });
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, block);
    }
}
