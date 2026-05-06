//! Gemini provider (`generativelanguage.googleapis.com/v1beta`).
//!
//! Plan 03 lands this module incrementally:
//!
//! - **T2** introduced the wire DTOs in [`wire`].
//! - **T3** added [`auth`] + [`endpoint`] URL helpers.
//! - **T4** added the canonical → wire encoder in [`request`].
//! - **T5** added the streaming JSON-array reader in [`stream`].
//! - **T6 (this commit)** adds the wire → canonical parser in [`response`].
//! - T7 wires everything into the [`crate::BackendProvider`] trait via a
//!   `GeminiProvider` struct + `from_config` factory.
//!
//! Submodules stay `pub(crate)` until a public surface is needed
//! (`from_config` will be the only public export when T7 lands).

// Consumed by sibling submodules in Plan 03 T7 (BackendProvider impl). Until
// T7 lands, the public items are only exercised by tests, so the unused-code
// lints are gated here.
#[allow(dead_code)]
pub(crate) mod auth;
#[allow(dead_code)]
pub(crate) mod endpoint;
#[allow(dead_code)]
pub(crate) mod request;
#[allow(dead_code)]
pub(crate) mod response;
#[allow(dead_code)]
pub(crate) mod stream;
pub(crate) mod wire;
