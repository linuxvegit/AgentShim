//! Error types for the plugin system. Three distinct error categories:
//!
//! - `PluginError::Timeout` / `Failed` — internal plugin failure; honoured
//!   by `on_error` (`skip` or `fail`).
//! - `PluginError::Aborted` — plugin actively rejected the request; maps
//!   to HTTP 400, regardless of `on_error` setting.
//! - `PluginError::ProtectedFieldMutated` — plugin tried to change a
//!   routing-affecting field of `CanonicalRequest`. Treated as `Failed`
//!   for on_error purposes; logs at ERROR.
//!
//! See spec §4.3 / §4.5.

use serde::{Deserialize, Serialize};

/// `on_error` policy for a single plugin. Set per-plugin in YAML.
/// `skip` swallows internal failures (log + metric + continue); `fail`
/// propagates them and aborts the request. `Aborted` is not subject to
/// this knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnError {
    #[default]
    Skip,
    Fail,
}

pub type PluginResult<T> = Result<T, PluginError>;

/// Errors a plugin can return or that the invoke() template synthesises.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// Plugin exceeded its per-hook timeout. The plugin's future is
    /// dropped (cancelled). Honoured by `on_error`.
    #[error("plugin {plugin} timed out after {elapsed_ms}ms in {hook}")]
    Timeout {
        plugin: String,
        hook: &'static str,
        elapsed_ms: u64,
    },

    /// Plugin returned an error from its hook method. Honoured by
    /// `on_error`.
    #[error("plugin {plugin} failed in {hook}: {message}")]
    Failed {
        plugin: String,
        hook: &'static str,
        message: String,
    },

    /// Plugin actively rejected the request. NOT subject to `on_error`.
    /// Maps to HTTP 400 by the gateway pipeline.
    #[error("plugin {plugin} aborted request: {reason}")]
    Aborted { plugin: String, reason: String },

    /// Plugin mutated a routing-affecting field of `CanonicalRequest`.
    /// Detected by the `invoke()` template's post-call diff. Treated as
    /// `Failed` for `on_error` purposes (logs at ERROR; surfaces 502
    /// when `on_error: fail`).
    #[error("plugin {plugin} modified protected field `{field}` in {hook}")]
    ProtectedFieldMutated {
        plugin: String,
        hook: &'static str,
        field: &'static str,
    },
}

/// Errors raised by `PluginFactory::instantiate`. Carry the YAML path
/// of the failure so operators can find it.
#[derive(Debug, thiserror::Error)]
pub enum PluginConfigError {
    #[error("plugin `{plugin}` config: missing required field `{field}` at {path}")]
    MissingField {
        plugin: String,
        field: &'static str,
        path: String,
    },
    #[error("plugin `{plugin}` config: invalid value for `{field}`: {reason}")]
    InvalidValue {
        plugin: String,
        field: String,
        reason: String,
    },
    #[error("plugin `{1}` config: deserialization failed: {0}")]
    Deserialize(#[source] serde_json::Error, /* plugin */ String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_error_default_is_skip() {
        assert_eq!(OnError::default(), OnError::Skip);
    }

    #[test]
    fn on_error_yaml_round_trip() {
        let s = serde_yaml::to_string(&OnError::Fail).unwrap();
        assert_eq!(s.trim(), "fail");
        let parsed: OnError = serde_yaml::from_str("skip").unwrap();
        assert_eq!(parsed, OnError::Skip);
    }

    #[test]
    fn plugin_error_display_format() {
        let e = PluginError::Timeout {
            plugin: "foo".to_string(),
            hook: "on_decoded_request",
            elapsed_ms: 51,
        };
        let s = e.to_string();
        assert!(s.contains("foo"));
        assert!(s.contains("51ms"));
        assert!(s.contains("on_decoded_request"));
    }
}
