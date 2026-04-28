#![forbid(unsafe_code)]

pub mod ids;
pub mod extensions;
pub mod error;
pub mod capabilities;
pub mod target;
pub mod media;
pub mod tool;

pub use ids::{RequestId, ResponseId, ToolCallId};
pub use extensions::ExtensionMap;
pub use error::{CoreError, StreamError};
pub use capabilities::ProviderCapabilities;
pub use target::{BackendTarget, FrontendInfo, FrontendKind, FrontendModel};
pub use media::BinarySource;
pub use tool::*;
