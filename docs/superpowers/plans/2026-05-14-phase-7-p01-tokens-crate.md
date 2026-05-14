# Plan P01 — `agent-shim-tokens` leaf crate extraction (Phase 7)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-14-plugin-system-design.md`](../specs/2026-05-14-plugin-system-design.md) §3.3, §10 (alternatives), §11 risk 7. [`ADR-0008`](../../adr/0008-plugin-system.md) decision (9).

**Goal:** Extract a single source of truth for the `cl100k_base` BPE encoder into a new leaf crate `agent-shim-tokens`. Two existing call sites (`frontends::count_tokens`, `router::cost_estimate`) migrate to it. Gateway boot warms the encoder so first-request cold-start doesn't bite.

**Architecture:** New crate `crates/tokens/` with a single `OnceLock<CoreBPE>` cell exposed as `cl100k_encoder() -> &'static CoreBPE` plus a `count_text(&str) -> u32` helper. The crate depends only on `tiktoken-rs`. Two existing crates (frontends, router) migrate their inline cells to import this. Gateway startup calls `cl100k_encoder()` once after `AppCore` construction.

**Tech stack:** Existing — `tiktoken-rs`, `OnceLock` from std.

**Frozen-core impact:** None. `crates/core/` is not touched. New crate sits beside it as a peer.

**Test target:** Refactor is behaviour-preserving. Existing tests on the two migrating sites must continue to pass unchanged. One new smoke test in the new crate (~3 assertions).

---

## File Structure

`crates/tokens/` (new):
- Create: `crates/tokens/Cargo.toml`
- Create: `crates/tokens/src/lib.rs` — module declarations + crate-level docs
- Create: `crates/tokens/src/cl100k.rs` — `cl100k_encoder()` + `count_text()` + tests

`crates/frontends/`:
- Modify: `crates/frontends/src/anthropic_messages/count_tokens.rs` — remove local `OnceLock<CoreBPE>` cell, replace `count_text(&str)` body with delegation to `agent_shim_tokens::count_text`. Remove `tiktoken_rs` direct use.
- Modify: `crates/frontends/Cargo.toml` — replace `tiktoken-rs.workspace = true` with `agent-shim-tokens = { path = "../tokens" }`.

`crates/router/`:
- Modify: `crates/router/src/cost_estimate.rs` — remove local `OnceLock<CoreBPE>` cell. The local helper functions stay (they extend beyond just counting text), but the underlying encoder access goes through `agent_shim_tokens::cl100k_encoder()`. Remove the inline cell + `OnceLock` import.
- Modify: `crates/router/Cargo.toml` — replace `tiktoken-rs.workspace = true` with `agent-shim-tokens = { path = "../tokens" }`.

Workspace:
- Modify: `Cargo.toml` (workspace root) — add `crates/tokens` to `members`.

Gateway:
- Modify: `crates/gateway/src/state.rs::AppCore::build` — after `let metrics = install_metrics(...)` and before `let latency_probe = ...`, add a one-line warmup call. The exact line in §6.8 of the spec; effectively `let _ = agent_shim_tokens::cl100k_encoder();`. Optional comment explaining purpose.
- Modify: `crates/gateway/Cargo.toml` — add `agent-shim-tokens = { path = "../tokens" }` to dependencies. (Needed for the warmup call.)

---

## Tasks

### Task 1: Create the `agent-shim-tokens` crate skeleton

**Files:**
- Create: `crates/tokens/Cargo.toml`
- Create: `crates/tokens/src/lib.rs`

- [ ] **Step 1: Write `crates/tokens/Cargo.toml`**

```toml
[package]
name = "agent-shim-tokens"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
tiktoken-rs.workspace = true
```

- [ ] **Step 2: Write `crates/tokens/src/lib.rs`**

```rust
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
```

- [ ] **Step 3: Add the crate to the workspace**

Open `Cargo.toml` at the repo root. Find the `members = [...]` array under `[workspace]`. Add `"crates/tokens"` keeping the existing ordering style (alphabetical-by-segment is closest to current). The result around members should look like:

```toml
[workspace]
resolver = "2"
members = ["crates/core", "crates/config", "crates/observability", "crates/observability-derive", "crates/gateway", "crates/frontends", "crates/protocol-tests", "crates/providers", "crates/router", "crates/tokens"]
```

- [ ] **Step 4: Verify the workspace builds with just the empty `lib.rs`**

Run: `cargo check -p agent-shim-tokens`
Expected: clean (1 warning about `cl100k` being declared but the module file doesn't exist yet, OR a compile error — that's fine since we'll add the file in Task 2).

If the error is anything other than "file not found for module `cl100k`" or "could not find file for module `cl100k`", stop and investigate.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/tokens/Cargo.toml crates/tokens/src/lib.rs
git commit -m "feat(tokens): add agent-shim-tokens crate skeleton (P01 T1)"
```

---

### Task 2: Implement `cl100k_encoder` and `count_text`

**Files:**
- Create: `crates/tokens/src/cl100k.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/tokens/src/cl100k.rs` with this content:

```rust
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
```

- [ ] **Step 2: Run tests to verify all three pass**

Run: `cargo test -p agent-shim-tokens`
Expected: 3 passed; 0 failed.

If any test fails, stop and investigate. The most likely cause is `tiktoken-rs` not being in the workspace deps (check workspace `Cargo.toml`).

- [ ] **Step 3: Commit**

```bash
git add crates/tokens/src/cl100k.rs
git commit -m "feat(tokens): cl100k_encoder + count_text with smoke tests (P01 T2)"
```

---

### Task 3: Migrate `frontends::count_tokens` to use the new crate

**Files:**
- Modify: `crates/frontends/src/anthropic_messages/count_tokens.rs`
- Modify: `crates/frontends/Cargo.toml`

- [ ] **Step 1: Find the exact lines to change**

Open `crates/frontends/src/anthropic_messages/count_tokens.rs`. Locate the `count_text` function around line 106-113. It looks like:

```rust
/// Count the cl100k_base tokens in a string. Uses `encode_ordinary` so
/// `<|...|>` literals in user content do not blow up.
fn count_text(s: &str) -> u32 {
    static CELL: OnceLock<CoreBPE> = OnceLock::new();
    let bpe = CELL
        .get_or_init(|| tiktoken_rs::cl100k_base().expect("cl100k_base tokenizer must initialize"));
    bpe.encode_ordinary(s).len() as u32
}
```

Also locate the top-of-file imports — `use tiktoken_rs::CoreBPE;` and `use std::sync::OnceLock;` (or similar).

- [ ] **Step 2: Replace the function body**

Edit `crates/frontends/src/anthropic_messages/count_tokens.rs`. Replace the `count_text` function (the full block above, including the doc comment) with:

```rust
/// Count the cl100k_base tokens in a string. Uses `encode_ordinary` so
/// `<|...|>` literals in user content do not blow up.
///
/// Delegates to `agent_shim_tokens::count_text` — the single
/// workspace-wide source of truth for the cl100k encoder. (P01 T3.)
fn count_text(s: &str) -> u32 {
    agent_shim_tokens::count_text(s)
}
```

- [ ] **Step 3: Remove now-unused imports**

In the same file, near the top, delete:
- `use tiktoken_rs::CoreBPE;` (no longer used here)
- `use std::sync::OnceLock;` if it was used only by the cell (rustc will tell you if a different fn also uses it)

If rustc complains that `OnceLock` is still needed elsewhere in the file, keep that import.

- [ ] **Step 4: Update `crates/frontends/Cargo.toml`**

Open `crates/frontends/Cargo.toml`. Find the `[dependencies]` section. Replace the line:

```toml
tiktoken-rs.workspace = true
```

with:

```toml
agent-shim-tokens = { path = "../tokens" }
```

If `tiktoken-rs` was used elsewhere in the crate (search via `grep "tiktoken" crates/frontends/src` first to be sure), keep it. Otherwise this swap is clean.

- [ ] **Step 5: Build the crate**

Run: `cargo check -p agent-shim-frontends`
Expected: clean, no warnings about unused imports.

If `tiktoken_rs` is still imported somewhere else, that's a sign Step 4 was premature — restore the workspace dep until the other usage is migrated separately.

- [ ] **Step 6: Run existing tests to verify behaviour preservation**

Run: `cargo test -p agent-shim-frontends`
Expected: all pre-existing tests pass. None should change. (count_text now lives behind a function call instead of an inline cell — the function-call wrapper preserves the original signature and behaviour exactly.)

- [ ] **Step 7: Commit**

```bash
git add crates/frontends/Cargo.toml crates/frontends/src/anthropic_messages/count_tokens.rs
git commit -m "refactor(frontends): migrate count_text to agent-shim-tokens (P01 T3)"
```

---

### Task 4: Migrate `router::cost_estimate` to use the new crate

**Files:**
- Modify: `crates/router/src/cost_estimate.rs`
- Modify: `crates/router/Cargo.toml`

- [ ] **Step 1: Locate the cell and its accessor**

Open `crates/router/src/cost_estimate.rs`. Around line 75-80 there's a private accessor function similar to:

```rust
/// Lazily-initialised cl100k_base tokenizer. Matches the pattern in
/// `count_tokens.rs`.
fn bpe() -> &'static CoreBPE {
    static CELL: OnceLock<CoreBPE> = OnceLock::new();
    CELL.get_or_init(|| tiktoken_rs::cl100k_base().expect("cl100k_base tokenizer must initialize"))
}
```

The exact name may be `bpe`, `cl100k_bpe`, or similar — use Grep to confirm: `grep -n "OnceLock\|tiktoken_rs::cl100k_base" crates/router/src/cost_estimate.rs`.

- [ ] **Step 2: Replace the accessor's body**

Edit the `bpe()` function. The new body is a single delegation:

```rust
/// Returns the cl100k_base encoder. Delegates to
/// `agent_shim_tokens::cl100k_encoder` — single workspace-wide source
/// of truth. (P01 T4.)
fn bpe() -> &'static tiktoken_rs::CoreBPE {
    agent_shim_tokens::cl100k_encoder()
}
```

Note: keep the function name (`bpe` or whatever it was) so call sites elsewhere in the file continue to compile unchanged.

- [ ] **Step 3: Remove now-unused imports**

At the top of `crates/router/src/cost_estimate.rs`:
- Delete `use std::sync::OnceLock;` if no other `OnceLock` exists in the file (grep first).
- Keep `use tiktoken_rs::CoreBPE;` — the function return type still mentions it. (Or fully qualify it as `tiktoken_rs::CoreBPE` in the signature and remove the import — engineer's choice based on style consistency with the rest of the file.)

- [ ] **Step 4: Update `crates/router/Cargo.toml`**

Open `crates/router/Cargo.toml`. Find `tiktoken-rs.workspace = true` in dependencies. There are two valid options:

**Option A (preferred when `tiktoken_rs` is only used for the encoder accessor):** Replace it with:

```toml
agent-shim-tokens = { path = "../tokens" }
```

**Option B (when `tiktoken_rs::CoreBPE` is still referenced as a type in this crate):** Keep `tiktoken-rs` AND add `agent-shim-tokens`. The cost is one extra workspace dep edge; the gain is no plumbing of type re-exports through `agent-shim-tokens`.

First grep: `grep -n "tiktoken_rs::" crates/router/src/` — if the only mention is the return type of `bpe()`, go with Option A and fully-qualify in the signature. Otherwise Option B.

- [ ] **Step 5: Build the crate**

Run: `cargo check -p agent-shim-router`
Expected: clean.

- [ ] **Step 6: Run existing tests**

Run: `cargo test -p agent-shim-router`
Expected: all pre-existing tests pass unchanged.

- [ ] **Step 7: Commit**

```bash
git add crates/router/Cargo.toml crates/router/src/cost_estimate.rs
git commit -m "refactor(router): migrate cost_estimate to agent-shim-tokens (P01 T4)"
```

---

### Task 5: Add gateway-boot warmup call

**Files:**
- Modify: `crates/gateway/Cargo.toml`
- Modify: `crates/gateway/src/state.rs`

- [ ] **Step 1: Add the workspace dep**

Open `crates/gateway/Cargo.toml`. Add to `[dependencies]`:

```toml
agent-shim-tokens = { path = "../tokens" }
```

- [ ] **Step 2: Locate the warmup insertion point**

Open `crates/gateway/src/state.rs`. Find `AppCore::build` — the helper around line 193. Inside it, locate the line:

```rust
let metrics = agent_shim_observability::install_metrics(&config.metrics);
```

(approximately line 300 in the current file).

The warmup needs to run after the global metrics recorder is installed (so the warmup itself can be measured if we ever add a `tiktoken_init_duration_seconds` metric) and before anything that calls into the encoder. The cleanest place is **immediately after `install_metrics`**.

- [ ] **Step 3: Insert the warmup line**

Edit `crates/gateway/src/state.rs`. Immediately after `let metrics = agent_shim_observability::install_metrics(&config.metrics);`, add a blank line and:

```rust
// Plan 07 P01 T5: pre-warm the cl100k tokenizer so the first request
// doesn't pay the ~50-100ms init cost on the hot path. The result is
// dropped — we only care about the side-effect of populating the
// `OnceLock` cell inside `agent-shim-tokens`.
let _ = agent_shim_tokens::cl100k_encoder();
```

- [ ] **Step 4: Build the crate**

Run: `cargo check -p agent-shim`
Expected: clean.

- [ ] **Step 5: Run existing tests**

Run: `cargo test -p agent-shim`
Expected: all pre-existing tests pass.

- [ ] **Step 6: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: all green. The behaviour-preservation guarantee for this plan rests on this step.

- [ ] **Step 7: Commit**

```bash
git add crates/gateway/Cargo.toml crates/gateway/src/state.rs
git commit -m "feat(gateway): warm cl100k encoder at boot (P01 T5)"
```

---

### Task 6: Run clippy and format checks

- [ ] **Step 1: Run clippy on the workspace**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

If clippy flags anything in the migrated files (most common: unused-import lint after a partial removal), fix in place. If it flags something in the new `agent-shim-tokens` crate, fix there.

- [ ] **Step 2: Run rustfmt**

Run: `cargo fmt --all -- --check`
Expected: clean.

If something needs formatting, run `cargo fmt --all` and commit separately:

```bash
git add -u
git commit -m "style: cargo fmt (P01 T6)"
```

- [ ] **Step 3: Final regression run**

Run: `cargo nextest run --workspace` (or `cargo test --workspace` if nextest is unavailable).
Expected: same test count as the baseline before this plan started, all green.

If the count differs by 3 (the three new tests from Task 2), that's expected and correct.

- [ ] **Step 4: No commit needed for verification**

Verification doesn't change files. Move on.

---

## Acceptance criteria

- `crates/tokens/` exists as a leaf crate with `cl100k_encoder()` and `count_text()`.
- No `OnceLock<CoreBPE>` cells remain in `crates/frontends/` or `crates/router/` — `grep -rn "OnceLock" crates/frontends/src crates/router/src | grep -i tikt` must return zero hits.
- `crates/gateway/src/state.rs::AppCore::build` calls `cl100k_encoder()` once after `install_metrics`.
- `cargo nextest run --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` both clean.
- `cargo deny check` (if it runs in CI) remains clean — no new external deps introduced.

## Notes for the implementer

- The plan never touches `crates/core/` so ADR-0007's frozen-core discipline is not exercised.
- The new crate has zero dependencies beyond `tiktoken-rs` which is already in the workspace.
- The migration of `cost_estimate.rs` may need Option B (keep `tiktoken-rs` dep on `router`) if `CoreBPE` type usage extends beyond the encoder accessor. Don't force Option A — grep first.
- If you discover that some test was depending on a specific behaviour of the per-crate `OnceLock` cell (very unlikely — encoders are pure functions of the static BPE table), the test was wrong; do not paper over the issue.
