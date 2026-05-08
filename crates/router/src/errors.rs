//! Errors raised by the resilience layer (Plan 04 P02+).
//!
//! These are distinct from `ProviderError` (which describes a single
//! upstream call's outcome) and `RouteError` (which describes resolver
//! failures). `ResilienceError` describes outcomes that involved walking
//! the chain.

use agent_shim_providers::ProviderError;
use thiserror::Error;

/// Per-attempt summary used for operator logging on failure.
#[derive(Debug, Clone)]
pub struct TriedUpstream {
    pub provider: String,
    pub model: String,
    pub attempts: u32,
    pub last_error_tag: String, // e.g. "upstream_5xx", "network", "decode"
    pub last_error_msg: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Error)]
pub enum ResilienceError {
    /// Every chain element was attempted; every retry budget exhausted; the
    /// most recent error was fallback-eligible (so we walked off the end of
    /// the chain). HTTP 503.
    #[error("no upstream succeeded after trying {} options", tried.len())]
    NoUpstreamSucceeded {
        tried: Vec<TriedUpstream>,
        last_error: ProviderError,
    },

    /// Some chain element returned a terminal error; we stopped without
    /// trying the rest. The HTTP status passes through (typically 4xx from
    /// the provider's `Upstream{status}`).
    #[error("terminal error from upstream: {error}")]
    TerminalError {
        error: ProviderError,
        tried: Vec<TriedUpstream>,
    },

    /// Plan 03 will populate this. Defined here so HandlerError mapping is
    /// stable across plans.
    #[error("all {} upstreams have open circuit breakers", tried.len())]
    AllBreakersOpen { tried: Vec<String> },

    /// Plan 04 will populate this. Defined here for the same reason.
    #[error("rate limited on {dimension:?}; retry after {retry_after_secs}s")]
    RateLimited {
        dimension: RateLimitDimension,
        retry_after_secs: u32,
    },
}

/// Which bucket dimension rejected the request.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy)]
pub enum RateLimitDimension {
    PerKey,
    PerRoute,
    PerUpstream,
    PerIp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_are_useful_for_operators() {
        let err = ResilienceError::NoUpstreamSucceeded {
            tried: vec![TriedUpstream {
                provider: "openai".into(),
                model: "gpt-4o".into(),
                attempts: 3,
                last_error_tag: "upstream_5xx".into(),
                last_error_msg: "502 Bad Gateway".into(),
                elapsed_ms: 1200,
            }],
            last_error: ProviderError::Upstream {
                status: 502,
                body: "x".into(),
            },
        };
        assert!(err.to_string().contains("1 options"));
    }

    #[test]
    fn rate_limited_display_includes_dimension_and_secs() {
        let err = ResilienceError::RateLimited {
            dimension: RateLimitDimension::PerKey,
            retry_after_secs: 30,
        };
        let s = err.to_string();
        assert!(s.contains("PerKey"));
        assert!(s.contains("30"));
    }
}
