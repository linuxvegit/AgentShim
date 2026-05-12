//! Snapshot rebuild for hot-reload. Plan 04 P04 T2.
//!
//! Given a validated candidate config and a running `AppCore` view (just
//! the parts the rebuild needs — no Arcs to runtime objects), produce a
//! fresh `AppSnapshot`-shaped value. The actual `AppSnapshot` type lives
//! in the gateway crate to avoid circular deps; this module returns a
//! `ReloadBuild` struct the gateway can fold into its own snapshot.

use std::collections::HashSet;
use std::sync::Arc;

use agent_shim_config::GatewayConfig;

/// Rebuilt fields. The gateway crate lifts this into its own
/// `AppSnapshot` shape.
pub struct ReloadBuild {
    pub config: Arc<GatewayConfig>,
    pub auth_enabled: bool,
    pub auth_required: bool,
    pub configured_key_hashes: Arc<HashSet<String>>,
}

pub fn build(config: GatewayConfig) -> ReloadBuild {
    let auth_enabled = config.auth.enabled;
    let auth_required = config.auth.required;
    let configured_key_hashes: Arc<HashSet<String>> =
        Arc::new(config.auth.keys.keys().cloned().collect());
    ReloadBuild {
        config: Arc::new(config),
        auth_enabled,
        auth_required,
        configured_key_hashes,
    }
}
