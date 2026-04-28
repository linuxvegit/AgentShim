use serde::{Deserialize, Serialize};

use crate::extensions::ExtensionMap;
use crate::media::BinarySource;
use crate::tool::{ToolCallBlock, ToolResultBlock};

/// A plain text content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextBlock {
    pub text: String,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

/// An image content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageBlock {
    pub source: BinarySource,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

/// An audio content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioBlock {
    pub source: BinarySource,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

/// A file content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileBlock {
    pub source: BinarySource,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

/// A chain-of-thought reasoning block (Anthropic extended thinking).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningBlock {
    pub text: String,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

/// A redacted reasoning block (opaque to the caller).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactedReasoningBlock {
    pub data: String,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

/// A block with a content type that this version of the gateway does not recognise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsupportedBlock {
    /// Where this block originated (provider name or adapter name).
    pub origin: String,
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
    ToolCall(ToolCallBlock),
    ToolResult(ToolResultBlock),
    Reasoning(ReasoningBlock),
    RedactedReasoning(RedactedReasoningBlock),
    Unsupported(UnsupportedBlock),
}

impl ContentBlock {
    /// Convenience constructor for a plain text block.
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text(TextBlock {
            text: s.into(),
            extensions: ExtensionMap::new(),
        })
    }
}

impl From<TextBlock> for ContentBlock {
    fn from(b: TextBlock) -> Self {
        ContentBlock::Text(b)
    }
}

impl From<ToolCallBlock> for ContentBlock {
    fn from(b: ToolCallBlock) -> Self {
        ContentBlock::ToolCall(b)
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
            extensions: ExtensionMap::new(),
        });
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"text\""));
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, block);
    }

    #[test]
    fn text_convenience_constructor() {
        let block = ContentBlock::text("hello");
        match &block {
            ContentBlock::Text(t) => assert_eq!(t.text, "hello"),
            _ => panic!("expected Text variant"),
        }
    }

    #[test]
    fn unsupported_block_preserves_raw() {
        let block = ContentBlock::Unsupported(UnsupportedBlock {
            origin: "exotic_provider".into(),
            raw: serde_json::json!({"type": "exotic_type", "data": 42}),
        });
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, block);
    }
}
