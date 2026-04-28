use std::sync::Arc;
use agent_shim_config::GatewayConfig;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<GatewayConfig>,
}

impl AppState {
    pub fn new(config: GatewayConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}
