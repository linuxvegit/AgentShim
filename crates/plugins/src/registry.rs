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

use crate::error::{OnError, PluginConfigError, PluginError};
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
    #[allow(dead_code)] // used by P04 T2/T3 (run_on_resolved, wrap_stream) and tests
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
    #[allow(dead_code)] // populated by P06 from_specs; lookup goes via `plans`
    pub(crate) plugins: HashMap<String, Arc<PluginEntry>>,
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

    /// H2 hook chain walk. Spec §6.2 / §6.3 / §6.4 / §4.5.
    ///
    /// Walks the route's `on_decoded_request` list in declaration order.
    /// Each plugin sees the request as left by the previous successful
    /// plugin (clone-then-swap on every iteration). Errors are routed
    /// through the per-plugin `on_error` policy by `invoke()`; `Aborted`
    /// always propagates. The protected-field diff (`id`, `frontend`,
    /// `model`, `stream`) runs after every successful call.
    ///
    /// Fast path: when `lookup()` returns `None` or the route's
    /// `on_decoded_request` list is empty, the function returns `Ok(req)`
    /// without cloning. This is the zero-overhead case verified by the
    /// integration test in T12.
    pub async fn run_on_decoded_request(
        &self,
        route: (FrontendKind, &str),
        ctx: &crate::PluginContext,
        mut req: agent_shim_core::CanonicalRequest,
    ) -> Result<agent_shim_core::CanonicalRequest, PluginError> {
        let Some(plan) = self.lookup(route.0, route.1) else {
            return Ok(req);
        };
        if plan.on_decoded_request.is_empty() {
            return Ok(req);
        }
        for entry in &plan.on_decoded_request {
            if !entry.enabled {
                continue;
            }
            let candidate = req.clone();
            let plugin = entry.plugin.clone();
            let plugin_name = entry.name.clone();
            let hook_str = Hook::DecodedRequest.as_str();
            let outcome = crate::invoke::invoke(
                &plugin_name,
                ctx,
                hook_str,
                entry.timeouts.for_hook(Hook::DecodedRequest),
                entry.on_error,
                plugin.on_decoded_request(ctx, candidate),
            )
            .await;
            match outcome {
                crate::invoke::InvokeOutcome::Success(new_req) => {
                    // Protected-field diff: id / frontend / model / stream
                    // are not allowed to change. If the plugin mutated one,
                    // honour on_error (Skip → keep prior `req`; Fail →
                    // propagate ProtectedFieldMutated as 502).
                    if let Err(e) = crate::invoke::check_protected_fields(
                        &plugin_name,
                        hook_str,
                        &req,
                        &new_req,
                    ) {
                        match entry.on_error {
                            OnError::Skip => continue,
                            OnError::Fail => return Err(e),
                        }
                    }
                    req = new_req;
                }
                crate::invoke::InvokeOutcome::Skipped => {
                    // Keep prior `req`; loop to next plugin.
                }
                crate::invoke::InvokeOutcome::Propagate(err) => {
                    return Err(err);
                }
            }
        }
        Ok(req)
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
    use agent_shim_core::{
        request::RequestMetadata, CanonicalRequest, ContentBlock, ExtensionMap, FrontendInfo,
        FrontendModel, GenerationOptions, Message, RequestId, ResolvedPolicy, TextBlock,
    };
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn req() -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: FrontendModel::from("m"),
            },
            model: FrontendModel::from("m"),
            system: vec![],
            messages: vec![Message::user(vec![ContentBlock::Text(TextBlock {
                text: "hello".to_string(),
                extensions: ExtensionMap::new(),
            })])],
            tools: vec![],
            tool_choice: Default::default(),
            generation: GenerationOptions::default(),
            response_format: None,
            stream: false,
            metadata: RequestMetadata::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: ResolvedPolicy::default(),
            extensions: ExtensionMap::new(),
        }
    }

    /// Stub plugin used by `run_on_decoded_request` tests. Increments a
    /// shared atomic counter on each call and appends a marker message
    /// to `req.messages` so we can verify firing order downstream.
    struct CounterPlugin {
        n: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl crate::Plugin for CounterPlugin {
        fn kind_name(&self) -> &'static str {
            "counter"
        }
        fn hooks(&self) -> crate::HookSet {
            crate::HookSet::DECODED_REQUEST
        }
        async fn on_decoded_request(
            &self,
            _ctx: &crate::PluginContext,
            mut req: CanonicalRequest,
        ) -> crate::PluginResult<CanonicalRequest> {
            self.n.fetch_add(1, Ordering::SeqCst);
            req.messages
                .push(Message::user(vec![ContentBlock::Text(TextBlock {
                    text: "counter".to_string(),
                    extensions: ExtensionMap::new(),
                })]));
            Ok(req)
        }
    }

    fn registry_with_two_h2_plugins(
        counter_a: Arc<AtomicUsize>,
        counter_b: Arc<AtomicUsize>,
    ) -> PluginRegistry {
        let a = Arc::new(PluginEntry {
            name: "a".to_string(),
            plugin: Arc::new(CounterPlugin { n: counter_a }),
            on_error: OnError::Skip,
            timeouts: HookTimeouts::default(),
            enabled: true,
        });
        let b = Arc::new(PluginEntry {
            name: "b".to_string(),
            plugin: Arc::new(CounterPlugin { n: counter_b }),
            on_error: OnError::Skip,
            timeouts: HookTimeouts::default(),
            enabled: true,
        });
        let plan = RouteHookPlan {
            on_decoded_request: vec![a.clone(), b.clone()],
            ..Default::default()
        };
        let mut plans = HashMap::new();
        plans.insert(
            FrontendKind::AnthropicMessages,
            FrontendRoutePlans {
                specific: {
                    let mut m = HashMap::new();
                    m.insert("test-model".to_string(), plan);
                    m
                },
                wildcard: None,
                is_empty: false,
            },
        );
        let mut plugins = HashMap::new();
        plugins.insert("a".to_string(), a);
        plugins.insert("b".to_string(), b);
        PluginRegistry { plugins, plans }
    }

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

    // ── Plan 07 P04 T1: run_on_decoded_request ──────────────────────────────

    #[tokio::test]
    async fn run_on_decoded_request_fires_two_plugins_in_order() {
        let a = Arc::new(AtomicUsize::new(0));
        let b = Arc::new(AtomicUsize::new(0));
        let registry = registry_with_two_h2_plugins(a.clone(), b.clone());
        let ctx = crate::PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "anthropic_messages/test-model".to_string(),
        };
        let out = registry
            .run_on_decoded_request((FrontendKind::AnthropicMessages, "test-model"), &ctx, req())
            .await
            .expect("run_on_decoded_request must succeed for both plugins");
        assert_eq!(a.load(Ordering::SeqCst), 1, "plugin a fired once");
        assert_eq!(b.load(Ordering::SeqCst), 1, "plugin b fired once");
        // Each plugin appended one message; original had 1, expect 3.
        assert_eq!(
            out.messages.len(),
            3,
            "both plugin marker messages survived"
        );
    }

    #[tokio::test]
    async fn run_on_decoded_request_empty_registry_is_identity() {
        let registry = PluginRegistry::empty();
        let ctx = crate::PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x/y".to_string(),
        };
        let original = req();
        let original_len = original.messages.len();
        let out = registry
            .run_on_decoded_request(
                (FrontendKind::AnthropicMessages, "anything"),
                &ctx,
                original,
            )
            .await
            .expect("empty registry fast path returns Ok");
        assert_eq!(out.messages.len(), original_len, "request was untouched");
    }
}
