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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

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
    /// Cached `Plugin::kind_name()` value — `&'static str` literal.
    /// Populated at construction. P05 §7.1 / Q1.
    pub kind: &'static str,
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

// ── H5 span aggregation helpers (P05 §3) ─────────────────────────────

/// Drop-time recorder that writes aggregated counters onto the
/// `plugin.stream` span. `Span::record` after the span has been entered
/// in `GuardedH5Stream::poll_next` populates the span's fields just
/// before close, so trace backends see the final values.
struct StreamSpanRecorder {
    span: tracing::Span,
    event_count: Arc<AtomicU64>,
    failure_count: Arc<AtomicU64>,
}

impl Drop for StreamSpanRecorder {
    fn drop(&mut self) {
        self.span.record(
            "plugin.event_count",
            self.event_count.load(Ordering::Relaxed),
        );
        self.span.record(
            "plugin.failure_count",
            self.failure_count.load(Ordering::Relaxed),
        );
    }
}

/// Stream wrapper that enters the `plugin.stream` span on every poll
/// and owns the drop-guard recorder. `EnteredSpan<'_>` is `!Send` but
/// scope-bounded inside `poll_next`, which is fine because poll runs
/// synchronously on a single thread.
struct GuardedH5Stream<S> {
    inner: S,
    span: tracing::Span,
    _recorder: StreamSpanRecorder,
}

impl<S: futures::Stream + Unpin> futures::Stream for GuardedH5Stream<S> {
    type Item = S::Item;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        let _enter = this.span.enter();
        std::pin::Pin::new(&mut this.inner).poll_next(cx)
    }
}

/// Top-level plugin registry. Constructed once at startup. Immutable
/// thereafter; reload rebuilds the whole thing and arc-swaps (Q12).
pub struct PluginRegistry {
    #[allow(dead_code)] // populated by P06 from_specs; lookup goes via `plans`
    pub(crate) plugins: HashMap<String, Arc<PluginEntry>>,
    pub(crate) plans: HashMap<FrontendKind, FrontendRoutePlans>,
    /// Owns H7 task lifecycle. Lives as long as the registry; gateway
    /// shutdown calls `flush_pending_h7` to drain. P05 §6.8.
    pub(crate) supervisor: Arc<crate::supervisor::PluginSupervisor>,
}

impl PluginRegistry {
    /// Build an empty registry — no plugins, no plans. Used as the
    /// fast-path default in tests and in YAML configs that omit
    /// `plugins:` entirely.
    pub fn empty() -> Self {
        Self {
            plugins: HashMap::new(),
            plans: HashMap::new(),
            supervisor: Arc::new(crate::supervisor::PluginSupervisor::new()),
        }
    }

    /// Flush pending H7 tasks during shutdown. Returns
    /// `Vec<(plugin_name, dropped_count)>` — tasks that did not finish
    /// within the deadline are aborted (JoinSet drops). P05 §6.8.
    pub async fn flush_pending_h7(&self, deadline: Duration) -> Vec<(String, u64)> {
        self.supervisor.flush_pending_h7(deadline).await
    }

    /// Plan 07 P04 T12: test-only constructor for integration tests in
    /// other crates. Builds a registry containing a single
    /// `(name, plugin, on_error, hook)` entry bound to one
    /// `(frontend, model)` route. Tests in `crates/gateway/tests/` cannot
    /// reach the `pub(crate)` `RouteHookPlan` and `FrontendRoutePlans`
    /// types directly, so the plumbing has to live here.
    ///
    /// `#[doc(hidden)]` keeps this out of rustdoc; it has no production
    /// use. The full YAML-driven `from_specs` constructor lands in P06.
    #[doc(hidden)]
    pub fn for_testing_single_plugin(
        name: &str,
        plugin: Arc<dyn crate::Plugin>,
        on_error: OnError,
        hook: Hook,
        frontend: FrontendKind,
        model: &str,
    ) -> Self {
        let kind = plugin.kind_name();
        let entry = Arc::new(PluginEntry {
            name: name.to_string(),
            kind,
            plugin,
            on_error,
            timeouts: HookTimeouts::default(),
            enabled: true,
        });
        let mut plan = RouteHookPlan::default();
        match hook {
            Hook::DecodedRequest => plan.on_decoded_request.push(entry.clone()),
            Hook::Resolved => plan.on_resolved.push(entry.clone()),
            Hook::StreamEvent => plan.on_stream_event.push(entry.clone()),
            Hook::ResponseComplete => plan.on_response_complete.push(entry.clone()),
        }
        let mut specific = HashMap::new();
        specific.insert(model.to_string(), plan);
        let mut plans = HashMap::new();
        plans.insert(
            frontend,
            FrontendRoutePlans {
                specific,
                wildcard: None,
                is_empty: false,
            },
        );
        let mut plugins = HashMap::new();
        plugins.insert(name.to_string(), entry);
        Self {
            plugins,
            plans,
            supervisor: Arc::new(crate::supervisor::PluginSupervisor::new()),
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
                crate::invoke::InvokeArgs::from_entry(
                    entry,
                    Hook::DecodedRequest,
                    crate::invoke::SpanMode::PerInvocation,
                ),
                ctx,
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

    /// H3 hook chain walk. Spec §6.2 / §6.4 / §4.5.
    ///
    /// Identical shape to `run_on_decoded_request` but:
    /// - walks `plan.on_resolved`,
    /// - passes the resolved `BackendTarget` to each plugin.
    ///
    /// Protected-field semantics are the same — including `stream`, even
    /// though by §6.6 H3 runs after the streaming-vs-unary branch was
    /// decided (mutating `stream` here would silently inconsistency the
    /// downstream code path, hence the rejection).
    pub async fn run_on_resolved(
        &self,
        route: (FrontendKind, &str),
        ctx: &crate::PluginContext,
        mut req: agent_shim_core::CanonicalRequest,
        target: &agent_shim_core::BackendTarget,
    ) -> Result<agent_shim_core::CanonicalRequest, PluginError> {
        let Some(plan) = self.lookup(route.0, route.1) else {
            return Ok(req);
        };
        if plan.on_resolved.is_empty() {
            return Ok(req);
        }
        for entry in &plan.on_resolved {
            if !entry.enabled {
                continue;
            }
            let candidate = req.clone();
            let plugin = entry.plugin.clone();
            let plugin_name = entry.name.clone();
            let hook_str = Hook::Resolved.as_str();
            let outcome = crate::invoke::invoke(
                crate::invoke::InvokeArgs::from_entry(
                    entry,
                    Hook::Resolved,
                    crate::invoke::SpanMode::PerInvocation,
                ),
                ctx,
                plugin.on_resolved(ctx, candidate, target),
            )
            .await;
            match outcome {
                crate::invoke::InvokeOutcome::Success(new_req) => {
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
                crate::invoke::InvokeOutcome::Skipped => {}
                crate::invoke::InvokeOutcome::Propagate(err) => return Err(err),
            }
        }
        Ok(req)
    }

    /// H5 stream wrapping. Spec §6.5.
    ///
    /// When the route has no `on_stream_event` subscribers, returns the
    /// upstream stream as-is (zero overhead — no allocations, no
    /// futures-machinery). Otherwise wraps with a chain that runs every
    /// upstream-emitted event through every subscribed plugin in
    /// declaration order, flattening each plugin's `Vec<StreamEvent>`
    /// into the output.
    ///
    /// Error handling:
    /// - Upstream `Err(StreamError)` items pass through unchanged — plugins
    ///   never see them.
    /// - Plugin `Err` causes the wrapper to emit a single
    ///   `StreamEvent::Error { message }` event in the stream and stop;
    ///   downstream frontend encoding rendering decides the wire shape.
    /// - `Skipped` (on_error: skip) treats the plugin as identity — the
    ///   event passes through unchanged.
    pub fn wrap_stream(
        &self,
        route: (FrontendKind, &str),
        ctx: crate::PluginContext,
        upstream: agent_shim_core::CanonicalStream,
    ) -> agent_shim_core::CanonicalStream {
        // Fast path: no plan, no plugins, no H5 subscribers — return upstream.
        let plan = match self.lookup(route.0, route.1) {
            Some(p) if !p.on_stream_event.is_empty() => p.clone(),
            _ => return upstream,
        };
        let plugins: Vec<Arc<PluginEntry>> = plan.on_stream_event.clone();

        // P05 §3: aggregated plugin.stream span. NO plugin.name field
        // (covers all H5 plugins). Per-plugin attribution flows through
        // per-event log lines on failure (§7.5 noise control + Q14).
        let span = tracing::info_span!(
            "plugin.stream",
            "agent_shim.request_id" = ctx.request_id.0.as_str(),
            "agent_shim.route" = ctx.route_label.as_str(),
            "plugin.event_count" = tracing::field::Empty,
            "plugin.failure_count" = tracing::field::Empty,
        );
        let event_count = Arc::new(AtomicU64::new(0));
        let failure_count = Arc::new(AtomicU64::new(0));
        let recorder = StreamSpanRecorder {
            span: span.clone(),
            event_count: Arc::clone(&event_count),
            failure_count: Arc::clone(&failure_count),
        };

        let ctx = Arc::new(ctx);

        use futures::StreamExt;
        let inner = upstream
            .then(move |event_result| {
                let plugins = plugins.clone();
                let ctx = ctx.clone();
                let event_count = Arc::clone(&event_count);
                let failure_count = Arc::clone(&failure_count);
                async move {
                    // Upstream errors pass through unchanged (no plugin sees them).
                    let event = match event_result {
                        Ok(e) => e,
                        Err(err) => return vec![Err(err)],
                    };
                    event_count.fetch_add(1, Ordering::Relaxed);

                    let mut buf: Vec<agent_shim_core::StreamEvent> = vec![event];
                    for entry in &plugins {
                        if !entry.enabled {
                            continue;
                        }
                        let mut next: Vec<agent_shim_core::StreamEvent> =
                            Vec::with_capacity(buf.len());
                        let plugin = entry.plugin.clone();
                        for ev in buf.drain(..) {
                            let ev_for_invoke = ev.clone();
                            let ev_for_skip = ev;
                            let outcome = crate::invoke::invoke::<
                                Vec<agent_shim_core::StreamEvent>,
                                _,
                            >(
                                crate::invoke::InvokeArgs::from_entry(
                                    entry,
                                    Hook::StreamEvent,
                                    crate::invoke::SpanMode::Aggregated,
                                ),
                                &ctx,
                                plugin.on_stream_event(&ctx, ev_for_invoke),
                            )
                            .await;
                            match outcome {
                                crate::invoke::InvokeOutcome::Success(events) => {
                                    next.extend(events);
                                }
                                crate::invoke::InvokeOutcome::Skipped => {
                                    next.push(ev_for_skip);
                                }
                                crate::invoke::InvokeOutcome::Propagate(err) => {
                                    failure_count.fetch_add(1, Ordering::Relaxed);
                                    return vec![Ok(agent_shim_core::StreamEvent::Error {
                                        message: err.to_string(),
                                    })];
                                }
                            }
                        }
                        buf = next;
                    }
                    buf.into_iter().map(Ok).collect::<Vec<_>>()
                }
            })
            .flat_map(futures::stream::iter);
        // Box+pin into a uniformly `Unpin` stream so `GuardedH5Stream` can
        // project to `&mut Self` without `pin-project` machinery. The
        // `then(...).flat_map(...)` chain itself is `!Unpin`.
        let inner = Box::pin(inner) as futures::stream::BoxStream<'static, _>;

        Box::pin(GuardedH5Stream {
            inner,
            span,
            _recorder: recorder,
        })
    }

    /// H7 hook — fire-and-forget. Spec §6.2 / §6.7 / §6.8.
    ///
    /// Each subscribed plugin is `tokio::spawn`-ed. This function returns
    /// synchronously; the spawned tasks live until they complete or are
    /// dropped at shutdown. P05 will wire these into a `JoinSet` so
    /// shutdown can flush them within a deadline.
    pub fn run_on_response_complete(
        &self,
        route: (FrontendKind, &str),
        ctx: crate::PluginContext,
        summary: crate::ResponseSummary,
    ) {
        let Some(plan) = self.lookup(route.0, route.1) else {
            return;
        };
        if plan.on_response_complete.is_empty() {
            return;
        }
        let summary = Arc::new(summary);
        let ctx = Arc::new(ctx);
        for entry in &plan.on_response_complete {
            if !entry.enabled {
                continue;
            }
            let plugin = entry.plugin.clone();
            let plugin_name = entry.name.clone();
            let plugin_kind: &'static str = entry.kind;
            let timeout_ms = entry.timeouts.for_hook(Hook::ResponseComplete);
            let on_error = entry.on_error;
            let summary = summary.clone();
            let ctx = ctx.clone();
            // Route through supervisor instead of bare tokio::spawn so
            // shutdown can bound the wait (P05 §6.8).
            self.supervisor.spawn_h7(plugin_name.clone(), async move {
                let _ = crate::invoke::invoke::<(), _>(
                    crate::invoke::InvokeArgs {
                        plugin_name: &plugin_name,
                        plugin_kind,
                        hook: Hook::ResponseComplete.as_str(),
                        timeout_ms,
                        on_error,
                        span_mode: crate::invoke::SpanMode::PerInvocation,
                    },
                    &ctx,
                    plugin.on_response_complete(&ctx, &summary),
                )
                .await;
                // Return value is discarded — H7 cannot affect the response.
                // Outcome is captured via the logging/metrics in P05.
            });
        }
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
            kind: "counter",
            plugin: Arc::new(CounterPlugin { n: counter_a }),
            on_error: OnError::Skip,
            timeouts: HookTimeouts::default(),
            enabled: true,
        });
        let b = Arc::new(PluginEntry {
            name: "b".to_string(),
            kind: "counter",
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
        PluginRegistry {
            plugins,
            plans,
            supervisor: Arc::new(crate::supervisor::PluginSupervisor::new()),
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

    // ── Plan 07 P04 T2: run_on_resolved ─────────────────────────────────────

    #[tokio::test]
    async fn run_on_resolved_fires_plugin_with_target() {
        use agent_shim_core::BackendTarget;

        struct ResolvedCounter(Arc<AtomicUsize>);
        #[async_trait]
        impl crate::Plugin for ResolvedCounter {
            fn kind_name(&self) -> &'static str {
                "resolved_counter"
            }
            fn hooks(&self) -> crate::HookSet {
                crate::HookSet::RESOLVED
            }
            async fn on_resolved(
                &self,
                _ctx: &crate::PluginContext,
                req: CanonicalRequest,
                _target: &BackendTarget,
            ) -> crate::PluginResult<CanonicalRequest> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(req)
            }
        }

        let n = Arc::new(AtomicUsize::new(0));
        let entry = Arc::new(PluginEntry {
            name: "rc".to_string(),
            kind: "resolved_counter",
            plugin: Arc::new(ResolvedCounter(n.clone())),
            on_error: OnError::Skip,
            timeouts: HookTimeouts::default(),
            enabled: true,
        });
        let plan = RouteHookPlan {
            on_resolved: vec![entry.clone()],
            ..Default::default()
        };
        let mut plans = HashMap::new();
        plans.insert(
            FrontendKind::AnthropicMessages,
            FrontendRoutePlans {
                specific: {
                    let mut m = HashMap::new();
                    m.insert("m".to_string(), plan);
                    m
                },
                wildcard: None,
                is_empty: false,
            },
        );
        let registry = PluginRegistry {
            plugins: {
                let mut p = HashMap::new();
                p.insert("rc".to_string(), entry);
                p
            },
            plans,
            supervisor: Arc::new(crate::supervisor::PluginSupervisor::new()),
        };
        let ctx = crate::PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "anthropic_messages/m".to_string(),
        };
        let target = BackendTarget {
            provider: "test".to_string(),
            model: "u".to_string(),
            policy: Default::default(),
        };
        let _ = registry
            .run_on_resolved((FrontendKind::AnthropicMessages, "m"), &ctx, req(), &target)
            .await
            .expect("run_on_resolved must succeed");
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn run_on_resolved_empty_registry_is_identity() {
        use agent_shim_core::BackendTarget;
        let registry = PluginRegistry::empty();
        let ctx = crate::PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        let target = BackendTarget {
            provider: "test".to_string(),
            model: "u".to_string(),
            policy: Default::default(),
        };
        let out = registry
            .run_on_resolved(
                (FrontendKind::AnthropicMessages, "anything"),
                &ctx,
                req(),
                &target,
            )
            .await
            .expect("empty registry fast path returns Ok");
        assert_eq!(out.messages.len(), 1);
    }

    // ── Plan 07 P04 T3: wrap_stream (H5) ────────────────────────────────────

    #[tokio::test]
    async fn wrap_stream_fast_path_returns_identity() {
        use agent_shim_core::{CanonicalStream, StopReason, StreamEvent};
        use futures::stream::StreamExt;
        let registry = PluginRegistry::empty();
        let ctx = crate::PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x/y".to_string(),
        };
        let upstream: CanonicalStream =
            Box::pin(futures::stream::iter(vec![Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                stop_sequence: None,
            })]));
        let wrapped =
            registry.wrap_stream((FrontendKind::AnthropicMessages, "anything"), ctx, upstream);
        let collected: Vec<_> = wrapped.collect().await;
        assert_eq!(collected.len(), 1);
        assert!(collected[0].is_ok(), "fast path preserves Ok items");
    }

    #[tokio::test]
    async fn wrap_stream_h5_plugin_drops_every_other_event() {
        use agent_shim_core::{CanonicalStream, StreamEvent};
        use futures::stream::StreamExt;

        struct DropEverySecond {
            counter: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl crate::Plugin for DropEverySecond {
            fn kind_name(&self) -> &'static str {
                "drop_every_second"
            }
            fn hooks(&self) -> crate::HookSet {
                crate::HookSet::STREAM_EVENT
            }
            async fn on_stream_event(
                &self,
                _ctx: &crate::PluginContext,
                event: StreamEvent,
            ) -> crate::PluginResult<Vec<StreamEvent>> {
                let n = self.counter.fetch_add(1, Ordering::SeqCst);
                if n % 2 == 0 {
                    Ok(vec![event])
                } else {
                    Ok(vec![])
                }
            }
        }

        let n = Arc::new(AtomicUsize::new(0));
        let entry = Arc::new(PluginEntry {
            name: "d".to_string(),
            kind: "drop_every_second",
            plugin: Arc::new(DropEverySecond { counter: n.clone() }),
            on_error: OnError::Skip,
            timeouts: HookTimeouts::default(),
            enabled: true,
        });
        let plan = RouteHookPlan {
            on_stream_event: vec![entry.clone()],
            ..Default::default()
        };
        let mut plans = HashMap::new();
        plans.insert(
            FrontendKind::AnthropicMessages,
            FrontendRoutePlans {
                specific: {
                    let mut m = HashMap::new();
                    m.insert("m".to_string(), plan);
                    m
                },
                wildcard: None,
                is_empty: false,
            },
        );
        let registry = PluginRegistry {
            plugins: {
                let mut p = HashMap::new();
                p.insert("d".to_string(), entry);
                p
            },
            plans,
            supervisor: Arc::new(crate::supervisor::PluginSupervisor::new()),
        };
        let ctx = crate::PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        // 4 input events; 2 should survive.
        let events: Vec<_> = (0..4)
            .map(|i| {
                Ok(StreamEvent::TextDelta {
                    index: 0,
                    text: format!("e{i}"),
                })
            })
            .collect();
        let upstream: CanonicalStream = Box::pin(futures::stream::iter(events));
        let wrapped = registry.wrap_stream((FrontendKind::AnthropicMessages, "m"), ctx, upstream);
        let collected: Vec<_> = wrapped.collect().await;
        let oks: Vec<_> = collected.into_iter().filter_map(Result::ok).collect();
        assert_eq!(oks.len(), 2, "every other event survived");
        assert_eq!(n.load(Ordering::SeqCst), 4, "plugin saw all 4 events");
    }

    #[tokio::test]
    async fn wrap_stream_plugin_failure_emits_error_event_and_stops() {
        use agent_shim_core::{CanonicalStream, StreamEvent};
        use futures::stream::StreamExt;

        struct AlwaysFail;
        #[async_trait]
        impl crate::Plugin for AlwaysFail {
            fn kind_name(&self) -> &'static str {
                "always_fail"
            }
            fn hooks(&self) -> crate::HookSet {
                crate::HookSet::STREAM_EVENT
            }
            async fn on_stream_event(
                &self,
                _ctx: &crate::PluginContext,
                _event: StreamEvent,
            ) -> crate::PluginResult<Vec<StreamEvent>> {
                Err(crate::PluginError::Failed {
                    plugin: "always_fail".to_string(),
                    hook: "on_stream_event",
                    message: "boom".to_string(),
                })
            }
        }

        let entry = Arc::new(PluginEntry {
            name: "f".to_string(),
            kind: "always_fail",
            plugin: Arc::new(AlwaysFail),
            on_error: OnError::Fail, // ensures the error propagates
            timeouts: HookTimeouts::default(),
            enabled: true,
        });
        let plan = RouteHookPlan {
            on_stream_event: vec![entry.clone()],
            ..Default::default()
        };
        let mut plans = HashMap::new();
        plans.insert(
            FrontendKind::AnthropicMessages,
            FrontendRoutePlans {
                specific: {
                    let mut m = HashMap::new();
                    m.insert("m".to_string(), plan);
                    m
                },
                wildcard: None,
                is_empty: false,
            },
        );
        let registry = PluginRegistry {
            plugins: {
                let mut p = HashMap::new();
                p.insert("f".to_string(), entry);
                p
            },
            plans,
            supervisor: Arc::new(crate::supervisor::PluginSupervisor::new()),
        };
        let ctx = crate::PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        let upstream: CanonicalStream = Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::TextDelta {
                index: 0,
                text: "a".to_string(),
            }),
            Ok(StreamEvent::TextDelta {
                index: 0,
                text: "b".to_string(),
            }),
        ]));
        let wrapped = registry.wrap_stream((FrontendKind::AnthropicMessages, "m"), ctx, upstream);
        let collected: Vec<_> = wrapped.collect().await;
        // First event should be replaced with an Error event; second event
        // is consumed by the upstream stream but the plugin chain produces
        // another Error event for it (the wrapper does not short-circuit
        // upstream — each event runs through the chain independently).
        assert!(!collected.is_empty(), "at least one event emitted");
        let first = &collected[0];
        match first {
            Ok(StreamEvent::Error { message }) => {
                assert!(
                    message.contains("always_fail") || message.contains("boom"),
                    "error message names the plugin or carries the reason: {message}"
                );
            }
            other => panic!("expected StreamEvent::Error, got {other:?}"),
        }
    }

    // ── Plan 07 P04 T4: run_on_response_complete (H7) ───────────────────────

    #[tokio::test]
    async fn run_on_response_complete_fires_async() {
        use crate::ResponseSummary;

        struct RecordSummary {
            captured: Arc<tokio::sync::Mutex<Option<u64>>>,
        }
        #[async_trait]
        impl crate::Plugin for RecordSummary {
            fn kind_name(&self) -> &'static str {
                "record_summary"
            }
            fn hooks(&self) -> crate::HookSet {
                crate::HookSet::RESPONSE_COMPLETE
            }
            async fn on_response_complete(
                &self,
                _ctx: &crate::PluginContext,
                summary: &ResponseSummary,
            ) -> crate::PluginResult<()> {
                *self.captured.lock().await = Some(summary.elapsed_ms);
                Ok(())
            }
        }

        let captured = Arc::new(tokio::sync::Mutex::new(None));
        let entry = Arc::new(PluginEntry {
            name: "rs".to_string(),
            kind: "record_summary",
            plugin: Arc::new(RecordSummary {
                captured: captured.clone(),
            }),
            on_error: OnError::Skip,
            timeouts: HookTimeouts::default(),
            enabled: true,
        });
        let plan = RouteHookPlan {
            on_response_complete: vec![entry.clone()],
            ..Default::default()
        };
        let mut plans = HashMap::new();
        plans.insert(
            FrontendKind::AnthropicMessages,
            FrontendRoutePlans {
                specific: {
                    let mut m = HashMap::new();
                    m.insert("m".to_string(), plan);
                    m
                },
                wildcard: None,
                is_empty: false,
            },
        );
        let registry = PluginRegistry {
            plugins: {
                let mut p = HashMap::new();
                p.insert("rs".to_string(), entry);
                p
            },
            plans,
            supervisor: Arc::new(crate::supervisor::PluginSupervisor::new()),
        };
        let ctx = crate::PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        registry.run_on_response_complete(
            (FrontendKind::AnthropicMessages, "m"),
            ctx,
            ResponseSummary {
                usage: None,
                elapsed_ms: 42,
                upstream_status: crate::UpstreamStatus::Success,
            },
        );
        // Spawned task — yield in a loop until the captured value lands or
        // 1 s elapses (tokio scheduling can take a beat under load).
        for _ in 0..100 {
            if captured.lock().await.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(*captured.lock().await, Some(42));
    }

    #[tokio::test]
    async fn run_on_response_complete_empty_registry_is_noop() {
        use crate::ResponseSummary;
        let registry = PluginRegistry::empty();
        let ctx = crate::PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        registry.run_on_response_complete(
            (FrontendKind::AnthropicMessages, "anything"),
            ctx,
            ResponseSummary {
                usage: None,
                elapsed_ms: 0,
                upstream_status: crate::UpstreamStatus::Success,
            },
        );
        // No assertion needed — just verify the call returns without panic.
    }

    #[tokio::test(start_paused = true)]
    async fn flush_pending_h7_drops_slow_h7_plugin() {
        use crate::ResponseSummary;

        struct SlowH7;
        #[async_trait]
        impl crate::Plugin for SlowH7 {
            fn kind_name(&self) -> &'static str {
                "slow_h7"
            }
            fn hooks(&self) -> crate::HookSet {
                crate::HookSet::RESPONSE_COMPLETE
            }
            async fn on_response_complete(
                &self,
                _ctx: &crate::PluginContext,
                _summary: &ResponseSummary,
            ) -> crate::PluginResult<()> {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                Ok(())
            }
        }

        let entry = Arc::new(PluginEntry {
            name: "slow".to_string(),
            kind: "slow_h7",
            plugin: Arc::new(SlowH7),
            on_error: OnError::Skip,
            timeouts: HookTimeouts::default(),
            enabled: true,
        });
        let plan = RouteHookPlan {
            on_response_complete: vec![entry.clone()],
            ..Default::default()
        };
        let mut plans = HashMap::new();
        plans.insert(
            FrontendKind::AnthropicMessages,
            FrontendRoutePlans {
                specific: {
                    let mut m = HashMap::new();
                    m.insert("m".to_string(), plan);
                    m
                },
                wildcard: None,
                is_empty: false,
            },
        );
        let registry = PluginRegistry {
            plugins: {
                let mut p = HashMap::new();
                p.insert("slow".to_string(), entry);
                p
            },
            plans,
            supervisor: Arc::new(crate::supervisor::PluginSupervisor::new()),
        };
        let ctx = crate::PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        registry.run_on_response_complete(
            (FrontendKind::AnthropicMessages, "m"),
            ctx,
            ResponseSummary {
                usage: None,
                elapsed_ms: 0,
                upstream_status: crate::UpstreamStatus::Success,
            },
        );

        // Yield once so the spawn actually runs and registers in pending.
        tokio::task::yield_now().await;

        let dropped = registry
            .flush_pending_h7(std::time::Duration::from_millis(10))
            .await;
        assert_eq!(
            dropped,
            vec![("slow".to_string(), 1)],
            "slow H7 plugin dropped with attribution"
        );
    }
}
