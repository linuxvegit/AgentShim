use figment::{Figment, providers::{Format, Yaml, Env}};
use thiserror::Error;
use std::path::Path;
use crate::schema::GatewayConfig;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file not found: {0}")]
    NotFound(String),
    #[error("config parse error: {0}")]
    Parse(String),
    #[error("config IO error: {0}")]
    Io(String),
}

pub fn load_from_path(path: impl AsRef<Path>) -> Result<GatewayConfig, ConfigError> {
    let path = path.as_ref();
    if !path.exists() {
        return Err(ConfigError::NotFound(path.display().to_string()));
    }
    Figment::new()
        .merge(Yaml::file(path))
        .merge(Env::prefixed("AGENT_SHIM__").split("__"))
        .extract::<GatewayConfig>()
        .map_err(|e| ConfigError::Parse(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn loads_minimal_yaml() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "server:\n  port: 9000\n").unwrap();
        let cfg = load_from_path(f.path()).unwrap();
        assert_eq!(cfg.server.port, 9000);
    }

    #[test]
    fn missing_file_returns_not_found() {
        let result = load_from_path("/nonexistent/path/config.yaml");
        assert!(matches!(result, Err(ConfigError::NotFound(_))));
    }
}
