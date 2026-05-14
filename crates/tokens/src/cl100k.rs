//! cl100k_base encoder accessor + a `count_text` helper that uses
//! `encode_ordinary` so that any `<|...|>` literals in user content do
//! not blow up. Pattern matches the existing `OnceLock` cells in
//! `crates/frontends/src/anthropic_messages/count_tokens.rs` and
//! `crates/router/src/cost_estimate.rs` — those two are being migrated
//! to call into this module.

use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;

/// Lazily-initialised cl100k_base encoder. First call costs ~50-100ms;
/// subsequent calls are O(1). Gateway boot pre-warms this via a single
/// throw-away invocation so the cost is paid off the hot request path.
pub fn cl100k_encoder() -> &'static CoreBPE {
    static CELL: OnceLock<CoreBPE> = OnceLock::new();
    CELL.get_or_init(|| {
        tiktoken_rs::cl100k_base().expect("cl100k_base tokenizer must initialize")
    })
}

/// Count `cl100k_base` tokens in `s`. Uses `encode_ordinary` so
/// `<|...|>` literals do not trigger special-token errors.
pub fn count_text(s: &str) -> u32 {
    cl100k_encoder().encode_ordinary(s).len() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_counts_zero() {
        assert_eq!(count_text(""), 0);
    }

    #[test]
    fn ascii_text_counts_positive() {
        // "hello world" → 2 tokens in cl100k_base ([" hello", " world"]
        // style; verified by hand against tiktoken). We assert a range
        // rather than the exact number to insulate against tokenizer
        // upgrades; 1..=10 comfortably covers every variant.
        let n = count_text("hello world");
        assert!((1..=10).contains(&n), "expected 1..=10 tokens, got {n}");
    }

    #[test]
    fn special_token_literal_does_not_panic() {
        // The whole reason we use `encode_ordinary` instead of
        // `encode_with_special_tokens`: `<|endoftext|>` is a sentinel
        // in the cl100k_base table. With `encode_ordinary` it's treated
        // as ordinary text and tokenises without error.
        let n = count_text("<|endoftext|>");
        assert!(n > 0);
    }
}
