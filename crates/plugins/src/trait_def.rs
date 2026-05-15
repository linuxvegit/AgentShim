//! Plugin and PluginFactory traits. The four hooks (`on_decoded_request`,
//! `on_resolved`, `on_stream_event`, `on_response_complete`) all default
//! to no-op; plugins override only the ones they need.
//!
//! Per spec §4.1: hooks take owned `CanonicalRequest` and return owned
//! `CanonicalRequest`. The registry's `invoke()` template clones the
//! request before each plugin call (clone-then-swap, §6.4) so that an
//! `Err`-returning plugin never leaks a half-modified state.

use std::fmt;

use agent_shim_core::{BackendTarget, CanonicalRequest, StreamEvent};
use async_trait::async_trait;

use crate::context::{PluginContext, ResponseSummary};
use crate::error::{PluginConfigError, PluginResult};

/// One of the four lifecycle hooks. Used as a typed string in logs
/// (`plugin.hook` field) and in `Plugin::hooks()` subscription bitsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hook {
    DecodedRequest,
    Resolved,
    StreamEvent,
    ResponseComplete,
}

impl Hook {
    /// Stable string name. Used as the `plugin.hook` field in logs and
    /// metrics. MUST match the YAML key (`on_decoded_request` etc.).
    pub fn as_str(self) -> &'static str {
        match self {
            Hook::DecodedRequest => "on_decoded_request",
            Hook::Resolved => "on_resolved",
            Hook::StreamEvent => "on_stream_event",
            Hook::ResponseComplete => "on_response_complete",
        }
    }
}

impl fmt::Display for Hook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Bit-flag set of hooks a plugin subscribes to. Returned by
/// `Plugin::hooks()`. The registry consults this at construction time
/// to populate the route plan only on hooks the plugin actually wants.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HookSet(u8);

impl HookSet {
    pub const DECODED_REQUEST: Self = Self(1 << 0);
    pub const RESOLVED: Self = Self(1 << 1);
    pub const STREAM_EVENT: Self = Self(1 << 2);
    pub const RESPONSE_COMPLETE: Self = Self(1 << 3);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub fn contains(self, hook: Hook) -> bool {
        let bit = match hook {
            Hook::DecodedRequest => 1 << 0,
            Hook::Resolved => 1 << 1,
            Hook::StreamEvent => 1 << 2,
            Hook::ResponseComplete => 1 << 3,
        };
        (self.0 & bit) != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for HookSet {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for HookSet {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// The Plugin trait. One impl per plugin **kind**; each kind can be
/// instantiated many times under different names. All hook methods
/// default to no-op so plugins only override what they care about.
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    /// Globally-unique kind name. Matches the YAML `type:` field.
    /// Must be a `&'static str` so logs and spans can use it without
    /// allocation.
    fn kind_name(&self) -> &'static str;

    /// Hook subset this instance subscribes to. Decided at construction
    /// time (after the factory parses the per-plugin config). Returning
    /// an empty set is legal but means the plugin will never run.
    fn hooks(&self) -> HookSet;

    /// H2: after frontend decode, before route resolution.
    async fn on_decoded_request(
        &self,
        _ctx: &PluginContext,
        req: CanonicalRequest,
    ) -> PluginResult<CanonicalRequest> {
        Ok(req)
    }

    /// H3: after route + policy merge, before capability gate.
    async fn on_resolved(
        &self,
        _ctx: &PluginContext,
        req: CanonicalRequest,
        _target: &BackendTarget,
    ) -> PluginResult<CanonicalRequest> {
        Ok(req)
    }

    /// H5: per upstream→client stream event. Returns zero or more
    /// events; empty Vec = drop the event. Identity = `vec![event]`.
    async fn on_stream_event(
        &self,
        _ctx: &PluginContext,
        event: StreamEvent,
    ) -> PluginResult<Vec<StreamEvent>> {
        Ok(vec![event])
    }

    /// H7: after request completion. Fire-and-forget — return value is
    /// logged but does not affect the response. Spawned on the
    /// registry's internal JoinSet (P05); slow H7 plugins can be
    /// flushed at shutdown via `PluginRegistry::flush_pending_h7`.
    async fn on_response_complete(
        &self,
        _ctx: &PluginContext,
        _summary: &ResponseSummary,
    ) -> PluginResult<()> {
        Ok(())
    }
}

/// Factory for constructing `Plugin` instances from a parsed YAML
/// config block. One factory per plugin kind. Registered into
/// `PluginRegistry` at startup.
pub trait PluginFactory: Send + Sync + 'static {
    /// Kind name handled by this factory. Must match
    /// `Plugin::kind_name()` of the constructed instance.
    fn kind_name(&self) -> &'static str;

    /// Build a plugin instance.
    ///
    /// `plugin_name` is the YAML key (e.g. `compressor_for_deepseek`),
    /// used for error messages and logs.
    /// `config` is the raw `config:` map deserialised as a JSON value;
    /// factories own their internal config struct + serde
    /// deserialization.
    fn instantiate(
        &self,
        plugin_name: &str,
        config: serde_json::Value,
    ) -> Result<Box<dyn Plugin>, PluginConfigError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{FrontendKind, RequestId};

    struct NoopPlugin;

    #[async_trait]
    impl Plugin for NoopPlugin {
        fn kind_name(&self) -> &'static str {
            "noop"
        }
        fn hooks(&self) -> HookSet {
            HookSet::empty()
        }
    }

    #[test]
    fn hook_set_bit_or() {
        let s = HookSet::DECODED_REQUEST | HookSet::RESPONSE_COMPLETE;
        assert!(s.contains(Hook::DecodedRequest));
        assert!(!s.contains(Hook::Resolved));
        assert!(!s.contains(Hook::StreamEvent));
        assert!(s.contains(Hook::ResponseComplete));
        assert!(!s.is_empty());
    }

    #[test]
    fn hook_set_empty_is_empty() {
        assert!(HookSet::empty().is_empty());
        assert!(!HookSet::empty().contains(Hook::DecodedRequest));
    }

    #[test]
    fn hook_as_str_matches_yaml_keys() {
        assert_eq!(Hook::DecodedRequest.as_str(), "on_decoded_request");
        assert_eq!(Hook::Resolved.as_str(), "on_resolved");
        assert_eq!(Hook::StreamEvent.as_str(), "on_stream_event");
        assert_eq!(Hook::ResponseComplete.as_str(), "on_response_complete");
    }

    #[tokio::test]
    async fn default_methods_are_no_op() {
        let p = NoopPlugin;
        assert_eq!(p.kind_name(), "noop");
        assert!(p.hooks().is_empty());

        // Defaults return Ok(input)
        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "test/test".to_string(),
        };
        // Cheap stub: just verify the default impls compile & don't err.
        let event = StreamEvent::MessageStop {
            stop_reason: agent_shim_core::StopReason::EndTurn,
            stop_sequence: None,
        };
        let out = p.on_stream_event(&ctx, event.clone()).await.unwrap();
        assert_eq!(out.len(), 1);
    }
}
