//! Canonical safety rating types.
//!
//! Modelled on the Gemini API's `safetyRatings` array, but generalised so other
//! providers (or future Gemini revisions) can populate the same field. This is
//! the first new field added to the canonical model since v0.1; see ADR-0003
//! ("safety_ratings promotion to typed canonical surface") for the policy
//! rationale and migration plan.
//!
//! # Wire format
//!
//! Each rating round-trips as:
//!
//! ```json
//! {"category": "HARM_CATEGORY_HARASSMENT", "probability": "NEGLIGIBLE"}
//! ```
//!
//! Both `category` and `probability` are *open* enums — unrecognised wire
//! values are preserved as `Other(String)` so they can be forwarded
//! losslessly. This mirrors the [`crate::usage::StopReason::Other`] precedent.
use serde::{Deserialize, Serialize};

// Wire-string constants — referenced by both the `From<String>` parser and the
// `From<Self> for String` formatter so a typo on either side is impossible.
const WIRE_HATE_SPEECH: &str = "HARM_CATEGORY_HATE_SPEECH";
const WIRE_HARASSMENT: &str = "HARM_CATEGORY_HARASSMENT";
const WIRE_DANGEROUS_CONTENT: &str = "HARM_CATEGORY_DANGEROUS_CONTENT";
const WIRE_SEXUALLY_EXPLICIT: &str = "HARM_CATEGORY_SEXUALLY_EXPLICIT";

const WIRE_NEGLIGIBLE: &str = "NEGLIGIBLE";
const WIRE_LOW: &str = "LOW";
const WIRE_MEDIUM: &str = "MEDIUM";
const WIRE_HIGH: &str = "HIGH";

/// A single safety classification produced by the upstream model.
///
/// Multiple ratings are typically returned per response; one per safety
/// category that the provider evaluates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyRating {
    /// Which safety dimension this rating describes.
    pub category: SafetyCategory,
    /// How likely the content was to violate the [`SafetyCategory`].
    pub probability: SafetyLevel,
}

/// Safety dimensions evaluated by the provider.
///
/// Open enum: any wire value that isn't one of the known categories
/// round-trips through [`SafetyCategory::Other`] preserving the original
/// string verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum SafetyCategory {
    /// `HARM_CATEGORY_HATE_SPEECH`
    HateSpeech,
    /// `HARM_CATEGORY_HARASSMENT`
    Harassment,
    /// `HARM_CATEGORY_DANGEROUS_CONTENT`
    DangerousContent,
    /// `HARM_CATEGORY_SEXUALLY_EXPLICIT`
    SexuallyExplicit,
    /// Any wire value not recognised above. The inner string is the verbatim
    /// wire value so it round-trips exactly.
    Other(String),
}

impl From<String> for SafetyCategory {
    fn from(s: String) -> Self {
        match s.as_str() {
            WIRE_HATE_SPEECH => SafetyCategory::HateSpeech,
            WIRE_HARASSMENT => SafetyCategory::Harassment,
            WIRE_DANGEROUS_CONTENT => SafetyCategory::DangerousContent,
            WIRE_SEXUALLY_EXPLICIT => SafetyCategory::SexuallyExplicit,
            _ => SafetyCategory::Other(s),
        }
    }
}

impl From<SafetyCategory> for String {
    fn from(c: SafetyCategory) -> Self {
        match c {
            SafetyCategory::HateSpeech => WIRE_HATE_SPEECH.to_owned(),
            SafetyCategory::Harassment => WIRE_HARASSMENT.to_owned(),
            SafetyCategory::DangerousContent => WIRE_DANGEROUS_CONTENT.to_owned(),
            SafetyCategory::SexuallyExplicit => WIRE_SEXUALLY_EXPLICIT.to_owned(),
            SafetyCategory::Other(s) => s,
        }
    }
}

/// Likelihood that the content fell into the associated [`SafetyCategory`].
///
/// Open enum: any wire value not recognised below round-trips through
/// [`SafetyLevel::Other`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum SafetyLevel {
    /// `NEGLIGIBLE`
    Negligible,
    /// `LOW`
    Low,
    /// `MEDIUM`
    Medium,
    /// `HIGH`
    High,
    /// Any wire value not recognised above.
    Other(String),
}

impl From<String> for SafetyLevel {
    fn from(s: String) -> Self {
        match s.as_str() {
            WIRE_NEGLIGIBLE => SafetyLevel::Negligible,
            WIRE_LOW => SafetyLevel::Low,
            WIRE_MEDIUM => SafetyLevel::Medium,
            WIRE_HIGH => SafetyLevel::High,
            _ => SafetyLevel::Other(s),
        }
    }
}

impl From<SafetyLevel> for String {
    fn from(l: SafetyLevel) -> Self {
        match l {
            SafetyLevel::Negligible => WIRE_NEGLIGIBLE.to_owned(),
            SafetyLevel::Low => WIRE_LOW.to_owned(),
            SafetyLevel::Medium => WIRE_MEDIUM.to_owned(),
            SafetyLevel::High => WIRE_HIGH.to_owned(),
            SafetyLevel::Other(s) => s,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn known_rating_round_trips_to_expected_wire_shape() {
        let rating = SafetyRating {
            category: SafetyCategory::Harassment,
            probability: SafetyLevel::Negligible,
        };
        let json = serde_json::to_string(&rating).unwrap();
        assert_eq!(
            json,
            r#"{"category":"HARM_CATEGORY_HARASSMENT","probability":"NEGLIGIBLE"}"#
        );
        let back: SafetyRating = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rating);
    }

    #[test]
    fn unknown_category_preserves_wire_value_via_other() {
        let json = r#"{"category":"HARM_CATEGORY_FUTURE","probability":"HIGH"}"#;
        let rating: SafetyRating = serde_json::from_str(json).unwrap();
        assert_eq!(
            rating.category,
            SafetyCategory::Other("HARM_CATEGORY_FUTURE".to_owned())
        );
        assert_eq!(rating.probability, SafetyLevel::High);
        // Re-serialise must produce the same wire string verbatim.
        let back = serde_json::to_string(&rating).unwrap();
        assert_eq!(back, json);
    }

    #[test]
    fn unknown_level_preserves_wire_value_via_other() {
        let json = r#"{"category":"HARM_CATEGORY_HARASSMENT","probability":"VERY_HIGH"}"#;
        let rating: SafetyRating = serde_json::from_str(json).unwrap();
        assert_eq!(rating.category, SafetyCategory::Harassment);
        assert_eq!(
            rating.probability,
            SafetyLevel::Other("VERY_HIGH".to_owned())
        );
        let back = serde_json::to_string(&rating).unwrap();
        assert_eq!(back, json);
    }

    fn any_category_string() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("HARM_CATEGORY_HATE_SPEECH".to_owned()),
            Just("HARM_CATEGORY_HARASSMENT".to_owned()),
            Just("HARM_CATEGORY_DANGEROUS_CONTENT".to_owned()),
            Just("HARM_CATEGORY_SEXUALLY_EXPLICIT".to_owned()),
            "[A-Z_]{3,30}",
        ]
    }

    fn any_probability_string() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("NEGLIGIBLE".to_owned()),
            Just("LOW".to_owned()),
            Just("MEDIUM".to_owned()),
            Just("HIGH".to_owned()),
            "[A-Z_]{3,20}",
        ]
    }

    proptest! {
        #[test]
        fn safety_rating_roundtrips(
            category in any_category_string(),
            probability in any_probability_string(),
        ) {
            let rating = SafetyRating {
                category: SafetyCategory::from(category.clone()),
                probability: SafetyLevel::from(probability.clone()),
            };
            let json = serde_json::to_string(&rating).unwrap();
            let back: SafetyRating = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back, rating);
        }
    }
}
