use agent_shim_config::schema::{LogFormat, LoggingConfig, OtelConfig};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::otel::{build_layer, OtelHandle};

/// Tracing handles whose lifetimes must extend to process shutdown.
/// Hold this in `main.rs` (or `commands::serve::run`) and drop /
/// `shutdown()` it before exit so the OTLP batch exporter drains.
pub struct TracingHandles {
    pub otel: Option<OtelHandle>,
}

/// Initialize the global subscriber. Called once at startup.
///
/// `otel_cfg = None` reproduces v0.4 behavior — pretty/JSON fmt layer
/// only, no spans exported. `Some(cfg)` adds the OTel layer; if
/// `cfg.endpoint` is also `None`, the OTel layer is built but is a
/// no-op (spans flow only to the fmt layer).
pub fn init(log: &LoggingConfig, otel_cfg: Option<&OtelConfig>) -> TracingHandles {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::try_new(&log.filter).unwrap_or_else(|_| EnvFilter::new("info"))
    });

    // Build the optional OTel layer. Errors here are fatal — operators
    // who configure an endpoint expect spans to flow.
    let (otel_layer, handle) = if let Some(cfg) = otel_cfg {
        match build_layer::<tracing_subscriber::Registry>(cfg) {
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

    let registry = tracing_subscriber::registry().with(otel_layer).with(filter);

    match log.format {
        LogFormat::Json => {
            let _ = registry.with(fmt::layer().json()).try_init();
        }
        LogFormat::Pretty => {
            let _ = registry.with(fmt::layer().pretty()).try_init();
        }
    }

    TracingHandles { otel: handle }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_pretty_no_otel_does_not_panic() {
        let cfg = LoggingConfig {
            format: LogFormat::Pretty,
            filter: "info".to_string(),
        };
        let _ = init(&cfg, None);
    }

    #[test]
    fn init_json_no_otel_does_not_panic() {
        let cfg = LoggingConfig {
            format: LogFormat::Json,
            filter: "debug".to_string(),
        };
        let _ = init(&cfg, None);
    }

    #[test]
    fn init_with_disabled_otel_does_not_panic() {
        let cfg = LoggingConfig {
            format: LogFormat::Pretty,
            filter: "info".to_string(),
        };
        let otel = OtelConfig::default(); // endpoint = None
        let handles = init(&cfg, Some(&otel));
        assert!(handles.otel.is_none(), "no endpoint → no OtelHandle");
    }
}
