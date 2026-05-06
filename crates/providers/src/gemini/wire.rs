//! JSON DTOs for the Gemini Generate Content API
//! (`generativelanguage.googleapis.com/v1beta`).
//!
//! All structs use `#[serde(rename_all = "camelCase")]` so Rust's snake_case
//! fields map to the API's camelCase wire format. Optional fields use
//! `#[serde(skip_serializing_if = "Option::is_none", default)]` so they're
//! omitted when unset on outbound requests and tolerated when absent on
//! inbound responses.
//!
//! These types are the seam between Plan 03's encoder (`gemini/request.rs`,
//! T4), parser (`gemini/response.rs`, T6), and the wire bytes themselves.
//! `SafetyRating` and `UsageMetadata` are part of the response surface so
//! the parser can lift safety information into `extensions["gemini.safety_ratings"]`
//! per ADR-0002.
//!
//! T2 lands the DTO scaffold only. T4 (encoder) and T6 (parser) consume
//! these types from non-test code, so the module-level `dead_code` allow
//! below is removed when those tasks land.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request DTOs (outgoing)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentRequest {
    pub contents: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_settings: Option<Vec<SafetySetting>>,
}

/// `Content` is used both in requests and responses (with `role`
/// `"user"` | `"model"` | `"function"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Content {
    /// `"user"` (inbound), `"model"` (assistant output), `"function"`
    /// (tool result).
    pub role: String,
    pub parts: Vec<Part>,
}

/// A single part of a [`Content`]. Only one of the optional fields is
/// populated per part.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    /// Text content. Used for both user input and model text output.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub text: Option<String>,
    /// Reasoning marker — when true, this part's text is internal thinking
    /// rather than user-visible content. Maps to canonical
    /// `ContentBlock::Reasoning`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub thought: Option<bool>,
    /// Inline binary data (e.g. images). Vision support — outgoing.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub inline_data: Option<InlineData>,
    /// File reference (URL-based image input). Alternative to `inline_data`
    /// when the API supports URL forms for the model.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub file_data: Option<FileData>,
    /// Model-emitted tool call.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub function_call: Option<FunctionCall>,
    /// Inbound tool result (`role: "function"` parts carry these).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub function_response: Option<FunctionResponse>,
}

/// Vision payload — base64-encoded inline binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineData {
    /// MIME type (e.g. `"image/png"`, `"image/jpeg"`).
    pub mime_type: String,
    /// Base64-encoded bytes.
    pub data: String,
}

/// URL-referenced file payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileData {
    pub mime_type: String,
    pub file_uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionCall {
    pub name: String,
    /// Arguments as a `serde_json::Value` (Gemini emits a real JSON object,
    /// unlike OpenAI's stringified args).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub args: Option<serde_json::Value>,
    /// Optional id (newer API versions; present in some responses).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionResponse {
    pub name: String,
    /// Response payload as a `serde_json::Value`.
    pub response: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub function_declarations: Vec<FunctionDeclaration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionDeclaration {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
    /// JSON Schema for the function's parameters.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parameters: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub top_k: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_output_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub response_mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub response_schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub thinking_config: Option<ThinkingConfig>,
}

/// The headline reasoning knob.
///
/// The mapping from canonical `ReasoningEffort`
/// (minimal/low/medium/high/xhigh) to concrete budgets is documented in
/// `docs/providers/gemini.md`. AgentShim sets `include_thoughts` to
/// `true` whenever a `thinking_budget` is set so the canonical stream can
/// route reasoning into `ContentBlock::Reasoning`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingConfig {
    /// Maximum tokens to spend on internal thinking.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub thinking_budget: Option<i64>,
    /// When true (default false), the response includes `thought` parts.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub include_thoughts: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafetySetting {
    pub category: String,
    pub threshold: String,
}

// ---------------------------------------------------------------------------
// Response DTOs (incoming)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentResponse {
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    #[serde(default)]
    pub prompt_feedback: Option<PromptFeedback>,
    #[serde(default)]
    pub usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub content: Content,
    #[serde(default)]
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub safety_ratings: Vec<SafetyRating>,
    #[serde(default)]
    pub index: Option<i64>,
    #[serde(default)]
    pub citation_metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SafetyRating {
    pub category: String,
    pub probability: String,
    #[serde(default)]
    pub blocked: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptFeedback {
    #[serde(default)]
    pub block_reason: Option<String>,
    #[serde(default)]
    pub safety_ratings: Vec<SafetyRating>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageMetadata {
    #[serde(default)]
    pub prompt_token_count: Option<i64>,
    #[serde(default)]
    pub candidates_token_count: Option<i64>,
    #[serde(default)]
    pub total_token_count: Option<i64>,
    #[serde(default)]
    pub thoughts_token_count: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_part_roundtrip() {
        let part = Part {
            text: Some("hello".into()),
            ..Default::default()
        };
        let s = serde_json::to_string(&part).unwrap();
        assert_eq!(s, r#"{"text":"hello"}"#);

        let back: Part = serde_json::from_str(&s).unwrap();
        assert_eq!(back.text.as_deref(), Some("hello"));
        assert!(back.thought.is_none());
        assert!(back.inline_data.is_none());
        assert!(back.file_data.is_none());
        assert!(back.function_call.is_none());
        assert!(back.function_response.is_none());
    }

    #[test]
    fn thought_part_roundtrip() {
        let part = Part {
            text: Some("...".into()),
            thought: Some(true),
            ..Default::default()
        };
        let s = serde_json::to_string(&part).unwrap();
        assert_eq!(s, r#"{"text":"...","thought":true}"#);

        let back: Part = serde_json::from_str(&s).unwrap();
        assert_eq!(back.text.as_deref(), Some("..."));
        assert_eq!(back.thought, Some(true));
    }

    #[test]
    fn inline_data_part_roundtrip() {
        let part = Part {
            inline_data: Some(InlineData {
                mime_type: "image/png".into(),
                data: "BASE64...".into(),
            }),
            ..Default::default()
        };
        let s = serde_json::to_string(&part).unwrap();
        assert_eq!(
            s,
            r#"{"inlineData":{"mimeType":"image/png","data":"BASE64..."}}"#
        );

        let back: Part = serde_json::from_str(&s).unwrap();
        let inline = back.inline_data.expect("inline_data present");
        assert_eq!(inline.mime_type, "image/png");
        assert_eq!(inline.data, "BASE64...");
    }

    #[test]
    fn function_call_part_serializes_args_object() {
        let part = Part {
            function_call: Some(FunctionCall {
                name: "get_weather".into(),
                args: Some(json!({"city": "Paris"})),
                id: None,
            }),
            ..Default::default()
        };
        let s = serde_json::to_string(&part).unwrap();
        // Args MUST be a real JSON object (NOT stringified, unlike OpenAI).
        assert_eq!(
            s,
            r#"{"functionCall":{"name":"get_weather","args":{"city":"Paris"}}}"#
        );

        let back: Part = serde_json::from_str(&s).unwrap();
        let call = back.function_call.expect("function_call present");
        assert_eq!(call.name, "get_weather");
        assert_eq!(call.args, Some(json!({"city": "Paris"})));
        assert!(call.id.is_none());
    }

    #[test]
    fn generation_config_with_thinking_budget() {
        let cfg = GenerationConfig {
            thinking_config: Some(ThinkingConfig {
                thinking_budget: Some(1024),
                include_thoughts: Some(true),
            }),
            ..Default::default()
        };
        let s = serde_json::to_string(&cfg).unwrap();
        assert_eq!(
            s,
            r#"{"thinkingConfig":{"thinkingBudget":1024,"includeThoughts":true}}"#
        );

        let back: GenerationConfig = serde_json::from_str(&s).unwrap();
        let tc = back.thinking_config.expect("thinking_config present");
        assert_eq!(tc.thinking_budget, Some(1024));
        assert_eq!(tc.include_thoughts, Some(true));
    }

    #[test]
    fn candidate_deserializes_with_finish_reason_and_safety_ratings() {
        let raw = r#"{
            "content": {"role": "model", "parts": [{"text": "hi"}]},
            "finishReason": "STOP",
            "safetyRatings": [
                {"category": "HARM_CATEGORY_HARASSMENT", "probability": "NEGLIGIBLE"}
            ]
        }"#;
        let cand: Candidate = serde_json::from_str(raw).unwrap();
        assert_eq!(cand.content.role, "model");
        assert_eq!(cand.content.parts.len(), 1);
        assert_eq!(cand.content.parts[0].text.as_deref(), Some("hi"));
        assert_eq!(cand.finish_reason.as_deref(), Some("STOP"));
        assert_eq!(cand.safety_ratings.len(), 1);
        assert_eq!(cand.safety_ratings[0].category, "HARM_CATEGORY_HARASSMENT");
        assert_eq!(cand.safety_ratings[0].probability, "NEGLIGIBLE");
        assert!(cand.safety_ratings[0].blocked.is_none());
    }

    #[test]
    fn usage_metadata_with_thoughts_token_count() {
        let raw = r#"{
            "promptTokenCount": 10,
            "candidatesTokenCount": 5,
            "thoughtsTokenCount": 42,
            "totalTokenCount": 57
        }"#;
        let usage: UsageMetadata = serde_json::from_str(raw).unwrap();
        assert_eq!(usage.prompt_token_count, Some(10));
        assert_eq!(usage.candidates_token_count, Some(5));
        assert_eq!(usage.thoughts_token_count, Some(42));
        assert_eq!(usage.total_token_count, Some(57));
    }

    #[test]
    fn safety_rating_blocked_field_deserializes() {
        let raw = r#"{
            "category": "HARM_CATEGORY_DANGEROUS",
            "probability": "HIGH",
            "blocked": true
        }"#;
        let rating: SafetyRating = serde_json::from_str(raw).unwrap();
        assert_eq!(rating.category, "HARM_CATEGORY_DANGEROUS");
        assert_eq!(rating.probability, "HIGH");
        assert_eq!(rating.blocked, Some(true));
    }

    #[test]
    fn prompt_feedback_with_block_reason() {
        let raw = r#"{"blockReason":"SAFETY","safetyRatings":[]}"#;
        let pf: PromptFeedback = serde_json::from_str(raw).unwrap();
        assert_eq!(pf.block_reason.as_deref(), Some("SAFETY"));
        assert!(pf.safety_ratings.is_empty());
    }

    #[test]
    fn unknown_top_level_fields_are_ignored_on_response_deserialize() {
        let raw = r#"{"candidates":[],"futureField":"value"}"#;
        let resp: GenerateContentResponse = serde_json::from_str(raw).unwrap();
        assert!(resp.candidates.is_empty());
        assert!(resp.prompt_feedback.is_none());
        assert!(resp.usage_metadata.is_none());
    }
}
