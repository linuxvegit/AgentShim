//! Fallback chain support (Plan 04 P01 T3, P02 T*).
//!
//! P01 lands the error classifier (`fallback_eligibility`) used by the
//! retry loop to short-circuit non-retryable errors. The chain walker
//! itself arrives in P02 when `ResilientCaller` is introduced.

use agent_shim_providers::ProviderError;

/// Whether a `ProviderError` should trigger a fallback to the next upstream
/// in the chain (or, in the P01 single-upstream case, whether the retry
/// loop should give up).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackEligibility {
    /// The error is transient or load-related; try the next upstream.
    Eligible,
    /// The error is terminal; surface it to the client without retry/fallback.
    Terminal,
}

/// Default mapping per §5.4 of the Phase 4 design (D3). Operators can
/// override this per route via `retry.retry_on`.
pub fn fallback_eligibility(e: &ProviderError) -> FallbackEligibility {
    use FallbackEligibility::*;
    match e {
        ProviderError::Network(_) => Eligible,
        ProviderError::Upstream { status, .. } if *status >= 500 => Eligible,
        ProviderError::Upstream { status, .. } if *status == 429 => Eligible,
        ProviderError::Upstream { .. } => Terminal,           // 4xx (non-429)
        ProviderError::Decode(_) => Terminal,                 // upstream is broken;
                                                              // next upstream wouldn't help
        ProviderError::Encode(_) => Terminal,                 // gateway-side
        ProviderError::CapabilityMismatch(_) => Terminal,
        ProviderError::UnknownProvider(_) => Terminal,        // config bug
    }
}

/// Same logic but consulting a per-route override list. The override list
/// uses string tags matching the canonical YAML names: `network`,
/// `upstream_5xx`, `upstream_429`, `decode`, `encode`, `capability`.
pub fn fallback_eligibility_with_overrides(
    e: &ProviderError,
    retry_on: &[String],
) -> FallbackEligibility {
    use FallbackEligibility::*;
    let tag = error_tag(e);
    if retry_on.iter().any(|t| t == tag) {
        Eligible
    } else {
        Terminal
    }
}

fn error_tag(e: &ProviderError) -> &'static str {
    match e {
        ProviderError::Network(_) => "network",
        ProviderError::Upstream { status, .. } if *status >= 500 => "upstream_5xx",
        ProviderError::Upstream { status, .. } if *status == 429 => "upstream_429",
        ProviderError::Upstream { .. } => "upstream_4xx",
        ProviderError::Decode(_) => "decode",
        ProviderError::Encode(_) => "encode",
        ProviderError::CapabilityMismatch(_) => "capability",
        ProviderError::UnknownProvider(_) => "unknown_provider",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FallbackEligibility::*;

    #[test]
    fn classifier_default_mapping() {
        assert_eq!(fallback_eligibility(&ProviderError::Network("x".into())), Eligible);
        assert_eq!(
            fallback_eligibility(&ProviderError::Upstream { status: 500, body: "x".into() }),
            Eligible
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Upstream { status: 503, body: "x".into() }),
            Eligible
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Upstream { status: 429, body: "x".into() }),
            Eligible
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Upstream { status: 400, body: "x".into() }),
            Terminal
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Upstream { status: 401, body: "x".into() }),
            Terminal
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Decode("x".into())),
            Terminal
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Encode("x".into())),
            Terminal
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::CapabilityMismatch("x".into())),
            Terminal
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::UnknownProvider("x".into())),
            Terminal
        );
    }

    #[test]
    fn override_list_promotes_decode_to_eligible() {
        let retry_on = vec!["decode".to_string(), "network".into()];
        assert_eq!(
            fallback_eligibility_with_overrides(&ProviderError::Decode("x".into()), &retry_on),
            Eligible
        );
        // 5xx no longer in override → terminal under override semantics.
        assert_eq!(
            fallback_eligibility_with_overrides(
                &ProviderError::Upstream { status: 500, body: "x".into() },
                &retry_on
            ),
            Terminal
        );
    }
}
