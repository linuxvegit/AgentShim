//! Gemini provider (`generativelanguage.googleapis.com/v1beta`).
//!
//! Plan 03 lands this module incrementally:
//!
//! - **T2 (this commit)** introduces the wire DTOs in [`wire`].
//! - T3 adds auth + endpoint URL helpers.
//! - T4 adds the canonical → wire encoder.
//! - T5 adds the streaming JSON-array reader.
//! - T6 adds the wire → canonical parser.
//! - T7 wires everything into the [`crate::BackendProvider`] trait via a
//!   `GeminiProvider` struct + `from_config` factory.
//!
//! Submodules stay `pub(crate)` until a public surface is needed
//! (`from_config` will be the only public export when T7 lands).

pub(crate) mod wire;
