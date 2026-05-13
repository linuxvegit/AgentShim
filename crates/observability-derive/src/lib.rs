//! Internal proc-macro crate for `agent-shim-observability`.
//!
//! Provides `#[derive(Metric)]`, which turns a zero-sized marker struct
//! into a fully registered metric. See the host crate's `metrics/catalog.rs`
//! for the call site.
//!
//! T3 (this commit) emits NAME/KIND/HELP consts on the marker struct.
//! T4 extends the emission to register a `MetricDescriptor` in a
//! `linkme`-collected distributed slice. Until then, the consts alone
//! are enough to compile-test attribute parsing.
//!
//! Plan v0.6.1 P02 (M-6).
#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::{parse_macro_input, DeriveInput, Expr, ExprLit, Lit, LitStr};

#[proc_macro_derive(Metric, attributes(metric))]
pub fn derive_metric(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let struct_name = &input.ident;

    let mut name: Option<LitStr> = None;
    let mut kind: Option<LitStr> = None;
    let mut help: Option<LitStr> = None;

    for attr in &input.attrs {
        if !attr.path().is_ident("metric") {
            continue;
        }
        let parsed = attr.parse_nested_meta(|nested| {
            let ident = nested
                .path
                .get_ident()
                .ok_or_else(|| nested.error("expected key=value"))?
                .to_string();
            let value: Expr = nested.value()?.parse()?;
            let lit = match value {
                Expr::Lit(ExprLit { lit: Lit::Str(s), .. }) => s,
                _ => return Err(nested.error("expected string literal")),
            };
            match ident.as_str() {
                "name" => name = Some(lit),
                "kind" => kind = Some(lit),
                "help" => help = Some(lit),
                other => {
                    return Err(nested.error(format!("unknown metric attribute: {other}")));
                }
            }
            Ok(())
        });
        if let Err(e) = parsed {
            return e.to_compile_error().into();
        }
    }

    let name = match name {
        Some(n) => n,
        None => {
            return syn::Error::new(
                Span::call_site(),
                "#[derive(Metric)] requires `#[metric(name = \"...\")]`",
            )
            .to_compile_error()
            .into();
        }
    };
    let kind = match kind {
        Some(k) => k,
        None => {
            return syn::Error::new(
                Span::call_site(),
                "#[derive(Metric)] requires `#[metric(kind = \"counter|histogram|gauge\")]`",
            )
            .to_compile_error()
            .into();
        }
    };
    let help = help.unwrap_or_else(|| LitStr::new("", Span::call_site()));

    // Validate that kind is one of the three allowed values, but emit
    // it as a string for T3. T4 replaces this with an enum reference
    // once the host crate's MetricKind exists.
    let kind_val = kind.value();
    if !matches!(kind_val.as_str(), "counter" | "histogram" | "gauge") {
        return syn::Error::new(
            kind.span(),
            format!(
                "unknown metric kind: `{kind_val}` (expected counter, histogram, or gauge)"
            ),
        )
        .to_compile_error()
        .into();
    }

    let expanded = quote! {
        impl #struct_name {
            pub const NAME: &'static str = #name;
            pub const KIND: &'static str = #kind;
            pub const HELP: &'static str = #help;
        }
    };

    expanded.into()
}
