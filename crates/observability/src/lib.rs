// `linkme`'s `#[distributed_slice]` machinery emits `#[link_section]`
// attributes, which rustc treats as unsafe. We explicitly mark the
// catalog module as `unsafe_attr_outside_unsafe`-allowed and use
// `deny(unsafe_code)` (instead of `forbid`) so the lint can be
// relaxed locally for that module's link-section abuse, while
// keeping unsafe forbidden everywhere else in the crate.
#![deny(unsafe_code)]

// Self-alias so `#[derive(Metric)]` expansions inside this crate can
// resolve `::agent_shim_observability::*` paths the same way downstream
// crates do. Without this, the proc-macro's emitted paths fail to
// resolve when the host crate uses its own derive on the catalog.
extern crate self as agent_shim_observability;

pub mod metrics;
pub mod otel;
pub mod redaction;
pub mod reload;
pub mod request_id;
pub mod tracing_setup;

pub use metrics::{install as install_metrics, MetricsHandle};
pub use otel::{extract_context_from_headers, inject_context_into_headers, OtelHandle};
pub use redaction::{is_sensitive, SENSITIVE_HEADERS};
pub use request_id::{RequestIdLayer, RequestIdService};
pub use tracing_setup::{init, TracingHandles};

/// Internal: re-exports the proc-macro's runtime dependency (`linkme`)
/// so users of `#[derive(Metric)]` don't need to add it themselves.
#[doc(hidden)]
pub mod __private {
    pub use linkme;
}
