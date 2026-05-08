# Promote `gemini.safety_ratings` extension to typed canonical `Usage.safety_ratings`

**Status:** accepted (2026-05-08)

ADR-0002 named `extensions["gemini.safety_ratings"]` as the v0.3 promotion candidate, gated on "empirical cross-provider read patterns". Phase 3 closed that gate: every frontend (Anthropic Messages, OpenAI Chat, OpenAI Responses) now needs to expose filter information to agents, and they all read the same extension key with the same shape. That is the definition the v0.2 ADR was waiting for, so we promote.

The typed shape lives in `crates/core/src/safety.rs` (Plan 04 T1): `SafetyRating { category: SafetyCategory, probability: SafetyLevel }`, exposed on the canonical `Usage` struct as `Usage::safety_ratings: Option<Vec<SafetyRating>>` — its first new field since v0.1. Both `SafetyCategory` and `SafetyLevel` are **open enums** carrying an `Other(String)` variant so unknown wire values (future Gemini revisions, or other providers that adopt similar surfaces) round-trip losslessly. Same precedent as `StopReason::Other`. We considered a closed enum and rejected it: Gemini has shipped new categories before and will again, and a closed enum forces consumer breakage on the next change. We also considered carrying Gemini's wire-only `blocked: Option<bool>` flag onto canonical `SafetyRating` and rejected it — only Gemini emits it, and the canonical model represents cross-provider semantics. The `blocked` flag continues to ride on `extensions["gemini.safety_ratings"]` for the v0.3.0 overlap, then disappears from canonical-only consumers in v0.3.1.

Migration is one minor of double-write. v0.3.0 (this release) populates **both** `Usage.safety_ratings` (typed) and `extensions["gemini.safety_ratings"]` (legacy JSON). v0.3.1 drops the extension write — `attach_extensions` in `crates/providers/src/gemini/response.rs` carries explicit `// REMOVE in v0.3.1` markers at the double-write sites. Consumers reading the extension key get a one-line refactor: replace `block.extensions.get("gemini.safety_ratings")` with `usage.safety_ratings.as_ref()`.

## Consequences

- The frozen-core invariant from ADR-0002 is **partially relaxed** — exactly one new field, exactly the one ADR-0002 named. Plan files for v0.3.x and v0.4 carry `core changes: NONE` again.
- v0.3.0 → v0.3.1 migration for consumers is one line: swap the extension lookup for `usage.safety_ratings`.
- Future provider integrations (e.g. a future Anthropic safety surface) populate the same canonical field; agents read it once.
- Open-enum variants let us add wire values to `SafetyCategory` / `SafetyLevel` in minor releases without breaking consumers.
