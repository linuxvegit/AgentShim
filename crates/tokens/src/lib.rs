//! Single source of truth for the `cl100k_base` BPE encoder used across
//! AgentShim. Hosts a lazily-initialised `OnceLock<CoreBPE>` plus a
//! `count_text` convenience used by:
//!
//! - `agent-shim-frontends` Anthropic-Messages token counting
//! - `agent-shim-router` cost estimation
//! - `agent-shim-plugins` `prompt_compressor`
//!
//! Initialisation costs ~50-100ms on first call (reads BPE merges from a
//! large embedded `tiktoken_rs` table). Callers in the request hot path
//! should ensure the encoder is warmed at startup; the gateway does this
//! in `AppCore::build`.

#![forbid(unsafe_code)]

mod cl100k;

pub use cl100k::{cl100k_encoder, count_text};
