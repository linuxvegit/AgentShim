use agent_shim_config::schema::{
    FileLoggingConfig, LogFormat, LoggingConfig, OtelConfig, RotationPolicy,
};
use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{fmt, prelude::*, EnvFilter, Layer, Registry};

use crate::otel::{build_layer, OtelHandle};

/// Tracing handles whose lifetimes must extend to process shutdown.
/// Hold this in `main.rs` (or `commands::serve::run_core`) and drop /
/// `shutdown()` it before exit so the OTLP batch exporter drains and
/// the non-blocking file writer flushes buffered events.
///
/// Plan windows-service P02 T5 — added `file_guard` field.
pub struct TracingHandles {
    pub otel: Option<OtelHandle>,
    pub file_guard: Option<WorkerGuard>,
}

/// Initialize the global subscriber. Called once at startup.
///
/// `otel_cfg = None` reproduces v0.4 behavior — pretty/JSON fmt layer
/// only, no spans exported. `Some(cfg)` adds the OTel layer; if
/// `cfg.endpoint` is also `None`, the OTel layer is built but is a
/// no-op (spans flow only to the fmt layer).
///
/// When `log.file = Some(...)`, a rolling-file layer is also installed
/// — see [`build_file_layer`]. Plan windows-service P02 T5.
pub fn init(log: &LoggingConfig, otel_cfg: Option<&OtelConfig>) -> TracingHandles {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::try_new(&log.filter).unwrap_or_else(|_| EnvFilter::new("info"))
    });

    // Build the optional OTel layer. Errors here are fatal — operators
    // who configure an endpoint expect spans to flow.
    let (otel_layer, otel_handle) = if let Some(cfg) = otel_cfg {
        match build_layer::<Registry>(cfg) {
            Ok(Some((layer, handle))) => (Some(layer), Some(handle)),
            Ok(None) => (None, None),
            Err(e) => {
                eprintln!("FATAL: OTel exporter init failed: {e}");
                std::process::exit(2);
            }
        }
    } else {
        (None, None)
    };

    // Optional file layer (Plan windows-service P02 T5).
    let (file_layer, file_guard) = build_file_layer(log.file.as_ref());

    // Both the OTel layer (`OpenTelemetryLayer<Registry, _>`) and the boxed
    // file layer only implement `Layer<Registry>`, not `Layer<Layered<...>>`.
    // We can't chain them via `.with(a).with(b)` because after the first
    // `.with(...)` the subscriber type is no longer `Registry`. The fix is
    // to collect both into a single `Vec<Box<dyn Layer<Registry>>>`, which
    // itself implements `Layer<Registry>` and applies all inner layers at
    // once. Plan windows-service P02 T5.
    //
    // IMPORTANT: an EMPTY `Vec<Box<dyn Layer<S>>>` short-circuits the
    // whole subscriber — its `Layer::enabled` returns false, which masks
    // events from any downstream layer (including the stdout `fmt`
    // layer). When `otel_layer` and `file_layer` are both `None`, we
    // must wrap with `Option::None` instead of an empty Vec so the
    // composite layer is absent rather than disabled. Symptom otherwise:
    // foreground `agent-shim serve` with neither otel nor file logging
    // emits zero stdout — present since Plan windows-service P02 T5
    // introduced the Vec.
    let mut composite: Vec<Box<dyn Layer<Registry> + Send + Sync>> = Vec::new();
    if let Some(otel) = otel_layer {
        composite.push(Box::new(otel));
    }
    if let Some(file) = file_layer {
        composite.push(file);
    }
    let composite = if composite.is_empty() {
        None
    } else {
        Some(composite)
    };

    let registry = tracing_subscriber::registry().with(composite).with(filter);

    match log.format {
        LogFormat::Json => {
            if let Err(e) = registry.with(fmt::layer().json()).try_init() {
                eprintln!("WARNING: tracing subscriber init failed: {e}");
            }
        }
        LogFormat::Pretty => {
            if let Err(e) = registry.with(fmt::layer().pretty()).try_init() {
                eprintln!("WARNING: tracing subscriber init failed: {e}");
            }
        }
    }

    TracingHandles {
        otel: otel_handle,
        file_guard,
    }
}

/// Build the optional file-logging layer.
///
/// Behavior:
/// - `cfg = None` → returns `(None, None)`; subscriber has no file layer.
/// - `cfg = Some(...)` → ensures the parent directory exists (fatal on failure,
///   matching OTel init's behavior), constructs a `RollingFileAppender` with
///   the configured rotation and `max_log_files` retention, and wraps it in
///   `tracing_appender::non_blocking` for async writes.
///
/// The returned `WorkerGuard` must outlive every emitted event; the caller
/// stores it on `TracingHandles.file_guard` so it lives until process
/// shutdown drops the handles.
///
/// Plan windows-service P02 T5.
fn build_file_layer(
    cfg: Option<&FileLoggingConfig>,
) -> (
    Option<Box<dyn Layer<Registry> + Send + Sync>>,
    Option<WorkerGuard>,
) {
    let Some(cfg) = cfg else {
        return (None, None);
    };

    // 1. Resolve directory and filename prefix.
    // Path::new("foo.log").parent() returns Some("") (NOT None) — filter the
    // empty path so bare filenames fall through to cwd as intended, rather
    // than triggering create_dir_all("") which errors with "path not found".
    let dir = cfg
        .path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| {
            // `path` was a bare filename (no parent). Use cwd.
            std::path::PathBuf::from(".")
        });
    let prefix = match cfg.path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => {
            eprintln!(
                "FATAL: logging.file.path has no filename component: {:?}",
                cfg.path
            );
            std::process::exit(2);
        }
    };

    // 2. Create directory if needed. Fatal on failure (matches OTel behavior).
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("FATAL: cannot create log directory {:?}: {}", dir, e);
        std::process::exit(2);
    }

    // 3. Optional warning for relative paths — they resolve against cwd,
    // which is C:\Windows\System32 under Windows Service. Documented in
    // the spec; we surface it loudly here.
    // Note: under Windows Service mode (SCM-launched), stderr is detached,
    // so this warning vanishes. It still fires correctly for foreground
    // `agent-shim serve` invocations on all platforms. A more robust path
    // (Windows Event Log) is out of scope for v0.6.x; tracking as a known
    // limitation: an operator who installs with a relative `logging.file.path`
    // under Windows Service won't see a warning, but the resulting path
    // resolves to a subdirectory of C:\Windows\System32 — usually a
    // permission-denied which fails fast and is visible via the
    // Stopped(Win32(2)) status reported by run_service.
    if cfg.path.is_relative() {
        eprintln!(
            "WARNING: logging.file.path {:?} is relative — under Windows \
             Service the cwd is C:\\Windows\\System32. Use an absolute path.",
            cfg.path
        );
    }

    // 4. Build the rolling appender.
    let rotation = match cfg.rotation {
        RotationPolicy::Daily => Rotation::DAILY,
        RotationPolicy::Hourly => Rotation::HOURLY,
        RotationPolicy::Never => Rotation::NEVER,
    };
    let mut builder = RollingFileAppender::builder()
        .rotation(rotation)
        .filename_prefix(prefix);
    // `max_files == 0` means unlimited retention — omit the builder call
    // entirely so tracing-appender keeps every rolled file. Don't substitute
    // usize::MAX; that would cap (very high but finite) and behave subtly
    // different from "no cap".
    if cfg.max_files > 0 {
        builder = builder.max_log_files(cfg.max_files);
    }
    let appender = match builder.build(&dir) {
        Ok(a) => a,
        Err(e) => {
            eprintln!(
                "FATAL: cannot initialize rolling file appender at {:?}: {}",
                dir, e
            );
            std::process::exit(2);
        }
    };

    // 5. Wrap in non_blocking and build the layer.
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
    let layer: Box<dyn Layer<Registry> + Send + Sync> = match cfg.format {
        LogFormat::Json => Box::new(fmt::layer().json().with_writer(non_blocking)),
        LogFormat::Pretty => Box::new(fmt::layer().with_writer(non_blocking)),
    };

    (Some(layer), Some(guard))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_pretty_no_otel_does_not_panic() {
        let cfg = LoggingConfig {
            format: LogFormat::Pretty,
            filter: "info".to_string(),
            file: None,
            print_catalog_on_start: false,
        };
        let _ = init(&cfg, None);
    }

    #[test]
    fn init_json_no_otel_does_not_panic() {
        let cfg = LoggingConfig {
            format: LogFormat::Json,
            filter: "debug".to_string(),
            file: None,
            print_catalog_on_start: false,
        };
        let _ = init(&cfg, None);
    }

    #[test]
    fn init_with_disabled_otel_does_not_panic() {
        let cfg = LoggingConfig {
            format: LogFormat::Pretty,
            filter: "info".to_string(),
            file: None,
            print_catalog_on_start: false,
        };
        let otel = OtelConfig::default(); // endpoint = None
        let handles = init(&cfg, Some(&otel));
        assert!(handles.otel.is_none(), "no endpoint → no OtelHandle");
        assert!(handles.file_guard.is_none(), "no file cfg → no guard");
    }

    /// Regression: when neither `logging.file` nor `otel.endpoint` is
    /// configured, the empty `Vec<Box<dyn Layer<S>>>` previously fed to
    /// `registry.with(composite)` was treated as a Layer whose
    /// `enabled()` returned false, short-circuiting the entire
    /// subscriber and silencing the stdout `fmt` layer. The fix wraps
    /// the composite as `Option<Vec<_>>` so the empty case becomes
    /// `None` (a true no-op layer) instead of an empty Vec (a deny-all
    /// layer).
    ///
    /// This test verifies the no-emit symptom isn't present by
    /// installing a custom capture layer alongside the fmt layer: if
    /// the empty composite (Option::None) wired up by `init` does not
    /// short-circuit, our capture sees the events. We can't observe
    /// stdout directly from inside a test (`fmt::layer` writes to the
    /// process stdout), so we use a side-channel layer.
    #[test]
    fn no_file_no_otel_does_not_silence_subscriber() {
        use std::sync::{Arc, Mutex};
        use tracing::Subscriber;
        use tracing_subscriber::layer::{Context, SubscriberExt};

        #[derive(Clone, Default)]
        struct CaptureLayer(Arc<Mutex<Vec<String>>>);

        impl<S: Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                let mut s = format!("{}: ", event.metadata().target());
                struct V<'a>(&'a mut String);
                impl<'a> tracing::field::Visit for V<'a> {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        use std::fmt::Write;
                        let _ = write!(self.0, "{}={:?} ", field.name(), value);
                    }
                }
                event.record(&mut V(&mut s));
                self.0.lock().unwrap().push(s);
            }
        }

        // We can't use `init` here because it calls try_init globally
        // and we need an isolated subscriber for the test. Instead,
        // replicate the exact subscriber-construction logic from `init`
        // with the fix in place and run it with `with_default`.
        let capture = CaptureLayer::default();
        let composite: Vec<Box<dyn Layer<Registry> + Send + Sync>> = Vec::new();
        let composite = if composite.is_empty() {
            None
        } else {
            Some(composite)
        };
        let filter = EnvFilter::new("info");
        let subscriber = tracing_subscriber::registry()
            .with(composite)
            .with(filter)
            .with(capture.clone());

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "regression_test", "event must be visible");
        });

        let events = capture.0.lock().unwrap();
        assert!(
            events.iter().any(|s| s.contains("regression_test")),
            "Empty composite Vec must NOT short-circuit downstream \
             layers. Captured events: {:?}",
            *events
        );
    }
}
