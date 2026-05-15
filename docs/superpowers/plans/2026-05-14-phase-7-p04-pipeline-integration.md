# Plan P04 — Pipeline integration (Phase 7)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-14-plugin-system-design.md`](../specs/2026-05-14-plugin-system-design.md) §6.2, §6.4, §6.5, §6.6, §6.7, §6.9. [`ADR-0008`](../../adr/0008-plugin-system.md) decisions (3) and (4).

**Goal:** Add the four `PluginRegistry::run_*` / `wrap_stream` methods on top of P02's trait+registry, wire them into `pipeline.rs::dispatch_inner` at the four spec §6.6 anchor points, install a universal `H7Guard` covering all three streaming frontends, and add a `HandlerError::PluginFailed` variant with its 400/502 mapping. The empty-registry fast path stays byte-identical to the v0.6.1 baseline (verified by integration test using `PluginRegistry::empty()`).

**Architecture:** Method bodies live in `crates/plugins/src/registry.rs`, each driven by P02's existing `invoke()` template + clone-then-swap (§6.4) + the protected-field diff (§4.5). H5 (`wrap_stream`) uses `futures::StreamExt::then` + `flat_map` to splice 1-in-N-out, fast-pathing to identity when the route has no `on_stream_event` plugins (§6.5). H7 spawns each plugin onto a `tokio::spawn` (JoinSet wiring delayed to P05). The pipeline gains four call sites — anchored by code comments referencing this spec section, not line numbers — and a generalised `H7Guard` replaces today's Anthropic-only `StreamLogger`. `HandlerError::PluginFailed { kind, plugin, hook, aborted }` is added with envelope rendering in each of the three frontend `IntoResponse` branches.

**Tech stack:** existing — `tokio` (`time::timeout`, `task::spawn`), `futures` (`StreamExt::then`, `stream::iter::flat_map`), `async-trait`, `arc-swap`. No new dependencies.

**Frozen-core impact:** None. `crates/core/` is not touched. ADR-0007 discipline preserved.

**Test target:** Current workspace baseline (post-P03) is **745 passing**. This plan adds ~25 new tests across `crates/plugins/` unit tests (run_* and wrap_stream mocks) and `crates/gateway/tests/plugins_pipeline.rs` (integration: empty-registry parity, four-hook fire order, H7-on-drop-of-stream, PluginFailed → 400 / 502, ProtectedFieldMutated → 502, mid-stream SSE error frame). Target ~770 on completion.

**Subagent-driven workflow note:** This is the largest plan in Phase 7 by code surface. Each task is intentionally small and single-file-bounded so the subagent doesn't have to reason across the whole pipeline at once. Most pipeline edits land as separate tasks rather than as one mega-edit.

---

## File Structure

`crates/plugins/src/`:
- Modify: `registry.rs` — implement `run_on_decoded_request`, `run_on_resolved`, `wrap_stream`, `run_on_response_complete`. Drop the `#[allow(dead_code)]` attributes that P02 placed on the supporting machinery.
- Modify: `invoke.rs` — drop the `#[allow(dead_code)]` on `InvokeOutcome`, `invoke()`, `check_protected_fields` (they now have real callers).

`crates/gateway/src/`:
- Modify: `handlers/mod.rs` — add `HandlerError::PluginFailed { kind, plugin, hook, aborted }` variant + envelope-rendering branches in `IntoResponse`.
- Modify: `pipeline.rs` — four call sites for the registry hooks, generalised `H7Guard` replacing `StreamLogger`, and `handler_error_status_hint` branch for `PluginFailed`. The OpenAI Chat / OpenAI Responses streaming branches that previously did NOT wrap `upstream_stream` in a guard now wrap it in `H7Guard` so H7 fires uniformly across all three frontends (spec §6.7).
- Modify: `state.rs::AppCore` — add `pub plugins: Arc<agent_shim_plugins::PluginRegistry>` field, defaulted to `PluginRegistry::empty()` in `AppState::build`. (The hot-reload `arc-swap` of the registry lands in P07 — this plan just makes the slot exist.)

`crates/gateway/tests/` (new file):
- Create: `plugins_pipeline.rs` — integration tests: empty-registry byte-identical parity vs no-registry, four-hook firing order against a stub plugin, H7 fires after stream drains, PluginFailed → 502 / Aborted → 400 envelope assertions, protected-field mutation → 502, H5 mid-stream error → SSE `event: error` frame.

No changes to `crates/core/`. No changes to `crates/frontends/` or `crates/providers/`. No changes to `crates/router/` or `crates/config/`.

---

## Tasks

### Task 1: Implement `PluginRegistry::run_on_decoded_request`

**Files:**
- Modify: `crates/plugins/src/registry.rs`

- [ ] **Step 1: Drop the `#[allow(dead_code)]` attributes the method will start using**

Open `crates/plugins/src/registry.rs`. Remove the `#[allow(dead_code)] // used by P04 run_* methods` attributes on:
- `HookTimeouts::for_hook` (line ~52)
- `FrontendRoutePlans` (line ~76 — leave the field-level `#[allow(dead_code)] // fields populated by P03 from_specs constructor` for now; T1 only uses the type, not its fields directly)
- `RouteHookPlan` and its `is_empty` method
- `PluginRegistry.plugins`, `PluginRegistry.plans`
- `PluginRegistry::lookup`

The attribute family also exists on the `invoke()` template — leave it alone in this task (T1 only depends on the registry surface). T2 will run `invoke()` for the first time.

- [ ] **Step 2: Add the method signature + body**

Append to the `impl PluginRegistry` block (after `lookup`):

```rust
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
    /// integration test in T11.
    pub async fn run_on_decoded_request(
        &self,
        route: (agent_shim_core::FrontendKind, &str),
        ctx: &crate::PluginContext,
        mut req: agent_shim_core::CanonicalRequest,
    ) -> Result<agent_shim_core::CanonicalRequest, crate::PluginError> {
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
            let outcome = crate::invoke::invoke(
                &plugin_name,
                ctx,
                crate::Hook::DecodedRequest.as_str(),
                entry.timeouts.for_hook(crate::Hook::DecodedRequest),
                entry.on_error,
                async move { plugin.on_decoded_request(ctx, candidate).await },
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
                        crate::Hook::DecodedRequest.as_str(),
                        &req,
                        &new_req,
                    ) {
                        match entry.on_error {
                            crate::OnError::Skip => continue,
                            crate::OnError::Fail => return Err(e),
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
```

Note: the `async move { plugin.on_decoded_request(ctx, candidate).await }` future captures `ctx` by reference — which works because `invoke()` takes `&PluginContext` and the future is awaited immediately inside the same scope (no `'static` requirement). If the compiler complains about the `ctx` borrow because `async-trait` desugars to a boxed future, fall back to cloning the `PluginContext` once outside the loop (`PluginContext: Clone`).

- [ ] **Step 3: `pub(crate) fn` visibility for `InvokeOutcome` and `invoke()`**

The registry now uses `crate::invoke::invoke` and `crate::invoke::InvokeOutcome` from a sibling module. Verify the symbols are reachable:

```rust
// in crates/plugins/src/invoke.rs the current attributes are
#[allow(dead_code)] pub(crate) enum InvokeOutcome<T> { ... }
#[allow(dead_code)] pub(crate) async fn invoke<...>(...) -> InvokeOutcome<T> { ... }
#[allow(dead_code)] pub(crate) fn check_protected_fields(...) -> Result<(), PluginError> { ... }
```

The `#[allow(dead_code)]` attrs will become stale once we have callers; drop them in T2/T6 alongside their respective first uses. T1 only needs the symbols to compile-resolve.

- [ ] **Step 4: Build + workspace check**

Run: `rtk cargo check -p agent-shim-plugins`
Expected: clean. If the implicit `ctx` borrow across the await in `async move { ... }` triggers a lifetime error, switch to:
```rust
let ctx_owned = ctx.clone(); // PluginContext: Clone (T1 in P02)
... async move { plugin.on_decoded_request(&ctx_owned, candidate).await }
```
and use `&ctx_owned` in the surrounding `invoke()` call as well.

- [ ] **Step 5: Add a unit test using a stub plugin**

Append inside `mod tests` at the bottom of `registry.rs`:

```rust
    use agent_shim_core::{
        request::RequestMetadata, BackendTarget, CanonicalRequest, ContentBlock,
        ExtensionMap, FrontendInfo, FrontendModel, GenerationOptions, Message,
        MessageRole, RequestId, ResolvedPolicy, StreamEvent,
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
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text("hello".to_string())],
            }],
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

    struct CounterPlugin {
        n: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl crate::Plugin for CounterPlugin {
        fn kind_name(&self) -> &'static str { "counter" }
        fn hooks(&self) -> crate::HookSet { crate::HookSet::DECODED_REQUEST }
        async fn on_decoded_request(
            &self,
            _ctx: &crate::PluginContext,
            mut req: CanonicalRequest,
        ) -> crate::PluginResult<CanonicalRequest> {
            self.n.fetch_add(1, Ordering::SeqCst);
            // Push a marker message so order is verifiable.
            req.messages.push(Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text("counter".to_string())],
            });
            Ok(req)
        }
    }

    fn registry_with_two_h2_plugins(counter_a: Arc<AtomicUsize>, counter_b: Arc<AtomicUsize>)
        -> PluginRegistry
    {
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
            .run_on_decoded_request(
                (FrontendKind::AnthropicMessages, "test-model"),
                &ctx,
                req(),
            )
            .await
            .expect("ok");
        assert_eq!(a.load(Ordering::SeqCst), 1);
        assert_eq!(b.load(Ordering::SeqCst), 1);
        // Each plugin appended one message; original had 1, expect 3.
        assert_eq!(out.messages.len(), 3);
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
        let out = registry
            .run_on_decoded_request(
                (FrontendKind::AnthropicMessages, "anything"),
                &ctx,
                original.clone(),
            )
            .await
            .expect("ok");
        assert_eq!(out.messages.len(), original.messages.len());
    }
```

Imports added at top of `mod tests`:

```rust
    use super::*;
    use crate::OnError; // already in scope through earlier tests; verify
```

If `RouteHookPlan` and `FrontendRoutePlans` need to be `pub(crate)`-but-constructable-from-tests, the inner-module `#[cfg(test)]` block has visibility access already since it's defined inside the same crate root module path.

- [ ] **Step 6: Run tests + clippy + fmt**

```
rtk cargo nextest run -p agent-shim-plugins
rtk cargo clippy -p agent-shim-plugins --all-targets -- -D warnings
rtk cargo fmt -p agent-shim-plugins -- --check
```

Expected: all green.

- [ ] **Step 7: Commit**

```bash
git add crates/plugins/src/registry.rs crates/plugins/src/invoke.rs
git commit -m "feat(plugins): PluginRegistry::run_on_decoded_request (P04 T1)"
```

---

### Task 2: Implement `PluginRegistry::run_on_resolved`

**Files:**
- Modify: `crates/plugins/src/registry.rs`

- [ ] **Step 1: Add the method**

`run_on_resolved` is structurally identical to `run_on_decoded_request` — same clone-then-swap, same protected-field check — except it takes an extra `target: &BackendTarget` argument and walks `plan.on_resolved`.

Append to the `impl PluginRegistry` block (after `run_on_decoded_request`):

```rust
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
        route: (agent_shim_core::FrontendKind, &str),
        ctx: &crate::PluginContext,
        mut req: agent_shim_core::CanonicalRequest,
        target: &agent_shim_core::BackendTarget,
    ) -> Result<agent_shim_core::CanonicalRequest, crate::PluginError> {
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
            let target_owned = target.clone(); // BackendTarget: Clone (verify)
            let outcome = crate::invoke::invoke(
                &plugin_name,
                ctx,
                crate::Hook::Resolved.as_str(),
                entry.timeouts.for_hook(crate::Hook::Resolved),
                entry.on_error,
                async move { plugin.on_resolved(ctx, candidate, &target_owned).await },
            )
            .await;
            match outcome {
                crate::invoke::InvokeOutcome::Success(new_req) => {
                    if let Err(e) = crate::invoke::check_protected_fields(
                        &plugin_name,
                        crate::Hook::Resolved.as_str(),
                        &req,
                        &new_req,
                    ) {
                        match entry.on_error {
                            crate::OnError::Skip => continue,
                            crate::OnError::Fail => return Err(e),
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
```

- [ ] **Step 2: Verify `BackendTarget: Clone`**

Run: `rtk grep -n "#\[derive.*Clone\|impl Clone for BackendTarget\|pub struct BackendTarget" crates/core/src/ --include='*.rs'`
Expected: `BackendTarget` has `#[derive(Clone)]`. If not, fall back to `let target_ptr: &BackendTarget = target;` and use it directly in the future — `BackendTarget` then needs to outlive the future, which the borrow naturally provides via the `'_` lifetime on `&self`.

The plain `target.clone()` path is preferred because the future is `async move`. If `BackendTarget` isn't `Clone`, see the fallback above.

- [ ] **Step 3: Unit test**

Add inside `mod tests`:

```rust
    #[tokio::test]
    async fn run_on_resolved_fires_plugin_with_target() {
        // Reuse CounterPlugin from T1 but subscribe to RESOLVED instead.
        struct ResolvedCounter(Arc<AtomicUsize>);
        #[async_trait]
        impl crate::Plugin for ResolvedCounter {
            fn kind_name(&self) -> &'static str { "resolved_counter" }
            fn hooks(&self) -> crate::HookSet { crate::HookSet::RESOLVED }
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
                specific: { let mut m = HashMap::new(); m.insert("m".to_string(), plan); m },
                wildcard: None,
                is_empty: false,
            },
        );
        let registry = PluginRegistry {
            plugins: { let mut p = HashMap::new(); p.insert("rc".to_string(), entry); p },
            plans,
        };
        let ctx = crate::PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "anthropic_messages/m".to_string(),
        };
        let target = BackendTarget {
            provider: "test".to_string(),
            upstream_model: agent_shim_core::UpstreamModel::from("u"),
            policy: Default::default(),
        };
        let _ = registry
            .run_on_resolved((FrontendKind::AnthropicMessages, "m"), &ctx, req(), &target)
            .await
            .expect("ok");
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }
```

The exact field set of `BackendTarget` may differ — if construction fails, run `rtk grep -n "pub struct BackendTarget" crates/core/src/` to inspect, and adjust the literal.

- [ ] **Step 4: Build + lint + commit**

```
rtk cargo nextest run -p agent-shim-plugins
rtk cargo clippy -p agent-shim-plugins --all-targets -- -D warnings
rtk cargo fmt -p agent-shim-plugins -- --check
git add crates/plugins/src/registry.rs
git commit -m "feat(plugins): PluginRegistry::run_on_resolved (P04 T2)"
```

---

### Task 3: Implement `PluginRegistry::wrap_stream` (H5)

**Files:**
- Modify: `crates/plugins/src/registry.rs`

- [ ] **Step 1: Add the method**

`wrap_stream` is the only synchronous public method on the registry. It composes a wrapped `CanonicalStream` using `futures::StreamExt::then` for per-event plugin invocation and `futures::stream::iter` + `flat_map` to splice 1-in-N-out. The fast path (no plugins on the route OR no H5 subscribers) returns the upstream untouched.

Append:

```rust
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
    /// - Upstream `Err` items pass through unchanged — plugins never see
    ///   them (matches today's StreamEvent semantics).
    /// - Plugin `Err` emits one error event in the inbound frontend's
    ///   dialect (rendered downstream by the frontend's `encode_stream`)
    ///   and closes the wrapped stream. Specific envelope production is
    ///   handled by the frontend's existing error-event path; the
    ///   wrapper's job is to surface the error as a `StreamEvent::Error`
    ///   (or close, if `Error` doesn't exist on `StreamEvent` — see
    ///   Step 2 below).
    pub fn wrap_stream(
        &self,
        route: (agent_shim_core::FrontendKind, &str),
        ctx: crate::PluginContext,
        upstream: agent_shim_core::CanonicalStream,
    ) -> agent_shim_core::CanonicalStream {
        // Fast path: no plan, no plugins, no H5 subscribers — return upstream.
        let plan = match self.lookup(route.0, route.1) {
            Some(p) if !p.on_stream_event.is_empty() => p.clone(),
            _ => return upstream,
        };
        let plugins: Vec<Arc<PluginEntry>> = plan.on_stream_event.clone();
        let ctx = Arc::new(ctx); // shared across all events

        use futures::StreamExt;
        let wrapped = upstream.then(move |event_result| {
            let plugins = plugins.clone();
            let ctx = ctx.clone();
            async move {
                // Upstream errors pass through.
                let mut event = match event_result {
                    Ok(e) => e,
                    Err(err) => return vec![Err(err)],
                };
                // Walk the H5 chain; one in, N out per plugin. The
                // intermediate "many out" state is kept as a Vec<StreamEvent>
                // and re-fed through subsequent plugins one event at a time.
                let mut buf: Vec<agent_shim_core::StreamEvent> = vec![event];
                for entry in &plugins {
                    if !entry.enabled {
                        continue;
                    }
                    let mut next: Vec<agent_shim_core::StreamEvent> =
                        Vec::with_capacity(buf.len());
                    let plugin_name = entry.name.clone();
                    let plugin = entry.plugin.clone();
                    let timeout = entry.timeouts.for_hook(crate::Hook::StreamEvent);
                    let on_error = entry.on_error;
                    for ev in buf.drain(..) {
                        let ctx_ref = &*ctx;
                        let outcome = crate::invoke::invoke::<Vec<agent_shim_core::StreamEvent>, _>(
                            &plugin_name,
                            ctx_ref,
                            crate::Hook::StreamEvent.as_str(),
                            timeout,
                            on_error,
                            // The plugin sees one event at a time; the
                            // 1-in-N-out splicing is the registry's job.
                            async { plugin.on_stream_event(ctx_ref, ev.clone()).await },
                        )
                        .await;
                        match outcome {
                            crate::invoke::InvokeOutcome::Success(events) => {
                                next.extend(events);
                            }
                            crate::invoke::InvokeOutcome::Skipped => {
                                // Treat as identity for stream events:
                                // a skipped plugin must not drop the event
                                // wholesale. (Spec §6.5: skip = "act as if
                                // the plugin returned the same event
                                // unchanged".)
                                next.push(ev);
                            }
                            crate::invoke::InvokeOutcome::Propagate(_) => {
                                // Emit a wire-level error event then close.
                                // `StreamEvent::Error` may or may not exist;
                                // see Step 2 — fallback is `MessageStop`
                                // with a synthetic reason or `Err` boxing.
                                event = ev; // borrow trick to keep the
                                // surrounding closure type-uniform
                                let _ = event; // silence unused-variable
                                return vec![Err(Box::new(std::io::Error::new(
                                    std::io::ErrorKind::Other,
                                    "plugin failed mid-stream",
                                ))
                                    as Box<dyn std::error::Error + Send + Sync>)];
                            }
                        }
                    }
                    buf = next;
                }
                buf.into_iter().map(Ok).collect::<Vec<_>>()
            }
        });
        Box::pin(wrapped.flat_map(futures::stream::iter))
    }
```

- [ ] **Step 2: Decide the mid-stream-failure shape**

The plan code above emits `Err(Box<dyn Error + Send + Sync>)` so the inbound frontend's existing error-event path renders it as an SSE `event: error` frame. Verify this works:

Run: `rtk grep -n "type CanonicalStream\b" crates/core/src/`
Inspect the alias. If it's `Pin<Box<dyn Stream<Item = Result<StreamEvent, BoxedError>> + Send>>`, the above shape is correct. If it's `Result<StreamEvent, FrontendError>` or `Result<StreamEvent, ProviderError>`, adjust the boxed-error construction to the appropriate variant constructor (e.g. `agent_shim_providers::ProviderError::Other("plugin failed mid-stream".into())`).

Document the actual `CanonicalStream` Item type in a comment above the error-emission site.

- [ ] **Step 3: Unit test — fast path**

```rust
    #[tokio::test]
    async fn wrap_stream_fast_path_returns_identity() {
        use futures::stream::StreamExt;
        let registry = PluginRegistry::empty();
        let ctx = crate::PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x/y".to_string(),
        };
        let upstream: agent_shim_core::CanonicalStream = Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::MessageStop {
                stop_reason: agent_shim_core::StopReason::EndTurn,
                stop_sequence: None,
            }),
        ]));
        let wrapped = registry.wrap_stream((FrontendKind::AnthropicMessages, "anything"), ctx, upstream);
        let collected: Vec<_> = wrapped.collect().await;
        assert_eq!(collected.len(), 1);
        assert!(collected[0].is_ok());
    }
```

- [ ] **Step 4: Unit test — H5 plugin that drops every other event**

```rust
    #[tokio::test]
    async fn wrap_stream_h5_plugin_drops_every_other_event() {
        use futures::stream::StreamExt;
        struct DropEverySecond {
            counter: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl crate::Plugin for DropEverySecond {
            fn kind_name(&self) -> &'static str { "drop_every_second" }
            fn hooks(&self) -> crate::HookSet { crate::HookSet::STREAM_EVENT }
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
                specific: { let mut m = HashMap::new(); m.insert("m".to_string(), plan); m },
                wildcard: None,
                is_empty: false,
            },
        );
        let registry = PluginRegistry {
            plugins: { let mut p = HashMap::new(); p.insert("d".to_string(), entry); p },
            plans,
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
                    text: format!("e{i}"),
                })
            })
            .collect();
        let upstream: agent_shim_core::CanonicalStream =
            Box::pin(futures::stream::iter(events));
        let wrapped = registry.wrap_stream((FrontendKind::AnthropicMessages, "m"), ctx, upstream);
        let collected: Vec<_> = wrapped.collect().await;
        let oks: Vec<_> = collected.into_iter().filter_map(Result::ok).collect();
        assert_eq!(oks.len(), 2);
    }
```

The exact `StreamEvent::TextDelta { text }` variant fields may differ in the actual core crate (could be `{ delta }` or `{ index, delta }`). Run `rtk grep -n "TextDelta\b" crates/core/src/stream.rs` and adjust the literal.

- [ ] **Step 5: Build + commit**

```
rtk cargo nextest run -p agent-shim-plugins
rtk cargo clippy -p agent-shim-plugins --all-targets -- -D warnings
rtk cargo fmt -p agent-shim-plugins -- --check
git add crates/plugins/src/registry.rs
git commit -m "feat(plugins): PluginRegistry::wrap_stream H5 (P04 T3)"
```

---

### Task 4: Implement `PluginRegistry::run_on_response_complete` (H7)

**Files:**
- Modify: `crates/plugins/src/registry.rs`

- [ ] **Step 1: Add the method**

For P04, H7 uses bare `tokio::spawn` per plugin. The JoinSet wiring + `flush_pending_h7` lands in P05. This task only needs to make H7 fire correctly; tracking and shutdown flush are out of scope.

Append:

```rust
    /// H7 hook — fire-and-forget. Spec §6.2 / §6.7 / §6.8.
    ///
    /// Each subscribed plugin is `tokio::spawn`-ed. This function returns
    /// synchronously; the spawned tasks live until they complete or are
    /// dropped at shutdown. P05 will wire these into a `JoinSet` so
    /// shutdown can flush them within a deadline.
    pub fn run_on_response_complete(
        &self,
        route: (agent_shim_core::FrontendKind, &str),
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
            let timeout_ms = entry.timeouts.for_hook(crate::Hook::ResponseComplete);
            let on_error = entry.on_error;
            let summary = summary.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move {
                let _ = crate::invoke::invoke::<(), _>(
                    &plugin_name,
                    &ctx,
                    crate::Hook::ResponseComplete.as_str(),
                    timeout_ms,
                    on_error,
                    async { plugin.on_response_complete(&ctx, &summary).await },
                )
                .await;
                // Return value is discarded — H7 cannot affect the response.
                // Outcome is captured via the logging/metrics in P05.
            });
        }
    }
```

- [ ] **Step 2: Unit test**

```rust
    #[tokio::test]
    async fn run_on_response_complete_fires_async() {
        use crate::ResponseSummary;
        struct RecordSummary {
            captured: Arc<tokio::sync::Mutex<Option<u64>>>,
        }
        #[async_trait]
        impl crate::Plugin for RecordSummary {
            fn kind_name(&self) -> &'static str { "record_summary" }
            fn hooks(&self) -> crate::HookSet { crate::HookSet::RESPONSE_COMPLETE }
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
            plugin: Arc::new(RecordSummary { captured: captured.clone() }),
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
                specific: { let mut m = HashMap::new(); m.insert("m".to_string(), plan); m },
                wildcard: None,
                is_empty: false,
            },
        );
        let registry = PluginRegistry {
            plugins: { let mut p = HashMap::new(); p.insert("rs".to_string(), entry); p },
            plans,
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
        // Spawned task — yield once to let it run.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(*captured.lock().await, Some(42));
    }
```

- [ ] **Step 3: Build + commit**

```
rtk cargo nextest run -p agent-shim-plugins
rtk cargo clippy -p agent-shim-plugins --all-targets -- -D warnings
rtk cargo fmt -p agent-shim-plugins -- --check
git add crates/plugins/src/registry.rs
git commit -m "feat(plugins): PluginRegistry::run_on_response_complete H7 (P04 T4)"
```

---

### Task 5: Add `HandlerError::PluginFailed` variant + envelope rendering

**Files:**
- Modify: `crates/gateway/src/handlers/mod.rs`

- [ ] **Step 1: Add `agent-shim-plugins` to gateway deps**

Open `crates/gateway/Cargo.toml`. In `[dependencies]`, add:

```toml
agent-shim-plugins = { path = "../plugins" }
```

(If the entry already exists from P02, skip this step.)

- [ ] **Step 2: Add the variant**

Open `crates/gateway/src/handlers/mod.rs`. The `HandlerError` enum is at line ~21. Append a new variant after `Unauthorized`:

```rust
    /// Plan 07 P04: a plugin returned an error and `on_error: fail` (or
    /// the error was `Aborted` / `ProtectedFieldMutated`, both of which
    /// always propagate). `aborted` distinguishes the 400 case (plugin
    /// chose to reject the request) from the 502 case (plugin or
    /// gateway-side failure).
    ///
    /// `plugin` and `hook` are operator-facing diagnostic strings;
    /// neither is exposed in the response envelope (they go into the
    /// JSON error message only when `aborted: true`, where the plugin's
    /// `reason` field is the user-facing payload).
    #[error("plugin `{plugin}` failed on hook `{hook}` (aborted={aborted})")]
    PluginFailed {
        kind: FrontendKind,
        plugin: String,
        hook: &'static str,
        aborted: bool,
    },
```

- [ ] **Step 3: Add envelope rendering branches in `IntoResponse`**

In the `IntoResponse for HandlerError::into_response` method (line ~275), add a new early-return block right after the `Unauthorized` block (line ~334):

```rust
        // Plan 07 P04: a plugin's error surfaces here as one of:
        // - aborted=true: HTTP 400 (plugin chose to reject — user error).
        // - aborted=false: HTTP 502 (plugin/gateway failure, transient
        //   or otherwise).
        // Envelope shape mirrors `CapabilityMismatch` / `Unauthorized` —
        // dialect-specific JSON keyed off the carried `FrontendKind`.
        if let HandlerError::PluginFailed { kind, aborted, .. } = &self {
            let message = self.to_string(); // includes plugin + hook
            let body = match kind {
                FrontendKind::AnthropicMessages => serde_json::json!({
                    "type": "error",
                    "error": {
                        "type": if *aborted { "invalid_request_error" } else { "api_error" },
                        "message": message,
                    },
                }),
                FrontendKind::OpenAiChat | FrontendKind::OpenAiResponses => serde_json::json!({
                    "error": {
                        "message": message,
                        "type": if *aborted { "invalid_request_error" } else { "api_error" },
                        "code": if *aborted { "plugin_aborted" } else { "plugin_failed" },
                    },
                }),
            };
            let status = if *aborted { StatusCode::BAD_REQUEST } else { StatusCode::BAD_GATEWAY };
            return (status, axum::Json(body)).into_response();
        }
```

- [ ] **Step 4: Extend `handler_error_status_hint` in `pipeline.rs`**

Open `crates/gateway/src/pipeline.rs` around line 175 (`fn handler_error_status_hint`). Add a new arm before `HandlerError::Provider(_) => 502,`:

```rust
        HandlerError::PluginFailed { aborted: true, .. } => 400,
        HandlerError::PluginFailed { aborted: false, .. } => 502,
```

- [ ] **Step 5: Add unit tests**

In `mod tests` at the bottom of `handlers/mod.rs` (or its existing test module — check; if there's no tests module, append one), assert envelope shapes:

```rust
    #[test]
    fn plugin_failed_aborted_renders_400_anthropic_envelope() {
        let err = HandlerError::PluginFailed {
            kind: FrontendKind::AnthropicMessages,
            plugin: "p".to_string(),
            hook: "on_decoded_request",
            aborted: true,
        };
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        // Body parsing left to the integration tests in T11.
    }

    #[test]
    fn plugin_failed_not_aborted_renders_502_openai_envelope() {
        let err = HandlerError::PluginFailed {
            kind: FrontendKind::OpenAiChat,
            plugin: "p".to_string(),
            hook: "on_resolved",
            aborted: false,
        };
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }
```

- [ ] **Step 6: Build + lint + commit**

```
rtk cargo check -p agent-shim-gateway
rtk cargo nextest run -p agent-shim-gateway --lib
rtk cargo clippy -p agent-shim-gateway --all-targets -- -D warnings
rtk cargo fmt -p agent-shim-gateway -- --check
git add crates/gateway/Cargo.toml crates/gateway/src/handlers/mod.rs crates/gateway/src/pipeline.rs
git commit -m "feat(gateway): HandlerError::PluginFailed envelope + status mapping (P04 T5)"
```

If `Cargo.toml` was already updated (P02 might have added the dep), drop it from `git add`.

---

### Task 6: Generalise `StreamLogger` → `H7Guard` in `pipeline.rs`

**Files:**
- Modify: `crates/gateway/src/pipeline.rs`

- [ ] **Step 1: Read the current shape**

The current `StreamLogger` (lines ~1017–1045) only logs final usage when the SSE stream drops. It's used in `run_stream`'s Anthropic branch (line ~743). The OpenAI Chat and OpenAI Responses streaming branches use a plain `tracing::info!` after `frontend_response_to_axum` (line ~803).

`GuardedStream<S>` (line ~1047) wraps `(inner: S, _logger: StreamLogger)` and proxies `poll_next`. Its `Drop` impl runs `StreamLogger::drop`.

- [ ] **Step 2: Rename `StreamLogger` → `H7Guard` and extend it**

`H7Guard` captures the same logger fields PLUS the registry/route/context needed to fire H7. On Drop it logs (today's behaviour) AND invokes `registry.run_on_response_complete(...)`.

Replace the `StreamLogger` struct + Drop impl with:

```rust
/// Plan 07 P04: universal H7 guard. Fires `run_on_response_complete` on
/// the registry when the SSE stream finishes (or is dropped early by
/// client disconnect). Replaces the Anthropic-only `StreamLogger` of v0.6
/// — now active for all three frontends (Q6).
///
/// The structured-log line (today's "← {label} (stream) | ..." emission)
/// remains; it's just folded into the Drop alongside the H7 firing so
/// both happen at the same lifecycle point.
struct H7Guard {
    endpoint_label: &'static str,
    model_alias: String,
    upstream_model: String,
    usage: Arc<Mutex<Option<Usage>>>,
    started: std::time::Instant,
    /// `None` means "log only, no H7 invocation" — used as a temporary
    /// during the migration if any call site cannot wire the registry
    /// yet. Production wiring (T7) always sets `Some(_)`.
    registry: Option<Arc<agent_shim_plugins::PluginRegistry>>,
    route: Option<(agent_shim_core::FrontendKind, String)>,
    ctx: Option<agent_shim_plugins::PluginContext>,
}

impl Drop for H7Guard {
    fn drop(&mut self) {
        let u = self.usage.lock().clone();
        let (input, output) = match u {
            Some(ref usage) => (
                usage.input_tokens.unwrap_or(0),
                usage.output_tokens.unwrap_or(0),
            ),
            None => (0, 0),
        };
        let elapsed = self.started.elapsed();
        tracing::info!(
            "← {} (stream) | model: {} → {} | input: {} | output: {} | {:.1}s",
            self.endpoint_label,
            self.model_alias,
            self.upstream_model,
            input,
            output,
            elapsed.as_secs_f64()
        );
        if let (Some(registry), Some(route), Some(ctx)) =
            (self.registry.take(), self.route.take(), self.ctx.take())
        {
            let summary = agent_shim_plugins::ResponseSummary {
                usage: u,
                elapsed_ms: elapsed.as_millis() as u64,
                upstream_status: agent_shim_plugins::UpstreamStatus::Success,
            };
            registry.run_on_response_complete(
                (route.0, route.1.as_str()),
                ctx,
                summary,
            );
        }
    }
}
```

- [ ] **Step 3: Rename the type in `GuardedStream`**

Update the `GuardedStream` struct (line ~1047):

```rust
struct GuardedStream<S> {
    inner: S,
    _guard: H7Guard,
}
```

- [ ] **Step 4: Update the Anthropic streaming branch**

In `run_stream` (line ~740), `let logger = StreamLogger { ... }` becomes `let guard = H7Guard { ... registry: None, route: None, ctx: None }`. The actual registry/route/ctx wiring happens in T7.

Replace the `StreamLogger { ... }` literal with:
```rust
let guard = H7Guard {
    endpoint_label: label,
    model_alias: model_alias.clone(),
    upstream_model: upstream_model.clone(),
    usage: usage_capture.clone(),
    started,
    registry: None,
    route: None,
    ctx: None,
};
```

And update the `GuardedStream` literal:
```rust
let guarded = GuardedStream {
    inner: sse_stream,
    _guard: guard,
};
```

- [ ] **Step 5: Workspace check**

```
rtk cargo check -p agent-shim-gateway
rtk cargo clippy -p agent-shim-gateway --all-targets -- -D warnings
rtk cargo nextest run -p agent-shim-gateway --lib
```

Expected: clean. No tests rely on the type name `StreamLogger`.

- [ ] **Step 6: Commit**

```
rtk cargo fmt -p agent-shim-gateway -- --check
git add crates/gateway/src/pipeline.rs
git commit -m "refactor(gateway): rename StreamLogger to H7Guard with optional registry plumbing (P04 T6)"
```

This task is a deliberate rename-only commit (plus the new optional fields); behaviour is unchanged. Wiring the registry happens in T7. Splitting the rename keeps the diff readable.

---

### Task 7: Thread `PluginRegistry` through `AppCore` and into the pipeline

**Files:**
- Modify: `crates/gateway/src/state.rs`
- Modify: `crates/gateway/src/pipeline.rs`

- [ ] **Step 1: Add the `plugins` field to `AppCore`**

Open `crates/gateway/src/state.rs`. In the `AppCore` struct (line ~56), add:

```rust
    /// Plan 07 P04: plugin registry. Built once at startup; reload-time
    /// hot-swapping lands in P07. For now `PluginRegistry::empty()` is
    /// the always-true default (no config crate path wires plugins
    /// through yet — that's P06).
    pub plugins: Arc<agent_shim_plugins::PluginRegistry>,
```

- [ ] **Step 2: Default to `empty()` in `AppState::build`**

In `AppState::build` (`state.rs`), populate the new field at the end of the `AppCore` literal:

```rust
            plugins: Arc::new(agent_shim_plugins::PluginRegistry::empty()),
```

- [ ] **Step 3: Wire the registry into `H7Guard` from `run_stream`**

In `pipeline.rs::run_stream`'s Anthropic branch, change the `H7Guard` construction to populate the registry fields:

```rust
let guard = H7Guard {
    endpoint_label: label,
    model_alias: model_alias.clone(),
    upstream_model: upstream_model.clone(),
    usage: usage_capture.clone(),
    started,
    registry: Some(state.core.plugins.clone()),
    route: Some((frontend_kind, model_alias.clone())),
    ctx: Some(plugin_ctx_for_run.clone()),
};
```

(`plugin_ctx_for_run` is constructed once at the top of `run_stream` — see Step 4 — so both the Anthropic branch and the OpenAI branches can share it.)

- [ ] **Step 4: Build a `PluginContext` once near the top of `run_stream`**

Near the top of `run_stream` (after `frontend_kind` is in scope), construct:

```rust
let plugin_ctx_for_run = agent_shim_plugins::PluginContext {
    request_id: canonical.id,
    frontend: frontend_kind,
    route_label: format!("{:?}/{}", frontend_kind, model_alias),
};
```

The `format!` shape matches existing rate-limiter `route_label` conventions. Verify by grep: `rtk grep -n "route_label" crates/gateway/src/`.

- [ ] **Step 5: Update the OpenAI Chat / OpenAI Responses streaming branches**

The "plain post-spawn log" branch (line ~796 in `run_stream`) currently does NOT use a Drop guard. Replace it with the same `H7Guard` wrapping so H7 fires uniformly across all three frontends (spec §6.7).

Replace lines ~796–811 (the `} else { ... }` branch) with:

```rust
    } else {
        // Plan 07 P04: universal H7 guard across all streaming frontends.
        // Today's OpenAI Chat / Responses branches did not use a Drop
        // guard; they now do, so `on_response_complete` fires regardless
        // of inbound frontend.
        let usage_capture: Arc<Mutex<Option<Usage>>> = Arc::new(Mutex::new(None));
        let guard = H7Guard {
            endpoint_label: label,
            model_alias: model_alias.clone(),
            upstream_model: upstream_model.clone(),
            usage: usage_capture.clone(),
            started,
            registry: Some(state.core.plugins.clone()),
            route: Some((frontend_kind, model_alias.clone())),
            ctx: Some(plugin_ctx_for_run.clone()),
        };
        // Capture usage events for the guard's final log line.
        let logging_stream = upstream_stream.map(move |event| {
            if let Ok(ref ev) = event {
                match ev {
                    StreamEvent::UsageDelta { usage } => {
                        *usage_capture.lock() = Some(usage.clone());
                    }
                    StreamEvent::ResponseStop { usage: Some(u) } => {
                        *usage_capture.lock() = Some(u.clone());
                    }
                    _ => {}
                }
            }
            event
        });
        let canonical_stream: CanonicalStream = Box::pin(logging_stream);
        let frontend_response = {
            let _encode_span = tracing::info_span!("stream.encode").entered();
            spec.frontend.encode_stream(canonical_stream)
        };
        match frontend_response {
            FrontendResponse::Stream {
                content_type,
                stream: sse_stream,
            } => {
                let guarded = GuardedStream {
                    inner: sse_stream,
                    _guard: guard,
                };
                let body = Body::from_stream(guarded.map(|r| r.map_err(|e| e.to_string())));
                let mut r = Response::new(body);
                r.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    HeaderValue::from_str(&content_type)
                        .unwrap_or_else(|_| HeaderValue::from_static("text/event-stream")),
                );
                Ok(r)
            }
            _ => unreachable!("encode_stream must return Stream"),
        }
    }
```

Note: this drops the `tracing::info!` line that used to live in the OpenAI branches and moves it into the Drop impl of `H7Guard`. That's intentional — uniformity across frontends is the whole point of §6.7. Verify the line is still emitted (just at Drop time, not before).

Also drop the now-dead `spec.log_streaming_usage_on_drop: bool` field if its only role was selecting between the two branches. Cross-reference `rtk grep -n "log_streaming_usage_on_drop" crates/gateway/src/` — if it's now read by no one, mark it `#[allow(dead_code)]` and add a comment; full removal is a non-Phase-7 cleanup.

- [ ] **Step 6: Update `AppCore::Clone` derive**

`AppCore` already derives `Clone`. `Arc<PluginRegistry>` is `Clone`, so the new field is `Clone`-friendly automatically. Verify with `rtk cargo check -p agent-shim-gateway`.

- [ ] **Step 7: Update all `AppCore` literal sites**

Grep for `AppCore {` in tests / other modules and ensure each literal adds `plugins: Arc::new(agent_shim_plugins::PluginRegistry::empty()),`:

```
rtk grep -rn "AppCore\s*{" crates/ --include='*.rs'
```

Pattern matches T2 / T3 of P03 (mechanical field add).

- [ ] **Step 8: Tests + commit**

```
rtk cargo nextest run --workspace
rtk cargo clippy --workspace --all-targets -- -D warnings
rtk cargo fmt --all -- --check
git add crates/gateway/src/state.rs crates/gateway/src/pipeline.rs <other-touched-files>
git commit -m "feat(gateway): thread PluginRegistry through AppCore + universal H7Guard wiring (P04 T7)"
```

Expected: 745/745 baseline still passes. No new test in this task — pipeline integration tests in T11 cover end-to-end behaviour.

---

### Task 8: Add the H2 call site in `dispatch_inner`

**Files:**
- Modify: `crates/gateway/src/pipeline.rs`

- [ ] **Step 1: Locate the anchor**

In `dispatch_inner` (line ~202), find the line `let mut canonical = decoded.expect("canonical request was decoded above");` (line ~521). Per spec §6.6 row 1, the H2 call goes immediately after this.

- [ ] **Step 2: Build `PluginContext` before the H2 call**

The H2 call needs:
- The route key: `(frontend_kind, &model_alias)` — but `model_alias` isn't constructed until line ~559 (search for `let model_alias =`). Move it earlier OR construct a temporary one.

Inspection: `rtk grep -n "model_alias\s*=" crates/gateway/src/pipeline.rs`. If `model_alias` derives directly from `canonical.model.as_str()`, it can be constructed inline:

```rust
let model_alias_for_plugins = canonical.model.to_string();
```

`frontend_kind` is already in scope (constructed at line ~602). If not yet, hoist its computation upward:

```rust
let frontend_kind = spec.frontend.kind();
```

(This may already exist further down — move it up to before the H2 call site.)

Construct the context:

```rust
let plugin_ctx = agent_shim_plugins::PluginContext {
    request_id: canonical.id,
    frontend: frontend_kind,
    route_label: format!("{:?}/{}", frontend_kind, model_alias_for_plugins),
};
```

- [ ] **Step 3: Add the H2 call**

Immediately after `let mut canonical = decoded.expect(...)` and the `if spec.capture_anthropic_headers { ... }` block:

```rust
    // ── Plan 07 P04 spec §6.6 anchor 1: H2 (on_decoded_request) ───────
    // After decode + header capture, before route resolution. The
    // registry walks the route's H2 chain; protected-field violations
    // and on_error: fail propagate as `HandlerError::PluginFailed`.
    canonical = state
        .core
        .plugins
        .run_on_decoded_request(
            (frontend_kind, &model_alias_for_plugins),
            &plugin_ctx,
            canonical,
        )
        .await
        .map_err(|e| handler_error_from_plugin_error(e, frontend_kind))?;
```

- [ ] **Step 4: Add `handler_error_from_plugin_error` helper**

In `pipeline.rs`, near `handler_error_status_hint` (line ~175), add a free function:

```rust
/// Plan 07 P04: bridge from `PluginError` (raw, frontend-agnostic) to
/// `HandlerError::PluginFailed` (kind-aware, axum-renderable). Splits
/// `Aborted` from everything else into the `aborted: true | false`
/// distinction the status mapper consumes.
fn handler_error_from_plugin_error(
    err: agent_shim_plugins::PluginError,
    kind: agent_shim_core::FrontendKind,
) -> HandlerError {
    use agent_shim_plugins::PluginError;
    match err {
        PluginError::Aborted { plugin, .. } => HandlerError::PluginFailed {
            kind,
            plugin,
            hook: "aborted", // Aborted isn't tied to one hook
            aborted: true,
        },
        PluginError::Timeout { plugin, hook, .. } => HandlerError::PluginFailed {
            kind,
            plugin,
            hook,
            aborted: false,
        },
        PluginError::Failed { plugin, hook, .. } => HandlerError::PluginFailed {
            kind,
            plugin,
            hook,
            aborted: false,
        },
        PluginError::ProtectedFieldMutated { plugin, hook, .. } => HandlerError::PluginFailed {
            kind,
            plugin,
            hook,
            aborted: false,
        },
    }
}
```

(Verify against `crates/plugins/src/error.rs` that the four variants are exactly `Timeout`, `Failed`, `Aborted`, `ProtectedFieldMutated`. If there's a fifth, add a branch for it.)

- [ ] **Step 5: Build**

```
rtk cargo check -p agent-shim-gateway
```

Common failure: `canonical` is shadowed by `let mut canonical =` then the H2 call returns `CanonicalRequest` which we assign back. That should compile (the binding is mutable). If the compiler complains about `model_alias_for_plugins` being used after `canonical` was passed by value — `model_alias_for_plugins: String` is `Clone` and the closure took it by reference; should be fine. If not, clone it.

- [ ] **Step 6: Commit**

```
rtk cargo nextest run -p agent-shim-gateway --lib
git add crates/gateway/src/pipeline.rs
git commit -m "feat(gateway): wire H2 hook (on_decoded_request) into dispatch_inner (P04 T8)"
```

---

### Task 9: Add the H3 call site in `dispatch_inner`

**Files:**
- Modify: `crates/gateway/src/pipeline.rs`

- [ ] **Step 1: Locate the anchor**

Per spec §6.6 row 2, the H3 call goes immediately after:

```rust
canonical.resolved_policy = first_target.policy.resolve(&canonical);
```

This is line ~532.

- [ ] **Step 2: Insert the call**

```rust
    // ── Plan 07 P04 spec §6.6 anchor 2: H3 (on_resolved) ──────────────
    // After route resolve + policy snapshot, before capability gate.
    // Plugins see the resolved `BackendTarget` (= chain head) so they
    // can adapt to per-upstream context (e.g., target-specific prompt
    // shaping). Failure semantics identical to H2.
    canonical = state
        .core
        .plugins
        .run_on_resolved(
            (frontend_kind, &model_alias_for_plugins),
            &plugin_ctx,
            canonical,
            first_target,
        )
        .await
        .map_err(|e| handler_error_from_plugin_error(e, frontend_kind))?;
```

`first_target` is already in scope at this point (it's used on the line above). Verify with `rtk grep -n "first_target" crates/gateway/src/pipeline.rs`.

- [ ] **Step 3: Build + commit**

```
rtk cargo check -p agent-shim-gateway
rtk cargo nextest run -p agent-shim-gateway --lib
git add crates/gateway/src/pipeline.rs
git commit -m "feat(gateway): wire H3 hook (on_resolved) into dispatch_inner (P04 T9)"
```

---

### Task 10: Add the H5 call site (wrap upstream stream) in both streaming and unary paths

**Files:**
- Modify: `crates/gateway/src/pipeline.rs`

- [ ] **Step 1: Streaming path**

In `run_stream` (line ~662), find the line where the upstream stream is unwrapped from `upstream_stream_result`:

```rust
let upstream_stream = upstream_stream_result.map_err(|e| {
    tracing::error!(error = %e, "resilient call failed");
    HandlerError::from_resilience_error(e, frontend_kind)
})?;
```

(Line ~735.)

Insert immediately AFTER:

```rust
    // ── Plan 07 P04 spec §6.6 anchor 3 (streaming): H5 (on_stream_event) ──
    // Per-event plugin chain. Fast path: zero-allocation identity when
    // no `on_stream_event` subscribers exist for this route.
    let upstream_stream = state.core.plugins.wrap_stream(
        (frontend_kind, &model_alias),
        plugin_ctx_for_run.clone(),
        upstream_stream,
    );
```

(`plugin_ctx_for_run` was constructed in T7 Step 4.)

- [ ] **Step 2: Unary path**

In `run_unary` (line ~818), find the equivalent line (~885):

```rust
let stream = stream_result.map_err(|e| { ... })?;
```

Insert after:

```rust
    // ── Plan 07 P04 spec §6.6 anchor 3 (unary): H5 (on_stream_event) ──
    // Unary still goes through a stream → collect path internally;
    // plugins observe the intermediate stream events even though the
    // caller eventually sees a flat `CanonicalResponse`.
    let plugin_ctx_for_unary = agent_shim_plugins::PluginContext {
        request_id: canonical_id,
        frontend: frontend_kind,
        route_label: format!("{:?}/{}", frontend_kind, model_alias),
    };
    let stream = state.core.plugins.wrap_stream(
        (frontend_kind, &model_alias),
        plugin_ctx_for_unary,
        stream,
    );
```

`canonical_id` here means whatever `canonical.id` was BEFORE `canonical` got consumed. The simplest fix is to capture it once before the chain walk:

```rust
let canonical_id = canonical.id; // RequestId: Copy
```

Insert that snapshot near the top of `run_unary`.

- [ ] **Step 3: Build + commit**

```
rtk cargo check -p agent-shim-gateway
rtk cargo nextest run -p agent-shim-gateway --lib
git add crates/gateway/src/pipeline.rs
git commit -m "feat(gateway): wire H5 hook (wrap_stream) into both streaming and unary paths (P04 T10)"
```

---

### Task 11: Run unary H7 inline + final empty-registry regression check

**Files:**
- Modify: `crates/gateway/src/pipeline.rs`

- [ ] **Step 1: Add unary H7 invocation**

In `run_unary` (line ~888), after `let response = collect_stream(stream).await?;`, add:

```rust
    // ── Plan 07 P04 spec §6.6 anchor 4 (unary): H7 (on_response_complete) ──
    // Unary path runs H7 inline (caller is awaiting the HTTP response,
    // so plugin time is included in the total elapsed). Streaming path
    // fires H7 from the H7Guard's Drop — see `H7Guard` in this file.
    {
        let summary = agent_shim_plugins::ResponseSummary {
            usage: response.usage.clone(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            upstream_status: agent_shim_plugins::UpstreamStatus::Success,
        };
        let plugin_ctx_for_h7 = agent_shim_plugins::PluginContext {
            request_id: canonical_id,
            frontend: frontend_kind,
            route_label: format!("{:?}/{}", frontend_kind, model_alias),
        };
        state.core.plugins.run_on_response_complete(
            (frontend_kind, &model_alias),
            plugin_ctx_for_h7,
            summary,
        );
    }
```

This is fire-and-forget — `run_on_response_complete` spawns and returns synchronously, so the unary handler keeps going without awaiting H7. Plugin completion time IS included in the visible response time only when the spawned task happens to be CPU-bound enough to delay the runtime; in practice H7 plugins are short and runtime scheduling is independent.

Note: unary doesn't get an `H7Guard` because there's no Drop-time event to hook into — the unary response goes out as a single body. H7 firing here is structurally similar to the streaming path's `H7Guard::drop`, just synchronously triggered.

- [ ] **Step 2: Verify all four anchors are wired**

Run:
```
rtk grep -n "anchor [1-4]" crates/gateway/src/pipeline.rs
```

Expect 5 matches: H2 (anchor 1), H3 (anchor 2), H5 in run_stream (anchor 3 streaming), H5 in run_unary (anchor 3 unary), H7 in run_unary (anchor 4 unary). Plus the H7Guard's Drop for streaming (no anchor comment — search `H7Guard` instead).

- [ ] **Step 3: Build + workspace test**

```
rtk cargo nextest run --workspace
rtk cargo clippy --workspace --all-targets -- -D warnings
rtk cargo fmt --all -- --check
```

Expected: 745/745 + some new plugin-unit tests from T1-T4 = ~760 passing. Zero failures. The empty-registry fast path means every existing test (which doesn't configure plugins) sees byte-identical behaviour.

- [ ] **Step 4: Commit**

```
git add crates/gateway/src/pipeline.rs
git commit -m "feat(gateway): wire H7 hook (on_response_complete) inline in unary path (P04 T11)"
```

---

### Task 12: Integration tests in `crates/gateway/tests/plugins_pipeline.rs`

**Files:**
- Create: `crates/gateway/tests/plugins_pipeline.rs`

- [ ] **Step 1: Inspect existing integration test patterns**

Look at one existing file under `crates/gateway/tests/` for the style — e.g. `breaker_trip_skips_upstream.rs` or `e2e_openai_chat.rs`. Most use `mockito::Server` for upstream stubbing and `AppState::new` + `axum::Router` + `tower::ServiceExt::oneshot` for request driving.

- [ ] **Step 2: Build a custom `PluginRegistry` for integration tests**

The pipeline reads `state.core.plugins` directly. The minimal hack is to construct an `AppState` normally and then patch `state.core.plugins` before the request. Since `AppCore` is `Arc<...>` and immutable after construction, this requires a test helper.

Add a `#[cfg(test)] #[doc(hidden)] pub fn AppCore::with_plugins(self, plugins: Arc<PluginRegistry>) -> Self { ... }` helper in `state.rs`. Or, more cleanly, expose a test-only constructor `AppState::new_with_plugins(config, plugins) -> (Self, _)`.

Add to `state.rs` (gated by `#[cfg(test)]` or `#[doc(hidden)]`):

```rust
    /// Plan 07 P04: integration-test-only constructor that lets tests
    /// inject a custom `PluginRegistry` (instead of the default
    /// `empty()`). Same shape as `new_with_clock`.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub async fn new_with_plugins(
        config: agent_shim_config::GatewayConfig,
        plugins: Arc<agent_shim_plugins::PluginRegistry>,
    ) -> (
        Self,
        tokio::sync::mpsc::Receiver<crate::reload_trigger::ReloadRequest>,
    ) {
        let (state, rx) = Self::build(config, Arc::new(SystemClock)).await;
        // Rebuild AppCore with the override.
        let core = AppCore {
            plugins,
            ..AppCore::clone(&state.core)
        };
        let new_state = AppState {
            core: Arc::new(core),
            snapshot: state.snapshot,
        };
        (new_state, rx)
    }
```

(`AppCore` already derives `Clone`, so the field-update sugar works.)

- [ ] **Step 3: Write the integration tests**

Create `crates/gateway/tests/plugins_pipeline.rs`:

```rust
//! Plan 07 P04 integration tests: PluginRegistry wired into
//! `pipeline.rs::dispatch_inner` at the four spec §6.6 anchors.
//!
//! These tests use mockito for upstream stubbing and a small set of
//! stub plugins to verify the hook firing order, the empty-registry
//! parity, and the error envelope shapes.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use agent_shim_core::{
    BackendTarget, CanonicalRequest, FrontendKind, StreamEvent,
};
use agent_shim_plugins::{
    Hook, HookSet, HookTimeouts, OnError, Plugin, PluginContext, PluginEntry,
    PluginError, PluginRegistry, PluginResult, ResponseSummary,
};
use async_trait::async_trait;
use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

// ... (helpers: build minimal config pointing to mockito, build registry
//      with one plugin per hook, fire request through router, collect SSE)

/// T11 spec §9: empty registry path must match pre-P04 behaviour. We
/// don't compare byte-for-byte here (response is non-deterministic) but
/// we DO compare status code, content-type, and absence of plugin-error
/// envelope shape.
#[tokio::test]
async fn empty_registry_request_returns_200() {
    // ... build mockito stub returning a canned SSE response,
    //     wire AppState::new (NOT new_with_plugins — get the default
    //     empty() registry), POST a unary request, assert 200 + JSON body.
}

#[tokio::test]
async fn h2_plugin_fires_and_modifies_prompt() {
    // ... a stub plugin that appends a marker to the user message;
    //     verify the upstream mockito stub received the marker.
}

#[tokio::test]
async fn h3_plugin_fires_with_backend_target() {
    // ... a stub plugin that records the target.provider name into a
    //     shared Arc; assert the recorded value matches the configured
    //     upstream.
}

#[tokio::test]
async fn h5_plugin_drops_every_other_event() {
    // ... mockito stub returns 4 SSE deltas; H5 plugin drops every
    //     other; client sees 2.
}

#[tokio::test]
async fn h7_plugin_fires_after_unary_response() {
    // ... a stub plugin that records `summary.elapsed_ms` into an
    //     Arc<Mutex<Option<u64>>>; assert it's `Some(_)` after the
    //     client receives the response. Allow a brief tokio::time::sleep
    //     for the spawned H7 to run.
}

#[tokio::test]
async fn plugin_aborted_returns_400_anthropic_envelope() {
    // ... a stub plugin that returns `Err(PluginError::Aborted { ... })`
    //     on H2; assert response is 400 with `{"type":"error", ...}`.
}

#[tokio::test]
async fn plugin_failed_returns_502_when_on_error_fail() {
    // ... a stub plugin that returns `Err(PluginError::Failed)` with
    //     `OnError::Fail`; assert 502 + plugin_failed envelope.
}

#[tokio::test]
async fn plugin_protected_field_mutation_returns_502() {
    // ... a stub plugin that mutates `req.model`; assert 502 with
    //     ProtectedFieldMutated → PluginFailed envelope.
}

#[tokio::test]
async fn h5_mid_stream_failure_emits_sse_error_frame() {
    // ... a stub plugin that returns Err on its 3rd event; assert
    //     the response SSE contains an `event: error` frame at
    //     position 3 and closes afterwards. Status is still 200 (HTTP
    //     committed at first event).
}
```

This skeleton is the contract; each test body is ~30-50 lines following the existing `crates/gateway/tests/*.rs` style. The implementer fills them in.

If any test is too involved (e.g., precise SSE frame parsing for the mid-stream-error test), gate it behind `#[ignore]` and add a follow-up note — the spec compliance is what matters, exhaustive end-to-end SSE parsing is a P07 concern.

- [ ] **Step 4: Run tests + commit**

```
rtk cargo nextest run -p agent-shim-gateway --test plugins_pipeline
rtk cargo nextest run --workspace
rtk cargo clippy --workspace --all-targets -- -D warnings
rtk cargo fmt --all -- --check
git add crates/gateway/src/state.rs crates/gateway/tests/plugins_pipeline.rs
git commit -m "test(gateway): integration tests for plugin pipeline (P04 T12)"
```

Expected: workspace test count climbs by ~9 (one per `#[tokio::test]` above). Subtract any `#[ignore]`d tests.

---

## Acceptance criteria

- `PluginRegistry` has four new methods: `run_on_decoded_request`, `run_on_resolved`, `wrap_stream`, `run_on_response_complete`.
- All four methods exercise P02's `invoke()` template; H2/H3 also exercise `check_protected_fields`.
- `crates/gateway/src/pipeline.rs::dispatch_inner` has comment-marked anchors for spec §6.6 (positions 1–4) at the correct call sites.
- Streaming H7 fires via `H7Guard::drop` (generalised from `StreamLogger`) for ALL three frontends (Anthropic Messages, OpenAI Chat, OpenAI Responses).
- Unary H7 fires inline at end of `run_unary` before the HTTP response is returned.
- `HandlerError::PluginFailed { kind, plugin, hook, aborted }` exists with envelope rendering in `IntoResponse` and status mapping in `handler_error_status_hint` (400 for aborted, 502 otherwise).
- `AppCore.plugins: Arc<PluginRegistry>` field exists, defaulted to `PluginRegistry::empty()`.
- `AppState::new_with_plugins` test-only constructor exists for integration testing.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.
- `cargo test --workspace` shows zero failures with ~770 total tests (exact count flexible).
- No new external workspace dependencies (`agent-shim-plugins` was already present after P02).
- `crates/core/` is not touched (frozen-core invariant preserved).
- Empty-registry path: every existing pre-P04 integration test still passes — the `PluginRegistry::empty()` fast path adds zero observable behaviour.

## Notes for the implementer

- The clone-then-swap pattern is intentionally simple — `CanonicalRequest::clone()` is allocated-heavy but acceptable for a per-request control-plane path. Don't micro-optimize.
- `H7Guard` is `Drop`-based and `run_on_response_complete` uses `tokio::spawn` internally. That's the only path because `Drop` is synchronous and the trait method is async. The spawn semantics in P04 are unbounded; P05 wires them into a `JoinSet` for shutdown flush.
- The `&'static str` lifetime on `Hook::as_str()` matters here — it lets the registry pass hook names into `invoke()` and into `PluginError::Failed { hook: &'static str }` without allocation. Don't switch to `String`.
- `BackendTarget` is `Clone` in the current codebase (it carries `provider: String`, `upstream_model: UpstreamModel`, `policy: RoutePolicy` — all `Clone`). If a future refactor adds a non-`Clone` field, the registry will need a borrow-based path; fix it then, not preemptively.
- The integration test file is large but each test stands alone. If the implementer hits time pressure, prioritize in this order: (1) empty_registry_request_returns_200, (2) h2_plugin_fires_and_modifies_prompt, (3) plugin_aborted_returns_400, (4) plugin_failed_returns_502, (5) h7_plugin_fires_after_unary_response, (6) protected_field_mutation, (7) h3_plugin_fires_with_backend_target, (8) h5_plugin_drops_every_other_event, (9) h5_mid_stream_failure_emits_sse_error_frame.
- Don't try to unify `H7Guard` across unary + streaming. Unary doesn't have a Drop point — its H7 fire is inline. Two code paths is correct.
- The H7Guard captures `Arc<PluginRegistry>` — that's fine because the registry is itself in an `Arc<...>` on `AppCore` and outlives any in-flight request.
- This plan does NOT add registry construction from YAML — that's P06. T7's `AppCore::plugins` is `PluginRegistry::empty()` always. The integration tests use `AppState::new_with_plugins` to inject hand-built registries.
- If any task's implementer ends up needing to touch more than the modules listed in its **Files:** block, STOP and escalate — the plan is wrong and needs an update before proceeding.
