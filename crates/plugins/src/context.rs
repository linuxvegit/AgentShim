//! Request-level context handed to every plugin hook. Carries
//! request_id, frontend kind, route label, and a per-request scratch
//! map for plugins that need to keep state across hook invocations.
//!
//! The scratch field was added in P06b1 to support `pii_scrubber`'s H5
//! sliding-window buffer. See
//! `docs/superpowers/specs/2026-05-22-phase-7-p06b1-pii-scrubber-design.md`
//! §4.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use agent_shim_core::{FrontendKind, RequestId, Usage};

/// Per-request metadata carried into every plugin hook.
///
/// `route_label` is the canonical string `<frontend_kind>/<model_alias>`
/// used by the rate-limit registry and metrics; plugins can read it for
/// logging/audit purposes but should not parse it.
///
/// `scratch` is a typed key/value store keyed by plugin `kind_name()`
/// literals. Plugins MUST namespace entries by their own kind to avoid
/// collisions; access goes through `scratch_get_or_init::<T, _>` for
/// type-safe lookups. Cloning a `PluginContext` Arc-shares the same
/// scratch — H5 mutations survive across stream events because the
/// pipeline passes the same `PluginContext` Arc through every hook.
#[derive(Debug, Clone)]
pub struct PluginContext {
    pub request_id: RequestId,
    pub frontend: FrontendKind,
    pub route_label: String,
    pub(crate) scratch: Arc<parking_lot::RwLock<HashMap<&'static str, Box<dyn Any + Send + Sync>>>>,
}

impl PluginContext {
    /// Construct a new `PluginContext` with an empty scratch map.
    pub fn new(request_id: RequestId, frontend: FrontendKind, route_label: String) -> Self {
        Self {
            request_id,
            frontend,
            route_label,
            scratch: Arc::new(parking_lot::RwLock::new(HashMap::new())),
        }
    }

    /// Acquire a typed mutable reference to the per-request scratch slot
    /// for the given plugin kind. The `init` closure runs only when the
    /// slot is empty for this request.
    ///
    /// # Panics
    /// If the slot exists but holds a type other than `T`. This is a
    /// programmer error: two plugins claiming the same kind key. The
    /// registry's `kind_name()` uniqueness check (P06a
    /// `DuplicateFactoryKind`) prevents this in production.
    pub fn scratch_get_or_init<T, F>(&self, kind: &'static str, init: F) -> ScratchGuard<'_, T>
    where
        T: Send + Sync + 'static,
        F: FnOnce() -> T,
    {
        let mut map = self.scratch.write();
        map.entry(kind)
            .or_insert_with(|| Box::new(init()) as Box<dyn Any + Send + Sync>);
        ScratchGuard {
            map,
            kind,
            _t: std::marker::PhantomData,
        }
    }
}

/// RAII handle to a typed scratch slot. Holds the underlying RwLock
/// write guard for as long as it is in scope; subsequent calls to
/// `scratch_get_or_init` on the same `PluginContext` block until this
/// guard is dropped.
pub struct ScratchGuard<'a, T: 'static> {
    map: parking_lot::RwLockWriteGuard<'a, HashMap<&'static str, Box<dyn Any + Send + Sync>>>,
    kind: &'static str,
    _t: std::marker::PhantomData<T>,
}

impl<T: 'static> std::ops::Deref for ScratchGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.map
            .get(self.kind)
            .expect("inserted above")
            .downcast_ref::<T>()
            .unwrap_or_else(|| {
                panic!(
                    "scratch key collision: kind `{}` holds the wrong type",
                    self.kind
                )
            })
    }
}

impl<T: 'static> std::ops::DerefMut for ScratchGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.map
            .get_mut(self.kind)
            .expect("inserted above")
            .downcast_mut::<T>()
            .unwrap_or_else(|| {
                panic!(
                    "scratch key collision: kind `{}` holds the wrong type",
                    self.kind
                )
            })
    }
}

/// Summary of a completed request. Handed to `Plugin::on_response_complete`
/// (H7). Carries only what is safe to read after the request finishes —
/// the underlying `CanonicalRequest` is dropped by then.
#[derive(Debug, Clone)]
pub struct ResponseSummary {
    pub usage: Option<Usage>,
    pub elapsed_ms: u64,
    pub upstream_status: UpstreamStatus,
}

/// Why the upstream call ended. Sufficient detail for observation
/// purposes; refined error categorisation is intentionally absent so
/// plugins don't drift into trying to retry or rebrand failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamStatus {
    Success,
    Error,
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{FrontendKind, RequestId};

    #[test]
    fn plugin_context_is_clone() {
        let ctx = PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/claude-sonnet".to_string(),
        );
        let _ = ctx.clone(); // compile-time check
    }

    #[test]
    fn response_summary_holds_optional_usage() {
        let summary = ResponseSummary {
            usage: None,
            elapsed_ms: 42,
            upstream_status: UpstreamStatus::Success,
        };
        assert_eq!(summary.elapsed_ms, 42);
        assert_eq!(summary.upstream_status, UpstreamStatus::Success);
        assert!(summary.usage.is_none());
    }

    #[test]
    fn scratch_empty_by_default() {
        let ctx = PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/test".to_string(),
        );
        assert_eq!(ctx.scratch.read().len(), 0);
    }

    #[test]
    fn scratch_get_or_init_returns_typed_handle() {
        let ctx = PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/test".to_string(),
        );
        {
            let mut guard =
                ctx.scratch_get_or_init::<String, _>("plugin_a", || "hello".to_string());
            assert_eq!(&*guard, "hello");
            *guard = "mutated".to_string();
        }
        {
            let guard =
                ctx.scratch_get_or_init::<String, _>("plugin_a", || "default".to_string());
            assert_eq!(&*guard, "mutated", "second call returns prior value, not re-init");
        }
    }

    #[test]
    fn scratch_cloned_context_shares_map() {
        let ctx = PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/test".to_string(),
        );
        {
            let mut guard = ctx.scratch_get_or_init::<u32, _>("counter", || 0u32);
            *guard = 42;
        }
        let cloned = ctx.clone();
        let guard = cloned.scratch_get_or_init::<u32, _>("counter", || 0u32);
        assert_eq!(*guard, 42, "Arc-clone semantics: scratch is shared with original");
    }
}
