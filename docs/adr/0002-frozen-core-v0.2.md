# `agent-shim-core` is frozen for v0.2

**Status:** accepted (2026-04-30)

Phase 2 (v0.2) ships **zero changes** to `agent-shim-core`: no new fields, no new variants, no new types. Provider-specific data lives in the existing `extensions: HashMap<String, serde_json::Value>` slots on `ContentBlock`, `Message`, `CanonicalRequest`, `CanonicalResponse`, and `Usage`, with namespace prefixes — `gemini.*`, `anthropic.*`, `deepseek.*`. The locked-in v0.2 scope (DeepSeek, Gemini, Anthropic-as-backend, vision Tier-1) genuinely doesn't demand core additions: every concern fits the existing types or maps cleanly into `extensions`.

We rejected "promote-on-second-use" because predicting the second user is itself a guess. We rejected "pre-canonical-additions" (e.g. typed `gemini_safety_ratings`) because adding fields preemptively for single-provider features is YAGNI and locks the canonical model around v0.2's specific provider mix.

The single documented exception: `extensions["gemini.safety_ratings"]` is a **first-class behavior** — `docs/providers/gemini.md` describes it as a stable contract, not a debug field. Storage stays untyped in v0.2; v0.3 promotes to a typed field once we have empirical cross-provider read patterns.

## Consequences

- Phase 2 plan files all carry `core changes: NONE` in their frontmatter. Drift caught at PR review.
- Consumers reading `extensions["gemini.safety_ratings"]` in v0.2 will need a one-line refactor when v0.3 promotes it; documented in `docs/providers/gemini.md`.
- v0.3's first work item is reviewing all `extensions["<provider>.*"]` keys, identifying ones read by 2+ encoders, and promoting them. Empirical promotion replaces predictive design.
