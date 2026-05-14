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
    let mut composite: Vec<Box<dyn Layer<Registry> + Send + Sync>> = Vec::new();
    if let Some(otel) = otel_layer {
        composite.push(Box::new(otel));
    }
    if let Some(file) = file_layer {
        composite.push(file);
    }

    let registry = tracing_subscriber::registry().with(composite).with(filter);

    match log.format {
        LogFormat::Json => {
            let _ = registry.with(fmt::layer().json()).try_init();
        }
        LogFormat::Pretty => {
            let _ = registry.with(fmt::layer().pretty()).try_init();
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
    let dir = cfg
        .path
        .parent()
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
        };
        let _ = init(&cfg, None);
    }

    #[test]
    fn init_json_no_otel_does_not_panic() {
        let cfg = LoggingConfig {
            format: LogFormat::Json,
            filter: "debug".to_string(),
            file: None,
        };
        let _ = init(&cfg, None);
    }

    #[test]
    fn init_with_disabled_otel_does_not_panic() {
        let cfg = LoggingConfig {
            format: LogFormat::Pretty,
            filter: "info".to_string(),
            file: None,
        };
        let otel = OtelConfig::default(); // endpoint = None
        let handles = init(&cfg, Some(&otel));
        assert!(handles.otel.is_none(), "no endpoint → no OtelHandle");
        assert!(handles.file_guard.is_none(), "no file cfg → no guard");
    }
}
