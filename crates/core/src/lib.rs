#![forbid(unsafe_code)]

pub mod ids;
pub mod extensions;

pub use ids::{RequestId, ResponseId, ToolCallId};
pub use extensions::ExtensionMap;
