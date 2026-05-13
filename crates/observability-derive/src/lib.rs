//! Internal proc-macro crate for `agent-shim-observability`.
//!
//! Provides `#[derive(Metric)]`, which turns a zero-sized marker struct
//! into a fully registered metric. See the host crate's `metrics/catalog.rs`
//! for the call site.
//!
//! Plan v0.6.1 P02 (M-6).
#![forbid(unsafe_code)]

use proc_macro::TokenStream;

#[proc_macro_derive(Metric, attributes(metric))]
pub fn derive_metric(_input: TokenStream) -> TokenStream {
    // T3 fills this in.
    TokenStream::new()
}
