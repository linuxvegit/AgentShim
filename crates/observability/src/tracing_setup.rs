use agent_shim_config::schema::{LogFormat, LoggingConfig};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Initialize the global tracing subscriber based on config.
/// Safe to call once per process. Subsequent calls are no-ops if a subscriber is already set.
pub fn init(config: &LoggingConfig) {
    let filter = EnvFilter::try_new(&config.filter)
        .unwrap_or_else(|_| EnvFilter::new("info"));

    match config.format {
        LogFormat::Json => {
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().json())
                .try_init();
        }
        LogFormat::Pretty => {
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().pretty())
                .try_init();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_config::schema::{LogFormat, LoggingConfig};

    #[test]
    fn init_pretty_does_not_panic() {
        let cfg = LoggingConfig {
            format: LogFormat::Pretty,
            filter: "info".to_string(),
        };
        init(&cfg); // idempotent
    }

    #[test]
    fn init_json_does_not_panic() {
        let cfg = LoggingConfig {
            format: LogFormat::Json,
            filter: "debug".to_string(),
        };
        init(&cfg);
    }
}
