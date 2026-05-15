//! `PluginRegistry` — the gateway-facing surface of the plugin
//! system. Built at startup from a list of factories + parsed YAML
//! plugin entries. Owns the route_index used by `pipeline.rs` to
//! decide which plugins (if any) run for each request.
//!
//! Spec §6.1 / §6.2. Construction here is intentionally minimal —
//! the run_* methods come in P04 after pipeline integration is
//! designed; the JoinSet machinery for H7 spawn lifecycle comes in
//! P05.

use std::collections::HashMap;
use std::sync::Arc;

use agent_shim_core::FrontendKind;

use crate::error::{OnError, PluginConfigError};
use crate::trait_def::{Hook, Plugin};

/// Per-hook timeouts for a single plugin. Defaults follow spec §5.4:
/// 50 ms for H2/H3/H7, 5 ms for H5.
#[derive(Debug, Clone, Copy)]
pub struct HookTimeouts {
    pub on_decoded_request: u64,
    pub on_resolved: u64,
    pub on_stream_event: u64,
    pub on_response_complete: u64,
}

impl Default for HookTimeouts {
    fn default() -> Self {
        Self {
            on_decoded_request: 50,
            on_resolved: 50,
            on_stream_event: 5,
            on_response_complete: 50,
        }
    }
}

impl HookTimeouts {
    /// Apply a single value to every hook (the simple YAML form
    /// `timeout_ms: 50`).
    pub fn uniform(ms: u64) -> Self {
        Self {
            on_decoded_request: ms,
            on_resolved: ms,
            on_stream_event: ms,
            on_response_complete: ms,
        }
    }

    #[allow(dead_code)] // used by P04 run_* methods
    pub(crate) fn for_hook(&self, hook: Hook) -> u64 {
        match hook {
            Hook::DecodedRequest => self.on_decoded_request,
            Hook::Resolved => self.on_resolved,
            Hook::StreamEvent => self.on_stream_event,
            Hook::ResponseComplete => self.on_response_complete,
        }
    }
}

/// One entry in the registry, plus the policy knobs bound to it.
pub struct PluginEntry {
    pub name: String,
    pub plugin: Arc<dyn Plugin>,
    pub on_error: OnError,
    pub timeouts: HookTimeouts,
    pub enabled: bool,
}

/// All routes' per-hook ordered plugin lists, grouped by frontend.
/// The fast-path lookup is `plans.get(&frontend)` — when the result
/// is `None` or `is_empty == true`, the wrapper returns identity
/// without examining the inner maps (Q5 option C).
#[allow(dead_code)] // fields populated by P03 from_specs constructor
pub(crate) struct FrontendRoutePlans {
    pub(crate) specific: HashMap<String, RouteHookPlan>,
    pub(crate) wildcard: Option<RouteHookPlan>,
    pub(crate) is_empty: bool,
}

#[derive(Default, Clone)]
#[allow(dead_code)] // fields populated by P03 from_specs constructor
pub(crate) struct RouteHookPlan {
    pub(crate) on_decoded_request: Vec<Arc<PluginEntry>>,
    pub(crate) on_resolved: Vec<Arc<PluginEntry>>,
    pub(crate) on_stream_event: Vec<Arc<PluginEntry>>,
    pub(crate) on_response_complete: Vec<Arc<PluginEntry>>,
}

impl RouteHookPlan {
    #[allow(dead_code)] // used by P04 run_* methods
    pub(crate) fn is_empty(&self) -> bool {
        self.on_decoded_request.is_empty()
            && self.on_resolved.is_empty()
            && self.on_stream_event.is_empty()
            && self.on_response_complete.is_empty()
    }
}

/// Top-level plugin registry. Constructed once at startup. Immutable
/// thereafter; reload rebuilds the whole thing and arc-swaps (Q12).
pub struct PluginRegistry {
    #[allow(dead_code)] // used by P04 run_* methods
    pub(crate) plugins: HashMap<String, Arc<PluginEntry>>,
    #[allow(dead_code)] // used by P04 run_* methods
    pub(crate) plans: HashMap<FrontendKind, FrontendRoutePlans>,
}

impl PluginRegistry {
    /// Build an empty registry — no plugins, no plans. Used as the
    /// fast-path default in tests and in YAML configs that omit
    /// `plugins:` entirely.
    pub fn empty() -> Self {
        Self {
            plugins: HashMap::new(),
            plans: HashMap::new(),
        }
    }

    /// Lookup helper used by P04's `run_*` methods. Returns the route
    /// plan matching `(frontend, model)` if any plugin actually subscribes
    /// to any hook for that route. Returns `None` for the fast path.
    #[allow(dead_code)] // used by P04 run_* methods
    pub(crate) fn lookup<'a>(
        &'a self,
        frontend: FrontendKind,
        model: &str,
    ) -> Option<&'a RouteHookPlan> {
        let fp = self.plans.get(&frontend)?;
        if fp.is_empty {
            return None;
        }
        // Specific match first, then wildcard.
        if let Some(plan) = fp.specific.get(model) {
            return Some(plan);
        }
        fp.wildcard.as_ref()
    }
}

/// Errors the registry surfaces during construction. Wrapped by
/// `gateway::main` into the boot-time error envelope.
#[derive(Debug, thiserror::Error)]
pub enum RegistryBuildError {
    #[error("plugin `{plugin}` references unknown kind `{kind}` (known: {known:?})")]
    UnknownKind {
        plugin: String,
        kind: String,
        known: Vec<String>,
    },
    #[error("plugin `{1}`: {0}")]
    Instantiation(#[source] PluginConfigError, /* plugin name */ String),
    #[error(
        "route `{frontend:?}/{model}` references plugin `{plugin}` on hook `{hook}`, but that \
         plugin does not subscribe to it (subscribed: {subscribed:?})"
    )]
    HookSubscriptionMismatch {
        frontend: FrontendKind,
        model: String,
        plugin: String,
        hook: &'static str,
        subscribed: Vec<&'static str>,
    },
}

// ── Construction ────────────────────────────────────────────────────────────
//
// The full constructor (`PluginRegistry::from_specs`) lives in P03 once
// the config crate exposes parsed plugin/route entries. For P02 we ship
// `empty()` plus the type machinery so that pipeline-integration tests
// in P04 can use `PluginRegistry::empty()` as the no-plugins baseline.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_lookup_returns_none() {
        let r = PluginRegistry::empty();
        assert!(r
            .lookup(FrontendKind::AnthropicMessages, "anything")
            .is_none());
    }

    #[test]
    fn hook_timeouts_defaults() {
        let t = HookTimeouts::default();
        assert_eq!(t.on_decoded_request, 50);
        assert_eq!(t.on_resolved, 50);
        assert_eq!(t.on_stream_event, 5);
        assert_eq!(t.on_response_complete, 50);
    }

    #[test]
    fn hook_timeouts_uniform() {
        let t = HookTimeouts::uniform(20);
        assert_eq!(t.on_decoded_request, 20);
        assert_eq!(t.on_stream_event, 20);
    }

    #[test]
    fn hook_timeouts_for_hook_lookup() {
        let t = HookTimeouts {
            on_decoded_request: 100,
            on_resolved: 200,
            on_stream_event: 300,
            on_response_complete: 400,
        };
        assert_eq!(t.for_hook(Hook::DecodedRequest), 100);
        assert_eq!(t.for_hook(Hook::Resolved), 200);
        assert_eq!(t.for_hook(Hook::StreamEvent), 300);
        assert_eq!(t.for_hook(Hook::ResponseComplete), 400);
    }

    #[test]
    fn route_hook_plan_default_is_empty() {
        let plan = RouteHookPlan::default();
        assert!(plan.is_empty());
    }
}
