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

use agent_shim_core::{CanonicalRequest, FrontendKind};

use crate::context::PluginContext;
use crate::error::{OnError, PluginConfigError, PluginResult};
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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

    /// H2 runner: execute the `on_decoded_request` hook for every
    /// plugin subscribed to the `(frontend, model)` route, in order.
    /// Returns the final rewritten `CanonicalRequest`.
    ///
    /// Fast path: returns identity when no plan exists for the route
    /// or the plan's `on_decoded_request` list is empty.
    pub async fn run_on_decoded_request(
        &self,
        route: (FrontendKind, &str),
        ctx: &PluginContext,
        mut req: CanonicalRequest,
    ) -> PluginResult<CanonicalRequest> {
        let Some(plan) = self.lookup(route.0, route.1) else {
            return Ok(req);
        };
        if plan.on_decoded_request.is_empty() {
            return Ok(req);
        }
        let hook = Hook::DecodedRequest.as_str();
        for entry in &plan.on_decoded_request {
            if !entry.enabled {
                continue;
            }
            let plugin_name = entry.name.clone();
            let plugin = Arc::clone(&entry.plugin);
            let candidate = req.clone();
            let ctx_clone = ctx.clone();
            let outcome = crate::invoke::invoke(
                &plugin_name,
                &ctx_clone.clone(),
                hook,
                entry.timeouts.for_hook(Hook::DecodedRequest),
                entry.on_error,
                async move { plugin.on_decoded_request(&ctx_clone, candidate).await },
            )
            .await;
            match outcome {
                crate::invoke::InvokeOutcome::Success(updated) => {
                    match crate::invoke::check_protected_fields(&plugin_name, hook, &req, &updated)
                    {
                        Ok(()) => req = updated,
                        Err(e) => match entry.on_error {
                            OnError::Skip => { /* keep req */ }
                            OnError::Fail => return Err(e),
                        },
                    }
                }
                crate::invoke::InvokeOutcome::Skipped => {}
                crate::invoke::InvokeOutcome::Propagate(err) => return Err(err),
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::trait_def::HookSet;
    use agent_shim_core::{
        request::RequestMetadata, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel,
        GenerationOptions, RequestId, ResolvedPolicy,
    };

    fn req() -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: FrontendModel::from("m"),
            },
            model: FrontendModel::from("m"),
            system: vec![],
            messages: vec![],
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

    #[tokio::test]
    async fn run_on_decoded_request_empty_registry_is_identity() {
        let registry = PluginRegistry::empty();
        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "test/m".to_string(),
        };
        let original = req();
        let result = registry
            .run_on_decoded_request(
                (FrontendKind::AnthropicMessages, "m"),
                &ctx,
                original.clone(),
            )
            .await
            .unwrap();
        assert_eq!(result.id, original.id);
    }

    #[tokio::test]
    async fn run_on_decoded_request_fires_two_plugins_in_order() {
        // Counter plugin: increments shared AtomicUsize and records call order.
        struct CountingPlugin {
            counter: Arc<AtomicUsize>,
            my_order: usize,
            order_log: Arc<std::sync::Mutex<Vec<usize>>>,
        }

        #[async_trait]
        impl Plugin for CountingPlugin {
            fn kind_name(&self) -> &'static str {
                "counting"
            }
            fn hooks(&self) -> HookSet {
                HookSet::DECODED_REQUEST
            }
            async fn on_decoded_request(
                &self,
                _ctx: &PluginContext,
                req: CanonicalRequest,
            ) -> PluginResult<CanonicalRequest> {
                self.counter.fetch_add(1, Ordering::SeqCst);
                self.order_log.lock().unwrap().push(self.my_order);
                Ok(req)
            }
        }

        let counter = Arc::new(AtomicUsize::new(0));
        let order_log: Arc<std::sync::Mutex<Vec<usize>>> = Arc::new(std::sync::Mutex::new(vec![]));

        let plugin_a = Arc::new(CountingPlugin {
            counter: Arc::clone(&counter),
            my_order: 0,
            order_log: Arc::clone(&order_log),
        });
        let plugin_b = Arc::new(CountingPlugin {
            counter: Arc::clone(&counter),
            my_order: 1,
            order_log: Arc::clone(&order_log),
        });

        let entry_a = Arc::new(PluginEntry {
            name: "a".to_string(),
            plugin: plugin_a as Arc<dyn Plugin>,
            on_error: OnError::Fail,
            timeouts: HookTimeouts::default(),
            enabled: true,
        });
        let entry_b = Arc::new(PluginEntry {
            name: "b".to_string(),
            plugin: plugin_b as Arc<dyn Plugin>,
            on_error: OnError::Fail,
            timeouts: HookTimeouts::default(),
            enabled: true,
        });

        let plan = RouteHookPlan {
            on_decoded_request: vec![Arc::clone(&entry_a), Arc::clone(&entry_b)],
            ..Default::default()
        };

        let mut specific = HashMap::new();
        specific.insert("m".to_string(), plan);

        let fp = FrontendRoutePlans {
            specific,
            wildcard: None,
            is_empty: false,
        };

        let mut plans = HashMap::new();
        plans.insert(FrontendKind::AnthropicMessages, fp);

        let registry = PluginRegistry {
            plugins: HashMap::new(),
            plans,
        };

        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "test/m".to_string(),
        };

        registry
            .run_on_decoded_request((FrontendKind::AnthropicMessages, "m"), &ctx, req())
            .await
            .unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 2);
        assert_eq!(*order_log.lock().unwrap(), vec![0, 1]);
    }
}
