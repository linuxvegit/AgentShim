use serde::{Deserialize, Serialize};

use crate::content::ContentBlock;
use crate::ids::ResponseId;
use crate::usage::{StopReason, Usage};

/// A fully-collected (non-streaming) response from a backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalResponse {
    pub id: ResponseId,
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::TextBlock;

    #[test]
    fn canonical_response_round_trips() {
        let resp = CanonicalResponse {
            id: ResponseId::new(),
            model: "claude-3-5-sonnet-20241022".into(),
            content: vec![ContentBlock::Text(TextBlock {
                text: "Hello!".into(),
                extensions: Default::default(),
            })],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: CanonicalResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back, resp);
    }
}
