//! Internal proc-macro crate for `agent-shim-observability`.
//!
//! Provides `#[derive(Metric)]`, which turns a zero-sized marker struct
//! into a fully registered metric. See the host crate's `metrics/catalog.rs`
//! for the call site.
//!
//! T4 (this commit) emits NAME/KIND/HELP consts AND a
//! `MetricDescriptor` entry in the host crate's `linkme`-collected
//! distributed slice. KIND is now an enum reference
//! (`MetricKind::Counter|Histogram|Gauge`) rather than a string.
//!
//! Plan v0.6.1 P02 (M-6).
#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
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
                Expr::Lit(ExprLit {
                    lit: Lit::Str(s), ..
                }) => s,
                _ => return Err(nested.error("expected string literal")),
            };
            match ident.as_str() {
                "name" => {
                    if name.is_some() {
                        return Err(nested.error("duplicate `name` attribute"));
                    }
                    name = Some(lit);
                }
                "kind" => {
                    if kind.is_some() {
                        return Err(nested.error("duplicate `kind` attribute"));
                    }
                    kind = Some(lit);
                }
                "help" => {
                    if help.is_some() {
                        return Err(nested.error("duplicate `help` attribute"));
                    }
                    help = Some(lit);
                }
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

    // Map the kind string to an enum-reference token stream. Validation
    // and emission happen in one step: an unknown kind returns a
    // compile_error here.
    let kind_val = kind.value();
    let kind_variant = match kind_val.as_str() {
        "counter" => quote! { ::agent_shim_observability::metrics::catalog::MetricKind::Counter },
        "histogram" => {
            quote! { ::agent_shim_observability::metrics::catalog::MetricKind::Histogram }
        }
        "gauge" => quote! { ::agent_shim_observability::metrics::catalog::MetricKind::Gauge },
        other => {
            return syn::Error::new(
                kind.span(),
                format!("unknown metric kind: `{other}` (expected counter, histogram, or gauge)"),
            )
            .to_compile_error()
            .into();
        }
    };

    // Per-struct unique static name so multiple `#[derive(Metric)]`
    // invocations in the same module don't collide. Wrapping in an
    // anonymous const block doesn't work for `#[distributed_slice]`
    // because the slice entry must be at module scope to be visible
    // to the linker.
    let descriptor_static = format_ident!("_METRIC_DESCRIPTOR_{}", struct_name);

    // We route both the `distributed_slice` proc-macro path AND the
    // `#[linkme(crate = ...)]` helper attribute through
    // `::agent_shim_observability::__private::linkme` so downstream
    // crates only need a dep on `agent-shim-observability`. The
    // `#[linkme(crate = ...)]` helper is consumed by `distributed_slice`
    // during expansion (linkme-impl element.rs:`attr::linkme_path`),
    // so attribute-resolution order matters: the proc-macro attribute
    // must come first.
    let expanded = quote! {
        impl #struct_name {
            pub const NAME: &'static str = #name;
            pub const KIND: ::agent_shim_observability::metrics::catalog::MetricKind = #kind_variant;
            pub const HELP: &'static str = #help;
        }

        #[::agent_shim_observability::__private::linkme::distributed_slice(
            ::agent_shim_observability::metrics::catalog::METRIC_DESCRIPTORS
        )]
        #[linkme(crate = ::agent_shim_observability::__private::linkme)]
        #[allow(non_upper_case_globals)]
        static #descriptor_static:
            ::agent_shim_observability::metrics::catalog::MetricDescriptor =
            ::agent_shim_observability::metrics::catalog::MetricDescriptor {
                name: #name,
                kind: #kind_variant,
                help: #help,
            };
    };

    expanded.into()
}
