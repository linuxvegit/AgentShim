#![forbid(unsafe_code)]

pub mod openai_compatible;
pub mod github_copilot;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream};

#[derive(Debug, Clone, Default)]
pub struct ProviderCapabilities {
    pub streaming: bool,
    pub tool_use: bool,
    pub vision: bool,
    pub json_mode: bool,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("unknown provider: {0}")]
    UnknownProvider(String),
    #[error("upstream error (status={status}): {body}")]
    Upstream { status: u16, body: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("encode error: {0}")]
    Encode(String),
    #[error("capability mismatch: {0}")]
    CapabilityMismatch(String),
}

#[async_trait]
pub trait BackendProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> &ProviderCapabilities;
    async fn complete(
        &self,
        req: CanonicalRequest,
        target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError>;
}

#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn BackendProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Arc<dyn BackendProvider>) {
        self.providers.insert(provider.name().to_string(), provider);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn BackendProvider>> {
        self.providers.get(name).cloned()
    }

    pub fn resolve(
        &self,
        req: CanonicalRequest,
        target: BackendTarget,
    ) -> Result<(Arc<dyn BackendProvider>, CanonicalRequest, BackendTarget), ProviderError> {
        let provider = self
            .get(&target.provider)
            .ok_or_else(|| ProviderError::UnknownProvider(target.provider.clone()))?;
        Ok((provider, req, target))
    }
}
