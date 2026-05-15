//! The request pipeline — the deep module that turns an inbound HTTP request
//! into an HTTP response.
//!
//! Each frontend handler is now a thin axum binding that builds a
//! [`PipelineSpec`] and calls [`dispatch`]. Everything between
//! "decode the body" and "write the response" — route resolution, fuzzy model
//! matching, [`agent_shim_core::RoutePolicy`] resolution, provider lookup,
//! `proxy_raw` short-circuit, streaming vs unary branch, request/response
//! logging — lives here.
//!
//! This is the **interface is the test surface**: behaviour that used to be
//! re-implemented across three handlers (with subtle drift) now sits behind
//! [`dispatch`] and can be tested through it once.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use parking_lot::Mutex;

use agent_shim_core::{
    BackendTarget, CanonicalRequest, CanonicalStream, ContentBlock, StreamEvent, Usage,
};
use agent_shim_frontends::{FrontendProtocol, FrontendResponse};
use agent_shim_providers::{ProviderCapabilities, ProviderError};
use agent_shim_router::{AgentIdentity, BreakerPolicy, CostFilterInputs, RetryPolicy};

use crate::handlers::{collect_stream, frontend_response_to_axum, HandlerError};
use crate::state::AppState;

/// Per-frontend configuration for [`dispatch`].
///
/// Keep this struct small and dumb: it captures the genuine variations
/// between frontends, nothing more. New variations should be expressed by
/// adding a field here, not by branching on `frontend.kind()` inside the
/// pipeline.
pub struct PipelineSpec<'a> {
    /// The frontend doing decode/encode for this request.
    pub frontend: &'a dyn FrontendProtocol,
    /// Endpoint label used in log lines (e.g. `/v1/messages`).
    pub endpoint_label: &'static str,
    /// If true, every `anthropic-*` header on the inbound request is captured
    /// and threaded through to the provider via the resolved policy. Today
    /// only the Anthropic frontend uses this.
    pub capture_anthropic_headers: bool,
    /// If true, attempt `provider.proxy_raw` before decoding the body. Today
    /// only the OpenAI Responses frontend uses this — its raw passthrough
    /// avoids a parse/re-encode round-trip when the upstream natively speaks
    /// the Responses API.
    pub try_proxy_raw: bool,
    /// If true, emit a final usage log line when the streaming response is
    /// dropped. The Anthropic handler did this with a drop-guard for
    /// SSE-shaped output; the other two log immediately after spawning.
    pub log_streaming_usage_on_drop: bool,
}

/// Run the request through the pipeline and produce an HTTP response.
pub async fn dispatch(
    state: &AppState,
    spec: PipelineSpec<'_>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    // Plan 03 P03 T3: root `gateway.request` span wrapping the dispatch
    // body. Fields with `tracing::field::Empty` are filled in once route +
    // identity are resolved.
    //
    // Plan 03 P03 T3 followup (IMPORTANT-1): the body is wrapped in
    // `async move { ... }.instrument(root_span).await` rather than held
    // open with `root_span.enter()`. Holding a `tracing::Entered` guard
    // across `.await` points is the documented anti-pattern — on the
    // multi-threaded runtime, child spans created after a suspension
    // point may not parent under the root correctly. `.instrument(...)`
    // attaches the span to the future's poll loop instead, which is
    // task-aware.
    //
    // `root_for_record` is a separate handle kept outside the instrumented
    // block so we can `record(...)` `http.response.status_code` and
    // `agent_shim.status_class` after the body returns. Recording fires
    // a span event (or attribute update) on the same span; the
    // `record` API does not require the span to be entered.
    let frontend_label = match spec.frontend.kind() {
        agent_shim_core::FrontendKind::AnthropicMessages => "anthropic_messages",
        agent_shim_core::FrontendKind::OpenAiChat => "openai_chat",
        agent_shim_core::FrontendKind::OpenAiResponses => "openai_responses",
    };

    // Plan 03 P03 T3 followup (IMPORTANT-4): generate a fresh request_id
    // here so it can be stamped on the root span. This matches the same
    // documented compromise used by `ResilientCaller::complete` — until
    // middleware-level plumbing lands, we generate a UUID at the pipeline
    // entry so operators see a stable id correlating with the resilience
    // events emitted downstream.
    let request_id = uuid::Uuid::new_v4().to_string();

    let root_span = tracing::info_span!(
        "gateway.request",
        "http.request.method" = "POST",
        "http.route" = spec.endpoint_label,
        "agent_shim.frontend" = frontend_label,
        "agent_shim.request_id" = %request_id,
        "agent_shim.route" = tracing::field::Empty,
        "agent_shim.identity" = tracing::field::Empty,
        "http.response.status_code" = tracing::field::Empty,
        "agent_shim.status_class" = tracing::field::Empty,
    );

    // Plan 03 P03 T4 followup: attach any inbound W3C `traceparent`
    // context to the freshly-created root span. The tower-layer shape
    // attempted previously was broken because `Span::current()` at the
    // layer call site is `Span::none()` — the per-request span hadn't
    // been created yet. Here we own the root span, so `set_parent` has
    // a real target. Malformed/missing headers yield an empty context
    // and `set_parent` is a safe no-op.
    {
        use tracing_opentelemetry::OpenTelemetrySpanExt;
        let parent_ctx = agent_shim_observability::extract_context_from_headers(&headers);
        root_span.set_parent(parent_ctx);
    }

    let root_for_record = root_span.clone();
    use tracing::Instrument;
    let result =
        async move { dispatch_inner(state, spec, headers, body, root_for_record.clone()).await }
            .instrument(root_span.clone())
            .await;

    // Plan 03 P03 T3 followup (IMPORTANT-3): record the HTTP status class
    // on the root span before it closes. `Ok(Response)` carries the
    // axum-level status (200 for streaming; 200 or 4xx/5xx for unary).
    // `Err(HandlerError)` is rendered to an HTTP status via the
    // `IntoResponse` impl downstream; we can't observe the final wire
    // status here without round-tripping through that impl, so we
    // approximate via the error variant. The exact wire status is also
    // available on the metrics middleware's `agent_shim_status_class`
    // label, so the operator-facing data is not lost.
    let (status_code, status_class): (u16, &'static str) = match &result {
        Ok(resp) => {
            let s = resp.status().as_u16();
            (s, status_class_for(s))
        }
        Err(e) => {
            // Best-effort: map the variant to a representative status code.
            // The middleware records the exact wire status; this is just
            // for the trace span's operator-visible class.
            let s = handler_error_status_hint(e);
            (s, status_class_for(s))
        }
    };
    root_span.record("http.response.status_code", status_code as i64);
    root_span.record("agent_shim.status_class", status_class);
    result
}

/// Map an HTTP status code to its `agent_shim.status_class` label.
fn status_class_for(status: u16) -> &'static str {
    match status {
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "other",
    }
}

/// Best-effort hint for the wire status that the `IntoResponse` impl
/// will emit for a given [`HandlerError`]. Used only to set the root
/// span's `agent_shim.status_class` when dispatch returns `Err(_)`; the
/// metrics middleware records the exact wire status on its own labels.
fn handler_error_status_hint(e: &HandlerError) -> u16 {
    use agent_shim_providers::ProviderError;
    use agent_shim_router::RouteError;
    match e {
        HandlerError::Route(RouteError::NoRoute { .. }) => 404,
        HandlerError::Unauthorized { .. } => 401,
        HandlerError::CapabilityMismatch { .. } => 400,
        HandlerError::Frontend(agent_shim_frontends::FrontendError::InvalidBody(_)) => 400,
        HandlerError::Frontend(agent_shim_frontends::FrontendError::Unsupported(_)) => 422,
        HandlerError::Frontend(_) => 400,
        HandlerError::Provider(ProviderError::Upstream { status, .. }) => *status,
        HandlerError::Provider(_) => 502,
        HandlerError::NoUpstreamSucceeded { .. }
        | HandlerError::AllBreakersOpen { .. }
        | HandlerError::NoEligibleUpstream { .. } => 503,
        HandlerError::RateLimited { .. } => 429,
        HandlerError::TerminalUpstream {
            error: ProviderError::Upstream { status, .. },
            ..
        } => *status,
        HandlerError::TerminalUpstream { .. } => 502,
        HandlerError::PluginFailed { aborted: true, .. } => 400,
        HandlerError::PluginFailed { aborted: false, .. } => 502,
    }
}

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
            // Aborted isn't tied to one hook; the diagnostic is the
            // operator-facing message, not the hook string.
            hook: "aborted",
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

/// Body of [`dispatch`], pulled into a separate async fn so the parent can
/// wrap it with `.instrument(root_span)` rather than holding an `Entered`
/// guard across `.await` (Plan 03 P03 T3 followup, IMPORTANT-1).
async fn dispatch_inner(
    state: &AppState,
    spec: PipelineSpec<'_>,
    headers: HeaderMap,
    body: Bytes,
    root_span: tracing::Span,
) -> Result<Response, HandlerError> {
    let body_bytes = body.len();
    let started = std::time::Instant::now();

    // Plan 01 P01 T2: capture the policy snapshot once. In-flight requests
    // use this snapshot for their entire lifetime; mid-request reload (Plan
    // 04) does not affect them.
    let snapshot = state.snapshot.load_full();

    let frontend_label = match spec.frontend.kind() {
        agent_shim_core::FrontendKind::AnthropicMessages => "anthropic_messages",
        agent_shim_core::FrontendKind::OpenAiChat => "openai_chat",
        agent_shim_core::FrontendKind::OpenAiResponses => "openai_responses",
    };

    // Resolve the route up front. The Responses passthrough path needs the
    // target before it has decoded the body, so route resolution has to come
    // first regardless of which path we take.
    let model_alias = if spec.try_proxy_raw {
        // For raw passthrough we have to peek at the body for the model.
        extract_model_from_body(&body)?
    } else {
        // Otherwise we'll know the model after decode; use a placeholder for
        // the route lookup until decode runs. The decode path below replaces
        // this.
        String::new()
    };

    let mut decoded: Option<agent_shim_core::CanonicalRequest> = None;
    let model_alias = if spec.try_proxy_raw {
        model_alias
    } else {
        let canonical = spec
            .frontend
            .decode_request(&body)
            .map_err(HandlerError::Frontend)?;
        let alias = canonical.model.as_str().to_string();
        decoded = Some(canonical);
        alias
    };

    // Plan 04 P04 T4: extract identity + run the auth.required gate
    // BEFORE route resolution. Both are in-memory hash lookups so order
    // is a security choice rather than a perf one: returning 401 ahead
    // of 404 prevents an unauthenticated probe from enumerating which
    // routes exist. When `auth.enabled = false` we skip extraction —
    // identity is always Anonymous and the rate-limit registry is
    // separately gated by its own `enabled=false` short-circuit.
    //
    // Plan 03 P03 T3: wrap the auth gate in an `auth.verify` child span.
    let identity = {
        let _auth_span = tracing::info_span!("auth.verify").entered();
        if snapshot.auth_enabled {
            let authz = headers.get("authorization").and_then(|v| v.to_str().ok());
            let xkey = headers.get("x-api-key").and_then(|v| v.to_str().ok());
            let extracted = agent_shim_router::extract_identity_from_headers(authz, xkey);

            if snapshot.auth_required {
                match &extracted {
                    AgentIdentity::Anonymous => {
                        tracing::warn!(
                            endpoint = spec.endpoint_label,
                            "auth.required=true but request had no Authorization or x-api-key header"
                        );
                        return Err(HandlerError::Unauthorized {
                            kind: spec.frontend.kind(),
                        });
                    }
                    AgentIdentity::KeyHash(h) => {
                        // Non-constant-time compare on already-hashed values
                        // is fine: the hash itself is not a secret, so timing
                        // leaks reveal nothing about the original key bytes.
                        if !snapshot.configured_key_hashes.contains(h) {
                            // Operator log carries the hash (NEVER plaintext) so
                            // a misconfigured client can be tracked down without
                            // leaking the secret.
                            tracing::warn!(
                                endpoint = spec.endpoint_label,
                                presented_hash = %h,
                                "auth.required=true but presented key not in auth.keys allowlist"
                            );
                            return Err(HandlerError::Unauthorized {
                                kind: spec.frontend.kind(),
                            });
                        }
                    }
                }
            }
            extracted
        } else {
            AgentIdentity::Anonymous
        }
    };

    // Plan 03 P03 T3: record identity on the root span. Uses the same
    // anonymous / sha256-hash form as the resilience-event logs so
    // operators can correlate across span attributes and event fields.
    let identity_str = match &identity {
        AgentIdentity::Anonymous => "anonymous",
        AgentIdentity::KeyHash(h) => h.as_str(),
    };
    root_span.record("agent_shim.identity", identity_str);

    // Plan 03 P03 T3: `route.resolve` child span wrapping the resolver call.
    let chain = {
        let _span = tracing::info_span!("route.resolve").entered();
        state
            .core
            .resolver
            .resolve(spec.frontend.kind(), &model_alias)
            .map_err(|e| {
                tracing::warn!(model = %model_alias, error = %e, "no route");
                HandlerError::Route(e)
            })?
    };

    // Plan 03 P03 T3: now that route is resolved, record it on the root.
    root_span.record("agent_shim.route", model_alias.as_str());

    // Plan 02 P02 T5: emit a route-labeled counter parallel to the
    // middleware's `route="unknown"` counter. Operators query either
    // depending on whether they care about pre-resolution failures
    // (404 routes) or post-resolution traffic. The `.increment(0)`
    // registers the label set in /metrics without skewing the count.
    metrics::counter!(
        agent_shim_observability::metrics::names::REQUESTS_TOTAL,
        "frontend" => frontend_label,
        "route" => model_alias.clone(),
        "status_class" => "pending",
    )
    .increment(0);

    // Plan 04 P02 D2: route-level retry block governs every chain element
    // uniformly. Build one `RetryPolicy` and clone it per chain position so
    // `ResilientCaller::complete` can apply it independently to each
    // upstream's retry budget.
    let route_retry_config = match state
        .core
        .resolver
        .find_retry_policy(spec.frontend.kind(), &model_alias)
    {
        Some(c) => c,
        None => {
            tracing::debug!(
                model = %model_alias,
                "no route-specific retry policy; using RetryConfig defaults"
            );
            Default::default()
        }
    };
    let retry_policy = RetryPolicy::from(&route_retry_config);
    let policies: Vec<RetryPolicy> = vec![retry_policy; chain.len()];

    // Plan 04 P03 T4: parallel breaker policies — same uniform-per-route
    // shape as retry, fed into `ResilientCaller::complete` alongside the
    // retry policies. Falls back to `BreakerConfig::default()` (the §4.5/D6
    // defaults) when the route doesn't override.
    let route_breaker_config = match state
        .core
        .resolver
        .find_breaker_policy(spec.frontend.kind(), &model_alias)
    {
        Some(c) => c,
        None => {
            tracing::debug!(
                model = %model_alias,
                "no route-specific breaker policy; using BreakerConfig defaults"
            );
            Default::default()
        }
    };
    let breaker_policy = BreakerPolicy::from(&route_breaker_config);
    let breaker_policies: Vec<BreakerPolicy> = vec![breaker_policy; chain.len()];

    // Plan 04 P04 T4: build `client_ip` and `frontend_model` for the
    // rate-limit gates. These depend on `model_alias`, so they're built
    // AFTER the auth.required check but BEFORE the chain walk.
    //
    // `x-forwarded-for` is client-controlled when no reverse proxy
    // strips/rewrites it, so the per-IP bucket is trivially bypassable
    // by direct-internet-exposed deployments. Operators should front the
    // gateway with a known proxy that asserts the header. Threading the
    // socket peer addr through the handler signature is a v0.5 P5 concern.
    let client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Per-route bucket key: `<frontend_kind>/<model_alias>`. Matches
    // the format the YAML config uses for `rate_limit.per_route` keys
    // (see Plan 04 P04 T3).
    let frontend_kind_str = match spec.frontend.kind() {
        agent_shim_core::FrontendKind::AnthropicMessages => "anthropic_messages",
        agent_shim_core::FrontendKind::OpenAiChat => "openai_chat",
        agent_shim_core::FrontendKind::OpenAiResponses => "openai_responses",
    };
    let frontend_model = format!("{frontend_kind_str}/{model_alias}");

    // The chain is non-empty by router invariant (singular routes produce
    // 1-element vecs; array routes are validated to be non-empty by
    // `validate_routes`). The capability gate, the request log line, and
    // the proxy_raw short-circuit all run against `chain[0]`'s provider —
    // chain-element-aware capability checking is intentionally out of
    // scope for v0.4 (potential P03+ extension).
    let first_target = chain
        .first()
        .expect("ModelResolver::resolve never returns an empty chain on Ok")
        .clone();
    let upstream_model = first_target.model.clone();

    let first_provider = state
        .core
        .providers
        .get(&first_target.provider)
        .ok_or_else(|| {
            tracing::error!(provider = %first_target.provider, "provider not registered");
            HandlerError::Provider(agent_shim_providers::ProviderError::UnknownProvider(
                first_target.provider.clone(),
            ))
        })?;

    // Raw-passthrough short-circuit: only the Responses frontend uses this.
    // We don't know reasoning_effort etc. without decoding the body, so log
    // the route default as the best approximation. The passthrough path is
    // single-upstream by nature: it bypasses the canonical decode/encode
    // round-trip and so cannot itself participate in chain fallback.
    //
    // Plan 04 P02 T6: when proxy_raw returns `Err`, fall through to the
    // canonical decode path so the chain walk (`ResilientCaller`) gets the
    // chance to try chain[1..]. The cost is an extra attempt at chain[0]
    // via `complete()` — likely also failing — before fallback fires, but
    // proxy_raw failures are rare and the alternative (terminating on
    // chain[0]'s passthrough error and skipping fallback) violates the
    // user-visible contract that a non-empty `upstreams: [...]` chain
    // tries every element on eligible failures.
    //
    // `Ok(None)` (proxy_raw not applicable for this frontend) and `Err(_)`
    // both fall through. Successful proxy_raw returns the stream as-is —
    // resilience-layer handling for that case is intentionally out of
    // scope for v0.4.
    //
    // Plan 04 P04 T5 — KNOWN GAP (v0.5): when proxy_raw returns
    // `Ok(Some(...))` and serves the response without a canonical
    // round-trip, the rate-limit and breaker gates are bypassed
    // entirely. Both gates live inside `ResilientCaller::complete`,
    // which the proxy_raw fast path skips. Gating at the top of the
    // `try_proxy_raw` block double-consumes when proxy_raw falls
    // through to the canonical path; gating only inside `Ok(Some(...))`
    // means proxy_raw has already committed upstream resources by the
    // time we'd reject. Either fix needs a small refactor (e.g. an
    // explicit RateLimitTicket value that's only consumed when the
    // request actually completes) — out of scope for v0.4. The
    // `crates/gateway/tests/rate_limit_per_key_envelope.rs` Anthropic
    // test routes through an OAI-compat upstream specifically to
    // exercise the canonical-path gate.
    if spec.try_proxy_raw {
        tracing::info!(
            "→ {} | model: {} → {} | bodyBytes: {} | reasoning_default: {}",
            spec.endpoint_label,
            model_alias,
            upstream_model,
            body_bytes,
            first_target
                .policy
                .default_reasoning_effort
                .map(|e| e.as_str())
                .unwrap_or("none"),
        );

        match first_provider
            .proxy_raw(body.clone(), first_target.clone(), spec.frontend.kind())
            .await
        {
            Ok(Some((content_type, byte_stream))) => {
                tracing::info!(
                    "← {} (passthrough) | model: {} → {} | {:.1}s",
                    spec.endpoint_label,
                    model_alias,
                    upstream_model,
                    started.elapsed().as_secs_f64()
                );
                let body_stream =
                    Body::from_stream(byte_stream.map(|r| r.map_err(|e| e.to_string())));
                let mut r = Response::new(body_stream);
                r.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    HeaderValue::from_str(&content_type)
                        .unwrap_or_else(|_| HeaderValue::from_static("text/event-stream")),
                );
                return Ok(r);
            }
            Ok(None) => {
                // Frontend not supported by this provider's passthrough —
                // continue to canonical decode below.
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "proxy_raw failed; falling through to canonical chain walk"
                );
            }
        }

        // Passthrough not used (or failed) — fall through with a fresh decode.
        let canonical = spec
            .frontend
            .decode_request(&body)
            .map_err(HandlerError::Frontend)?;
        decoded = Some(canonical);
    }

    let mut canonical = decoded.expect("canonical request was decoded above");

    if spec.capture_anthropic_headers {
        canonical.inbound_anthropic_headers = capture_anthropic_headers(&headers);
    }

    // ── Plan 07 P04 spec §6.6 anchor 1: H2 (on_decoded_request) ───────
    // After decode + header capture, before route resolution. The
    // registry walks the route's H2 chain; protected-field violations
    // and `on_error: fail` propagate as `HandlerError::PluginFailed`.
    let frontend_kind_for_hooks = spec.frontend.kind();
    let plugin_ctx = agent_shim_plugins::PluginContext {
        request_id: canonical.id.clone(),
        frontend: frontend_kind_for_hooks,
        route_label: format!("{:?}/{}", frontend_kind_for_hooks, model_alias),
    };
    canonical = state
        .core
        .plugins
        .run_on_decoded_request(
            (frontend_kind_for_hooks, model_alias.as_str()),
            &plugin_ctx,
            canonical,
        )
        .await
        .map_err(|e| handler_error_from_plugin_error(e, frontend_kind_for_hooks))?;

    // Snapshot the merged route policy onto the canonical request. Providers
    // and logging both read from `resolved_policy` so the merge rule lives in
    // exactly one place (RoutePolicy::resolve). v0.4 D2: route-level policy
    // is uniform across the chain; resolving against `chain[0]`'s policy is
    // correct because every chain element shares the same `RoutePolicy`.
    canonical.resolved_policy = first_target.policy.resolve(&canonical);

    // Capability gate. Reject before any network call when the request asks
    // for a feature the target provider can't deliver — today this is just
    // vision (image blocks against a text-only backend). The check has to
    // run after decode (so we can scan content blocks) but before the chain
    // walk (so we never reach the upstream). The check inspects only
    // `chain[0]`'s capabilities; chain-element-aware gating is out of
    // scope for v0.4.
    check_capabilities(&canonical, first_provider.capabilities()).map_err(|e| {
        tracing::warn!(
            provider = %first_target.provider,
            error = %e,
            "capability mismatch — rejecting before upstream call"
        );
        HandlerError::CapabilityMismatch {
            kind: spec.frontend.kind(),
            message: e.to_string(),
        }
    })?;

    let is_stream = canonical.stream;
    let max_tokens = canonical.generation.max_tokens;
    let reasoning_budget = canonical
        .generation
        .reasoning
        .as_ref()
        .and_then(|r| r.budget_tokens);
    let beta_log = canonical
        .resolved_policy
        .anthropic_header("anthropic-beta")
        .map(|s| s.to_string());

    tracing::info!(
        "→ {} | model: {} → {} | bodyBytes: {} | maxTokens: {} | stream: {} | reasoning: {}{}{}",
        spec.endpoint_label,
        model_alias,
        upstream_model,
        body_bytes,
        max_tokens.unwrap_or(0),
        is_stream,
        canonical
            .resolved_policy
            .reasoning_effort
            .map(|e| e.as_str())
            .unwrap_or("none"),
        reasoning_budget
            .map(|b| format!(" (budget {} tok)", b))
            .unwrap_or_default(),
        beta_log
            .as_deref()
            .map(|b| format!(" | beta: {}", b))
            .unwrap_or_default(),
    );

    // Walk the chain via `ResilientCaller`: per-element retry, eligible
    // errors fall through to the next chain element, terminal errors
    // short-circuit. All resilience-shaped errors flow through the
    // `from_resilience_error` bridge so the response envelope is shaped
    // by the inbound frontend dialect.
    //
    // Plan 06 P04 T4: the cost-filter pre-pass runs INSIDE
    // `complete_with_cost_filter`. We pass the matching route entry +
    // gateway config so the filter can read `min_tier` /
    // `max_cost_usd` / per-upstream `tier` / `cost` /
    // `p95_latency_budget_ms`. The route lookup walks
    // `snapshot.config.routes` once; if no entry matches the route
    // (wildcard-only routes have model `*` so the linear scan
    // intentionally falls through to wildcard semantics) we pass
    // `None` and the filter step is skipped.
    let frontend_kind = spec.frontend.kind();
    if is_stream {
        run_stream(
            spec,
            state,
            canonical,
            chain,
            policies,
            breaker_policies,
            identity,
            client_ip,
            frontend_model,
            snapshot.clone(),
            RunContext {
                model_alias,
                upstream_model,
                started,
                frontend_kind,
            },
        )
        .await
    } else {
        run_unary(
            spec,
            state,
            canonical,
            chain,
            policies,
            breaker_policies,
            identity,
            client_ip,
            frontend_model,
            snapshot.clone(),
            RunContext {
                model_alias,
                upstream_model,
                started,
                frontend_kind,
            },
        )
        .await
    }
}

/// Per-request context shared between the streaming and unary branches.
struct RunContext {
    model_alias: String,
    upstream_model: String,
    started: std::time::Instant,
    /// The inbound frontend's wire dialect. Threaded into the run helpers
    /// so resilience-error → HandlerError mapping can shape the response
    /// envelope per the client's expected schema.
    frontend_kind: agent_shim_core::FrontendKind,
}

// `#[allow(clippy::too_many_arguments)]`: matches the signature of
// `ResilientCaller::complete` plus the per-frontend pipeline spec and
// the run-context bundle. Bundling identity/client_ip/frontend_model
// into a struct would just move the verbosity to the caller.
#[allow(clippy::too_many_arguments)]
async fn run_stream(
    spec: PipelineSpec<'_>,
    state: &AppState,
    canonical: CanonicalRequest,
    chain: Vec<BackendTarget>,
    policies: Vec<RetryPolicy>,
    breaker_policies: Vec<BreakerPolicy>,
    identity: AgentIdentity,
    client_ip: String,
    frontend_model: String,
    snapshot: Arc<crate::state::AppSnapshot>,
    ctx: RunContext,
) -> Result<Response, HandlerError> {
    let label = spec.endpoint_label;
    let RunContext {
        model_alias,
        upstream_model,
        started,
        frontend_kind,
    } = ctx;

    // Plan 07 P04 T7: build a `PluginContext` once for this streaming
    // request. Used by:
    //   - the H7Guard's Drop fire (both Anthropic and OpenAI streaming).
    //   - the H5 wrap_stream call (P04 T10).
    //   - the H2/H3 anchors live earlier in dispatch_inner so they
    //     build their own context up there. The `request_id` is captured
    //     here from the canonical request because this instance is
    //     scoped to `run_stream`. We clone the id because `canonical` is
    //     consumed later by the chain walk and `RequestId` is not Copy.
    let plugin_ctx_for_run = agent_shim_plugins::PluginContext {
        request_id: canonical.id.clone(),
        frontend: frontend_kind,
        route_label: format!("{:?}/{}", frontend_kind, model_alias),
    };

    // Plan 06 P04 T4: dispatch through the cost-filter-aware entry
    // point when we can locate the matching `RouteEntry`. Wildcard
    // routes and routes without `min_tier` / `max_cost_usd` /
    // per-upstream budgets pay the filter cost but skip every axis
    // — the filter degenerates to the identity function.
    let route_entry = find_route_entry(&snapshot.config, frontend_kind, &model_alias);
    let upstream_stream_result = match route_entry {
        Some(route) => {
            // Plan v0.6.1 P04 T7 (M-8): pick the per-frontend image-token
            // estimator so the cost-cap pre-pass charges image blocks at
            // the same rate the upstream will bill.
            let image_estimator =
                crate::image_estimator_selector::select_image_estimator(frontend_kind);
            state
                .core
                .resilient_caller
                .complete_with_cost_filter(
                    chain,
                    canonical,
                    policies,
                    breaker_policies,
                    identity,
                    client_ip,
                    frontend_model.clone(),
                    CostFilterInputs {
                        route,
                        config: &snapshot.config,
                        route_label: frontend_model,
                        image_estimator,
                    },
                )
                .await
        }
        None => {
            state
                .core
                .resilient_caller
                .complete(
                    chain,
                    canonical,
                    policies,
                    breaker_policies,
                    identity,
                    client_ip,
                    frontend_model,
                )
                .await
        }
    };
    // Walk the chain. `ResilientCaller::complete` returns a single
    // `CanonicalStream` from whichever upstream succeeded; downstream
    // encoding is unchanged from the pre-T6 single-upstream world.
    let upstream_stream = upstream_stream_result.map_err(|e| {
        tracing::error!(error = %e, "resilient call failed");
        HandlerError::from_resilience_error(e, frontend_kind)
    })?;

    if spec.log_streaming_usage_on_drop {
        // Anthropic-style: log final usage when the SSE stream is dropped.
        let usage_capture: Arc<Mutex<Option<Usage>>> = Arc::new(Mutex::new(None));
        let guard = H7Guard {
            endpoint_label: label,
            model_alias: model_alias.clone(),
            upstream_model: upstream_model.clone(),
            usage: usage_capture.clone(),
            started,
            // Plan 07 P04 T7: universal H7 firing.
            registry: Some(state.core.plugins.clone()),
            route: Some((frontend_kind, model_alias.clone())),
            ctx: Some(plugin_ctx_for_run.clone()),
        };

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
        // Plan 03 P03 T3: `stream.encode` span around the frontend's stream
        // encoder. The span guard drops before the response is returned —
        // the actual byte-streaming work happens inside the axum body
        // afterward, outside this span.
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
    } else {
        // Plan 07 P04 T7: universal H7 guard across all streaming
        // frontends. Today's OpenAI Chat / Responses branches did not
        // use a Drop guard; they now do, so `on_response_complete`
        // fires regardless of inbound frontend (§6.7).
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
}

// `#[allow(clippy::too_many_arguments)]`: see `run_stream` above —
// same justification, the parameters describe the full chain-walk
// context.
#[allow(clippy::too_many_arguments)]
async fn run_unary(
    spec: PipelineSpec<'_>,
    state: &AppState,
    canonical: CanonicalRequest,
    chain: Vec<BackendTarget>,
    policies: Vec<RetryPolicy>,
    breaker_policies: Vec<BreakerPolicy>,
    identity: AgentIdentity,
    client_ip: String,
    frontend_model: String,
    snapshot: Arc<crate::state::AppSnapshot>,
    ctx: RunContext,
) -> Result<Response, HandlerError> {
    let RunContext {
        model_alias,
        upstream_model,
        started,
        frontend_kind,
    } = ctx;

    // Plan 06 P04 T4: see `run_stream` — cost filter pre-pass via
    // `complete_with_cost_filter` when a route entry is locatable.
    let route_entry = find_route_entry(&snapshot.config, frontend_kind, &model_alias);
    let stream_result = match route_entry {
        Some(route) => {
            // Plan v0.6.1 P04 T7 (M-8): per-frontend image-token estimator
            // selection — see `run_stream` for the matching wiring on the
            // streaming path.
            let image_estimator =
                crate::image_estimator_selector::select_image_estimator(frontend_kind);
            state
                .core
                .resilient_caller
                .complete_with_cost_filter(
                    chain,
                    canonical,
                    policies,
                    breaker_policies,
                    identity,
                    client_ip,
                    frontend_model.clone(),
                    CostFilterInputs {
                        route,
                        config: &snapshot.config,
                        route_label: frontend_model,
                        image_estimator,
                    },
                )
                .await
        }
        None => {
            state
                .core
                .resilient_caller
                .complete(
                    chain,
                    canonical,
                    policies,
                    breaker_policies,
                    identity,
                    client_ip,
                    frontend_model,
                )
                .await
        }
    };
    let stream = stream_result.map_err(|e| {
        tracing::error!(error = %e, "resilient call failed");
        HandlerError::from_resilience_error(e, frontend_kind)
    })?;
    let response = collect_stream(stream).await?;
    let (input, output) = match &response.usage {
        Some(u) => (u.input_tokens.unwrap_or(0), u.output_tokens.unwrap_or(0)),
        None => (0, 0),
    };
    tracing::info!(
        "← {} (unary) | model: {} → {} | input: {} | output: {} | {:.1}s",
        spec.endpoint_label,
        model_alias,
        upstream_model,
        input,
        output,
        started.elapsed().as_secs_f64()
    );
    let frontend_response = spec
        .frontend
        .encode_unary(response)
        .map_err(HandlerError::Frontend)?;
    Ok(frontend_response_to_axum(frontend_response))
}

// ── internals ─────────────────────────────────────────────────────────────

/// Plan 06 P04 T4: locate the `RouteEntry` matching `(frontend, model)`
/// in the snapshot config. Returns `None` when no entry matches — in
/// that case the caller falls back to `complete()` (no cost filter).
///
/// The lookup mirrors `StaticRouter::from_config` resolution order:
/// a specific `(frontend, model)` route wins over a `(frontend, "*")`
/// wildcard. We don't cache this; routes count is small (typical
/// configs: 5-30 entries) and the lookup runs once per request.
fn find_route_entry<'a>(
    config: &'a agent_shim_config::GatewayConfig,
    frontend: agent_shim_core::FrontendKind,
    model: &str,
) -> Option<&'a agent_shim_config::RouteEntry> {
    let frontend_str = match frontend {
        agent_shim_core::FrontendKind::AnthropicMessages => "anthropic_messages",
        agent_shim_core::FrontendKind::OpenAiChat => "openai_chat",
        agent_shim_core::FrontendKind::OpenAiResponses => "openai_responses",
    };
    let alt = match frontend {
        agent_shim_core::FrontendKind::AnthropicMessages => "anthropic",
        agent_shim_core::FrontendKind::OpenAiChat => "openai",
        agent_shim_core::FrontendKind::OpenAiResponses => "responses",
    };
    // Specific match first.
    let specific = config
        .routes
        .iter()
        .find(|r| (r.frontend == frontend_str || r.frontend == alt) && r.model == model);
    if specific.is_some() {
        return specific;
    }
    // Wildcard fallback.
    config
        .routes
        .iter()
        .find(|r| (r.frontend == frontend_str || r.frontend == alt) && r.model == "*")
}

/// Pull every `anthropic-*` header off the inbound request, dropping the two
/// credential headers that are upstream-owned. Returns name/value pairs in
/// arrival order so providers can replay them verbatim.
fn capture_anthropic_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, value) in headers.iter() {
        let name_str = name.as_str();
        if !name_str.starts_with("anthropic-") {
            continue;
        }
        if matches!(name_str, "anthropic-api-key" | "anthropic-auth-token") {
            continue;
        }
        if let Ok(v) = value.to_str() {
            out.push((name_str.to_string(), v.to_string()));
        }
    }
    out
}

/// Peek at the JSON body to extract the `model` field for raw-passthrough
/// route resolution.
fn extract_model_from_body(body: &[u8]) -> Result<String, HandlerError> {
    #[derive(serde::Deserialize)]
    struct Minimal {
        model: String,
    }
    let m: Minimal = serde_json::from_slice(body).map_err(|e| {
        HandlerError::Frontend(agent_shim_frontends::FrontendError::InvalidBody(format!(
            "cannot extract model: {e}"
        )))
    })?;
    Ok(m.model)
}

/// Reject the request before dispatch when the inbound canonical body asks
/// for a feature the target provider cannot service. Today this is just
/// vision (any `ContentBlock::Image` against a backend whose
/// `capabilities.vision == false`); future capability mismatches add new
/// branches here.
///
/// Plan 04 D6: the check runs at the gateway boundary, not inside each
/// provider, so the error envelope is shaped by the inbound frontend (not
/// whichever upstream happened to be wired). Returning `Err` here aborts
/// before any network I/O, which is also what the mockito-based test in
/// Plan 04 T9 asserts (zero hits on the upstream mock).
fn check_capabilities(
    req: &CanonicalRequest,
    caps: &ProviderCapabilities,
) -> Result<(), ProviderError> {
    if request_has_image(req) && !caps.vision {
        return Err(ProviderError::CapabilityMismatch(
            "target provider does not support vision (image blocks present in request)".into(),
        ));
    }
    Ok(())
}

/// Scan every message's content blocks for an `Image` variant. Used by the
/// capability gate; pulled out so unit tests can exercise the predicate
/// against handcrafted requests without spinning up a full pipeline.
fn request_has_image(req: &CanonicalRequest) -> bool {
    req.messages
        .iter()
        .flat_map(|m| m.content.iter())
        .any(|b| matches!(b, ContentBlock::Image(_)))
}

/// Plan 07 P04 T6/T7: universal H7 guard. Fires `run_on_response_complete`
/// on the registry when the SSE stream finishes (or is dropped early by
/// client disconnect). Replaces the Anthropic-only `StreamLogger` of v0.6
/// — wiring for the registry/route/ctx happens in T7, so the new fields
/// are `Option<_>` for now (None == log only, no H7 invocation).
///
/// The structured-log line (today's "← {label} (stream) | ..." emission)
/// remains; it's folded into the Drop alongside the H7 firing so both
/// happen at the same lifecycle point.
struct H7Guard {
    endpoint_label: &'static str,
    model_alias: String,
    upstream_model: String,
    usage: Arc<Mutex<Option<Usage>>>,
    started: std::time::Instant,
    /// `None` means "log only, no H7 invocation" — T6 ships this state.
    /// T7 wires the registry/route/ctx for production callers.
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
            registry.run_on_response_complete((route.0, route.1.as_str()), ctx, summary);
        }
    }
}

struct GuardedStream<S> {
    inner: S,
    _guard: H7Guard,
}

impl<S: futures::Stream + Unpin> futures::Stream for GuardedStream<S> {
    type Item = S::Item;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_model_from_body_reads_top_level_field() {
        let body = br#"{"model":"gpt-4o","messages":[]}"#;
        assert_eq!(extract_model_from_body(body).unwrap(), "gpt-4o");
    }

    #[test]
    fn extract_model_from_body_rejects_missing_model() {
        let body = br#"{"messages":[]}"#;
        let err = extract_model_from_body(body).unwrap_err();
        assert!(matches!(err, HandlerError::Frontend(_)));
    }

    #[test]
    fn capture_anthropic_headers_filters_prefix_and_drops_credentials() {
        let mut h = HeaderMap::new();
        h.insert("content-type", "application/json".parse().unwrap());
        h.insert("anthropic-beta", "context-1m-2025-08-07".parse().unwrap());
        h.insert("anthropic-version", "2023-06-01".parse().unwrap());
        h.insert(
            "anthropic-api-key",
            "secret-should-be-dropped".parse().unwrap(),
        );
        h.insert("ANTHROPIC-AUTH-TOKEN", "another-secret".parse().unwrap());

        let captured = capture_anthropic_headers(&h);
        let names: Vec<&str> = captured.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"anthropic-beta"));
        assert!(names.contains(&"anthropic-version"));
        assert!(!names.iter().any(|n| n.contains("api-key")));
        assert!(!names.iter().any(|n| n.contains("auth-token")));
        assert!(!names.contains(&"content-type"));
    }

    // ── capability gate ───────────────────────────────────────────────

    use agent_shim_core::{
        media::BinarySource, request::RequestMetadata, ContentBlock, ExtensionMap, FrontendInfo,
        FrontendKind, FrontendModel, GenerationOptions, ImageBlock, Message, MessageRole,
        RequestId, ResolvedPolicy,
    };

    /// Build a CanonicalRequest carrying a single user message whose content
    /// blocks come from the caller. Keeps every other field at its default
    /// — the gate only inspects `messages[*].content`, so this is enough.
    fn req_with_content(blocks: Vec<ContentBlock>) -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: FrontendModel::from("test-model"),
            },
            model: FrontendModel::from("test-model"),
            system: vec![],
            messages: vec![Message {
                role: MessageRole::User,
                content: blocks,
                name: None,
                extensions: ExtensionMap::new(),
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

    fn img_block() -> ContentBlock {
        ContentBlock::Image(ImageBlock {
            source: BinarySource::Url {
                url: "https://example.com/cat.png".into(),
            },
            extensions: Default::default(),
        })
    }

    #[test]
    fn request_has_image_detects_image_block() {
        let req = req_with_content(vec![ContentBlock::text("hi"), img_block()]);
        assert!(request_has_image(&req));
    }

    #[test]
    fn request_has_image_returns_false_for_text_only() {
        let req = req_with_content(vec![ContentBlock::text("hi")]);
        assert!(!request_has_image(&req));
    }

    #[test]
    fn check_capabilities_passes_when_no_image_and_no_vision() {
        // Provider without vision is still fine for a text-only request.
        let caps = ProviderCapabilities {
            streaming: true,
            tool_use: false,
            vision: false,
            json_mode: false,
        };
        let req = req_with_content(vec![ContentBlock::text("hi")]);
        assert!(check_capabilities(&req, &caps).is_ok());
    }

    #[test]
    fn check_capabilities_passes_when_image_and_vision() {
        let caps = ProviderCapabilities {
            streaming: true,
            tool_use: false,
            vision: true,
            json_mode: false,
        };
        let req = req_with_content(vec![ContentBlock::text("describe"), img_block()]);
        assert!(check_capabilities(&req, &caps).is_ok());
    }

    #[test]
    fn check_capabilities_rejects_image_against_text_only_provider() {
        let caps = ProviderCapabilities {
            streaming: true,
            tool_use: false,
            vision: false,
            json_mode: false,
        };
        let req = req_with_content(vec![img_block()]);
        let err = check_capabilities(&req, &caps).unwrap_err();
        // Variant + message both observable so future refactors can't quietly
        // turn this into a different error class.
        match err {
            ProviderError::CapabilityMismatch(msg) => {
                assert!(msg.contains("vision"), "message must mention vision: {msg}");
            }
            other => panic!("expected CapabilityMismatch, got {other:?}"),
        }
    }
}
