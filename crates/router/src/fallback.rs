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
        ProviderError::Upstream { .. } => Terminal, // 4xx (non-429)
        ProviderError::Decode(_) => Terminal,       // upstream is broken;
        // next upstream wouldn't help
        ProviderError::Encode(_) => Terminal, // gateway-side
        ProviderError::CapabilityMismatch(_) => Terminal,
        ProviderError::UnknownProvider(_) => Terminal, // config bug
    }
}

/// Same logic but consulting a per-route override list. The override list
/// uses string tags matching the canonical YAML names:
///
/// | Tag                  | Variant              | Typical use in `retry_on:` |
/// |----------------------|----------------------|----------------------------|
/// | `network`            | `Network`            | ✅ Recommended             |
/// | `upstream_5xx`       | `Upstream` (≥500)    | ✅ Recommended             |
/// | `upstream_429`       | `Upstream` (==429)   | ✅ Recommended             |
/// | `upstream_4xx`       | `Upstream` (other)   | ⚠️ Rare — see note         |
/// | `decode`             | `Decode`             | ⚠️ Rare                    |
/// | `encode`             | `Encode`             | ❌ Don't                   |
/// | `capability_mismatch`| `CapabilityMismatch` | ❌ Don't                   |
/// | `unknown_provider`   | `UnknownProvider`    | ❌ Don't                   |
///
/// `upstream_4xx`, `encode`, `capability_mismatch`, and `unknown_provider`
/// are emitted for symmetry but adding them to `retry_on:` rarely helps —
/// the next upstream would likely return the same class of error.
///
/// **Pure overlay semantics:** The override list FULLY REPLACES the default
/// mapping. An empty `retry_on: []` makes every error terminal. If you want
/// the defaults plus extras, write the full set out explicitly:
///
/// ```yaml
/// retry:
///   retry_on: [network, upstream_5xx, upstream_429, decode]  # defaults + decode
/// ```
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
        ProviderError::CapabilityMismatch(_) => "capability_mismatch",
        ProviderError::UnknownProvider(_) => "unknown_provider",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FallbackEligibility::*;

    #[test]
    fn classifier_default_mapping() {
        assert_eq!(
            fallback_eligibility(&ProviderError::Network("x".into())),
            Eligible
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Upstream {
                status: 500,
                body: "x".into()
            }),
            Eligible
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Upstream {
                status: 503,
                body: "x".into()
            }),
            Eligible
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Upstream {
                status: 429,
                body: "x".into()
            }),
            Eligible
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Upstream {
                status: 400,
                body: "x".into()
            }),
            Terminal
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Upstream {
                status: 401,
                body: "x".into()
            }),
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
                &ProviderError::Upstream {
                    status: 500,
                    body: "x".into()
                },
                &retry_on
            ),
            Terminal
        );
    }

    #[test]
    fn error_tags_are_stable_canonical_strings() {
        // Pin operator-facing strings. Changing any of these is a
        // breaking change for v0.4.x retry_on overrides.
        assert_eq!(error_tag(&ProviderError::Network("".into())), "network");
        assert_eq!(
            error_tag(&ProviderError::Upstream {
                status: 500,
                body: "".into()
            }),
            "upstream_5xx"
        );
        assert_eq!(
            error_tag(&ProviderError::Upstream {
                status: 503,
                body: "".into()
            }),
            "upstream_5xx"
        );
        assert_eq!(
            error_tag(&ProviderError::Upstream {
                status: 429,
                body: "".into()
            }),
            "upstream_429"
        );
        assert_eq!(
            error_tag(&ProviderError::Upstream {
                status: 401,
                body: "".into()
            }),
            "upstream_4xx"
        );
        assert_eq!(error_tag(&ProviderError::Decode("".into())), "decode");
        assert_eq!(error_tag(&ProviderError::Encode("".into())), "encode");
        assert_eq!(
            error_tag(&ProviderError::CapabilityMismatch("".into())),
            "capability_mismatch"
        );
        assert_eq!(
            error_tag(&ProviderError::UnknownProvider("".into())),
            "unknown_provider"
        );
    }

    #[test]
    fn empty_override_list_makes_everything_terminal() {
        let retry_on: Vec<String> = vec![];
        assert_eq!(
            fallback_eligibility_with_overrides(&ProviderError::Network("x".into()), &retry_on),
            Terminal
        );
        assert_eq!(
            fallback_eligibility_with_overrides(
                &ProviderError::Upstream {
                    status: 500,
                    body: "x".into()
                },
                &retry_on
            ),
            Terminal
        );
    }
}
