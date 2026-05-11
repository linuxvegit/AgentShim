#![forbid(unsafe_code)]

pub mod metrics;
pub mod otel;
pub mod redaction;
pub mod request_id;
pub mod tracing_setup;

pub use metrics::{install as install_metrics, MetricsHandle};
pub use otel::{OtelHandle, TraceparentLayer, TraceparentService};
pub use redaction::{is_sensitive, SENSITIVE_HEADERS};
pub use request_id::{RequestIdLayer, RequestIdService};
pub use tracing_setup::{init, TracingHandles};
