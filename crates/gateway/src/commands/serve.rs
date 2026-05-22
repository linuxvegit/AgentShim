use anyhow::Result;
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use agent_shim_config::GatewayConfig;

/// Phase 1 P01: shared core of the foreground `serve` command and the
/// (future) Windows Service SCM entry point. Builds the application state,
/// spawns the reload-applying task and (on Unix) the SIGHUP listener,
/// binds the listener(s), invokes `on_listening` with the bound public
/// address once it is accepting connections, then serves until
/// `shutdown_signal` resolves.
///
/// Callers that load a config from disk should pass `config_path =
/// Some(path)` so `AppCore::config_path` is populated and SIGHUP /
/// `POST /admin/reload` (with no body) can re-read the original file.
/// Callers constructing a config in memory (tests, future programmatic
/// embedding) pass `None`.
pub async fn run_core<S, L>(
    cfg: GatewayConfig,
    config_path: Option<PathBuf>,
    shutdown_signal: S,
    on_listening: L,
) -> anyhow::Result<()>
where
    S: Future<Output = ()> + Send + 'static,
    L: FnOnce(SocketAddr) + Send + 'static,
{
    // Phase 1 P01: validation is the caller's responsibility — `run`
    // validates the on-disk config before invoking us. In-memory callers
    // (tests, the future Windows Service SCM entry) pass pre-validated
    // configs or skip validation entirely (e.g. tests that bind port 0
    // via TcpListener and rely on the OS to pick a free port).
    let tracing_handles = agent_shim_observability::init(&cfg.logging, cfg.otel.as_ref());
    let (mut state, mut reload_rx) = crate::state::AppState::new(cfg).await?;

    // Plan 04 P04 T4: pin the `--config` path onto `AppCore` so the
    // SIGHUP listener and `POST /admin/reload` (with no body) can re-read
    // the original file. `AppState::new` constructs `AppCore` without a
    // path because it's a `GatewayConfig`-in-memory constructor; we fork
    // a fresh `AppCore` here with the path filled in. Cheap — every
    // field is `Arc`-shaped.
    let core_with_path = Arc::new(crate::state::AppCore {
        config_path: config_path.clone(),
        ..(*state.core).clone()
    });
    state.core = core_with_path;

    // Plan 04 P04 T4: reload-applying task. Drains `reload_rx`; for each
    // request, parses YAML / re-reads disk, validates against the
    // `AppCore` baseline, builds a fresh `AppSnapshot`, atomic-swaps.
    // The mpsc has bounded capacity (8 in `AppState::build`) so this
    // task is the single serializer of reload events — no two swaps can
    // race for the ArcSwap. Spec §5.2.
    {
        let state_for_task = state.clone();
        tokio::spawn(async move {
            while let Some(req) = reload_rx.recv().await {
                let outcome = handle_reload(&state_for_task, req.source).await;
                let _ = req.respond_to.send(outcome);
            }
        });
    }

    // Plan 04 P04 T4: SIGHUP listener (Unix only). Each `SIGHUP` enqueues
    // a `ReloadSource::Sighup` into the same channel so reload semantics
    // match `POST /admin/reload` with no body. The oneshot reply is
    // discarded — operators reading server logs to confirm a SIGHUP reload
    // landed should look for the `agent_shim::reload` tracing target.
    #[cfg(unix)]
    {
        let reload_tx = state.core.reload_tx.clone();
        tokio::spawn(async move {
            let mut sig =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to install SIGHUP handler");
                        return;
                    }
                };
            while sig.recv().await.is_some() {
                let (tx, _rx) = tokio::sync::oneshot::channel();
                let _ = reload_tx
                    .send(crate::reload_trigger::ReloadRequest {
                        source: crate::reload_trigger::ReloadSource::Sighup,
                        respond_to: tx,
                    })
                    .await;
                tracing::info!(
                    target = "agent_shim::reload",
                    "SIGHUP received; reload requested"
                );
            }
        });
    }

    // Phase 1 P01: bind listener(s) eagerly so `on_listening` can fire
    // with the real bound address before we hand control to
    // `axum::serve`. Previously this binding happened inside
    // `server::run` / `server::run_with_admin`; extracting it here lets
    // the (future) Windows Service SCM entry point observe the bound
    // address and report SERVICE_RUNNING to the SCM at the right moment.
    let public_bind: SocketAddr = format!(
        "{}:{}",
        state.core.server_config.bind, state.core.server_config.port
    )
    .parse()?;
    let public_listener = tokio::net::TcpListener::bind(public_bind).await?;
    let public_local = public_listener.local_addr()?;
    tracing::info!("Listening on {} (public)", public_local);

    // Bind admin (if configured) BEFORE firing on_listening, so a bind
    // failure here aborts before any caller is told "running".
    let admin_listener_opt = if let Some(admin_cfg) = state.core.admin_config.clone() {
        let admin_bind: SocketAddr = format!("{}:{}", admin_cfg.bind, admin_cfg.port).parse()?;
        let admin_listener = tokio::net::TcpListener::bind(admin_bind).await?;
        tracing::info!("Listening on {} (admin)", admin_listener.local_addr()?);
        Some(admin_listener)
    } else {
        None
    };

    // Fire only after every listener is bound — used by the Windows Service
    // SCM entry to signal SERVICE_RUNNING; we must not signal "up" if a
    // subsequent bind could still fail.
    on_listening(public_local);

    // P05 T11: clone the plugin Arc + shutdown.plugin_flush_secs BEFORE
    // moving `state` into axum. After axum drain, no new H7 spawns occur
    // (no requests in flight) so flush can run safely on this clone.
    // P07: plugins now live on AppSnapshot; capture the current snapshot once.
    let plugins = state.snapshot.load_full().plugins.clone();
    let flush_secs = state.snapshot.load_full().config.shutdown.plugin_flush_secs;

    let result = if let Some(admin_listener) = admin_listener_opt {
        crate::server::run_with_admin_on_listeners(
            public_listener,
            admin_listener,
            state,
            shutdown_signal,
        )
        .await
    } else {
        crate::server::run_on_listener(public_listener, state, shutdown_signal).await
    };

    // P05 T11: flush H7 plugin tasks AFTER axum drain, BEFORE otel
    // shutdown — otel must still be live to flush the warn! lines
    // emitted for dropped tasks. P05 §6.8.
    let dropped = plugins
        .flush_pending_h7(std::time::Duration::from_secs(flush_secs))
        .await;
    for (plugin_name, count) in dropped {
        agent_shim_observability::metrics::recorders::record_h7_dropped(&plugin_name, count);
        tracing::warn!(
            "plugin.name" = %plugin_name,
            dropped_count = count,
            deadline_secs = flush_secs,
            "H7 task dropped at shutdown",
        );
    }

    if let Some(otel) = tracing_handles.otel {
        otel.shutdown();
    }
    result
}

pub async fn run(config_path: &Path) -> Result<()> {
    let cfg = agent_shim_config::load_from_path(config_path)
        .map_err(|e| anyhow::anyhow!("Failed to load config: {}", e))?;
    agent_shim_config::validate(&cfg)
        .map_err(|e| anyhow::anyhow!("Config validation failed: {}", e))?;
    run_core(
        cfg,
        Some(config_path.to_path_buf()),
        crate::shutdown::shutdown_signal(),
        |_addr| {}, // foreground: no-op on listening
    )
    .await
}

/// Plan 04 P04 T4: reload-applying handler. One copy runs in the task
/// spawned by `run`; the mpsc bounds capacity to 8 so this is also the
/// only place an `ArcSwap` swap happens — no other task ever calls
/// `state.snapshot.store`. Spec §5.2.
///
/// Path/Sighup sources re-read YAML from disk; Yaml sources use the body
/// directly. The baseline for `validate_for_reload` is built from the
/// CURRENT running state (live snapshot's upstream keys + the immutable
/// `AppCore` server/admin config + the snapshot's otel.endpoint), not
/// from the original startup config — so a future, looser policy on
/// reload-time validation only has to update the baseline construction
/// here, never the AppCore plumbing.
///
/// Public so the integration tests in `crates/gateway/tests/reload_*`
/// can drive a reload-applying task in-process without spawning the
/// full server. P04 T9 review followup: renamed from
/// `handle_reload_for_test` (the original name suggested the function
/// existed only for tests, which was misleading since production calls
/// it from `run`'s spawned task).
pub async fn handle_reload(
    state: &crate::state::AppState,
    source: crate::reload_trigger::ReloadSource,
) -> crate::reload_trigger::ReloadOutcome {
    use crate::reload_trigger::{ReloadOutcome, ReloadSource};

    // P04 T9 review HIGH-1: read YAML via `tokio::fs` so the
    // reload-applying task doesn't block the tokio worker thread on a
    // slow/networked filesystem.
    let yaml: String = match source {
        ReloadSource::Path(p) => match tokio::fs::read_to_string(&p).await {
            Ok(s) => s,
            Err(e) => return ReloadOutcome::Io(e.to_string()),
        },
        ReloadSource::Yaml(s) => s,
        ReloadSource::Sighup => match state.core.config_path.as_ref() {
            Some(p) => match tokio::fs::read_to_string(p).await {
                Ok(s) => s,
                Err(e) => return ReloadOutcome::Io(e.to_string()),
            },
            None => {
                return ReloadOutcome::Io("no config path; SIGHUP can't re-read".into());
            }
        },
    };

    let candidate: agent_shim_config::GatewayConfig = match serde_yaml::from_str(&yaml) {
        Ok(c) => c,
        Err(e) => return ReloadOutcome::Parse(e.to_string()),
    };

    let current = state.snapshot.load_full();
    let baseline = agent_shim_config::ReloadBaseline {
        upstream_names: current.config.upstreams.keys().cloned().collect(),
        server: state.core.server_config.clone(),
        admin: state.core.admin_config.clone(),
        otel_endpoint: current
            .config
            .otel
            .as_ref()
            .and_then(|o| o.endpoint.clone()),
    };

    match agent_shim_config::validate_for_reload(&candidate, &baseline) {
        Ok(diff) => {
            // Build a fresh snapshot from the candidate config and
            // atomic-swap. In-flight requests captured the OLD snapshot
            // at the top of `pipeline::dispatch` and stay on it (spec
            // §2.2); new requests after this store() see the new one.
            let build = agent_shim_observability::reload::build(candidate);
            // P07 T1: plugins now live on AppSnapshot. T1 is a pure
            // refactor — the reload path preserves the current plugin
            // Arc byte-for-byte; actual hot-swapping lands in T3.
            let current_plugins = state.snapshot.load_full().plugins.clone();
            let new_snap = Arc::new(crate::state::AppSnapshot {
                config: build.config,
                auth_enabled: build.auth_enabled,
                auth_required: build.auth_required,
                configured_key_hashes: build.configured_key_hashes,
                plugins: current_plugins,
            });
            state.snapshot.store(new_snap);

            // Phase 6 P02 T4: rebuild the rate-limit registry from the
            // just-committed snapshot. Spec §5.4: governor doesn't
            // expose retuning, so we replace the whole registry —
            // buckets reset with new burst capacity. The swap happens
            // AFTER the AppSnapshot swap (commit point 2 in spec §6.2):
            // a request racing between the two stores sees a valid
            // (old or new) snapshot AND a valid (old or new) limiter,
            // both legal — just from different config versions for a
            // tiny window. Inherent ArcSwap property; documented.
            let committed = state.snapshot.load_full();
            let new_limiter = if committed.config.rate_limit.enabled {
                agent_shim_router::LimiterRegistry::from_config(&committed.config.rate_limit)
            } else {
                agent_shim_router::LimiterRegistry::disabled()
            };
            state
                .core
                .limiter_registry
                .store(std::sync::Arc::new(new_limiter));

            metrics::counter!(
                agent_shim_observability::metrics::names::CONFIG_RELOADS_TOTAL,
                "result" => "ok",
            )
            .increment(1);
            tracing::info!(target = "agent_shim::reload", "config reloaded");
            ReloadOutcome::Ok(diff)
        }
        Err(agent_shim_config::ReloadValidationError::ImmutableFieldChanged {
            field,
            old,
            new,
        }) => {
            metrics::counter!(
                agent_shim_observability::metrics::names::CONFIG_RELOADS_TOTAL,
                "result" => "immutable_field",
            )
            .increment(1);
            ReloadOutcome::ImmutableField(format!(
                "{field}: {old} → {new} forbidden (restart required)"
            ))
        }
        Err(agent_shim_config::ReloadValidationError::UpstreamSetChanged { added, removed }) => {
            metrics::counter!(
                agent_shim_observability::metrics::names::CONFIG_RELOADS_TOTAL,
                "result" => "immutable_field",
            )
            .increment(1);
            ReloadOutcome::ImmutableField(format!(
                "upstreams.* set changed (added={added:?}, removed={removed:?}); \
                 immutable in v0.5"
            ))
        }
        Err(other) => {
            metrics::counter!(
                agent_shim_observability::metrics::names::CONFIG_RELOADS_TOTAL,
                "result" => "validation_error",
            )
            .increment(1);
            ReloadOutcome::ValidationError(other.to_string())
        }
    }
}
