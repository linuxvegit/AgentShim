# ADR-0007: Disciplined lift of the frozen-core invariant (v0.6.1)

**Status:** Accepted, 2026-05-13.
**Related:** ADR-0002 (frozen-core v0.2), ADR-0004 (resilient caller —
where the original invariant was reaffirmed), ADR-0006 (cost-aware
routing — the parallel disciplined unfreeze for `providers/src/`).

## Context

v0.6.1 adds the first member of `crates/core/` since v0.5.0: a new
`pub mod cost` containing `ImageTokenEstimator`, `ImageSizeHint`, and
`ImageDetail`. Required to close M-8 (non-text content blocks no longer
silently dropped from cost estimation).

Adding to `core` violates the v0.2-era frozen-core invariant verbatim
("empty diff against the baseline release"). This ADR codifies the
discipline rules that govern lifting that invariant, so v0.7+ phases
have a precedent to follow rather than treating each addition as an
ad-hoc judgement call.

The shape mirrors what Phase 6 did for `crates/providers/src/` in
ADR-0006 §2.2 — an invariant lift narrowed by enumerated rules.

## Decision

The frozen-core invariant may be lifted by a phase or release **iff
all four of the following rules are honoured**:

### (a) Trait-only additions

The only permitted additions are:
- New `pub trait` items.
- New `pub enum` variants on existing `#[non_exhaustive]` enums.
- New `pub enum` items that are themselves `#[non_exhaustive]` (companion
  types for a new trait — e.g. an input-shape enum the trait method
  takes by value).
- New `pub mod` declarations whose body contains only items from the
  permitted list above, plus `#[cfg(test)]` test modules.

The following are explicitly **not** permitted under this discipline:
- Adding fields to existing `pub struct`s.
- Removing or renaming any existing public item.
- Changing signatures on existing methods.
- Modifying derives on existing types.
- Adding `pub use` re-exports that broaden the surface beyond the
  enumerated additions.

### (b) Category tag per hunk

Each `crates/core/` diff hunk in the release that lifts the invariant
must be classified in the release's CHANGELOG entry + PR description
as one of:
- (i) new trait
- (ii) new enum variant on `#[non_exhaustive]`
- (iii) new test-only type

If a hunk doesn't fit any category, the addition is out of scope for
the disciplined lift; either narrow it (split into a separate ADR
proposing a wider invariant change) or defer it.

### (c) Re-state in the next phase spec

The phase or release after the one that lifted the invariant must
re-state the new frozen-core baseline in its design spec. The
post-lift release tag becomes the new diff target; subsequent phases
diff against that, not against the original v0.5.0 baseline.

### (d) Other frozen crates stay frozen

A lift on one frozen crate (e.g. `core`) does NOT cascade. The
sibling frozen-by-policy crates (`frontends/`, `providers/src/`)
remain bound by their respective invariants unless their own ADR
proposes a parallel lift.

## Worked example: v0.6.1's lift of `core`

v0.6.1's diff against `b90b7f3` (the v0.6.0 release point) is:

```
$ git diff b90b7f3 --stat -- crates/core/
 crates/core/src/cost.rs | 111 ++++++++++++++++++++++++++++++++++++++++++++++++
 crates/core/src/lib.rs  |   1 +
 2 files changed, 112 insertions(+)
```

Two files changed. Classification per rule (b):

| Hunk | File | Category |
|------|------|----------|
| New file `cost.rs` | trait `ImageTokenEstimator` | (i) new trait |
| `cost.rs` cont. | enum `ImageSizeHint` (`#[non_exhaustive]`) | (i) new trait + companion enum |
| `cost.rs` cont. | enum `ImageDetail` (`#[non_exhaustive]`) | (i) new trait + companion enum |
| `cost.rs` cont. | `#[cfg(test)] mod tests` | (iii) new test-only type |
| `lib.rs` +1 | `pub mod cost;` | (i) module declaration enabling above |

No struct fields modified. No existing methods renamed. No existing
enums altered. No `pub use cost::*;` re-export — the new types are
accessed via fully qualified `agent_shim_core::cost::*` paths. Both
new enums carry `#[non_exhaustive]` from inception so future variants
remain compatible with rule (a).

`crates/frontends/` and `crates/providers/src/` remain at zero diff
against `b90b7f3` per rule (d).

## Why not a public-API freeze enforced by tooling?

`cargo public-api diff --simplified crates/core/` is the natural
mechanical check, and we recommend running it in CI for each release.
However, that's a verification tool, not a substitute for the
intent-level discipline rules above. Tooling tells you *what
changed*; the rules in this ADR define *what's allowed to change*.
The two work together — `cargo public-api` produces the diff, this
ADR's (b) rule classifies each entry, and the spec's release-notes
section is where the classification gets recorded.

## A hypothetical v0.7 addition under this discipline

If a v0.7 plan wanted to add a `CapabilityHint` trait to `core` to
let frontends declare what they can/can't proxy, the diff would
look like:

```diff
+++ crates/core/src/capability_hint.rs   (new file)
+pub trait CapabilityHint: Send + Sync {
+    fn supports(&self, capability: Capability) -> bool;
+}

+++ crates/core/src/lib.rs
+pub mod capability_hint;
```

Classification: hunk 1 is `(i) new trait`, hunk 2 is `(i) module
declaration enabling above`. Both fit. The v0.7 release notes carry
the classification table; the v0.7 design spec re-states the new
post-v0.7 baseline as the diff target for v0.8.

If instead v0.7 wanted to add a `capabilities: Vec<Capability>` field
to the existing `pub struct ProviderCapabilities`, that would violate
rule (a) ("no field additions to existing structs") — the change
would need its own ADR proposing a wider invariant change, OR a
refactoring approach (e.g. a new struct `ProviderCapabilitiesV2` and
a deprecation of `ProviderCapabilities` over multiple releases).

## Consequences

- Adding to `core` is a deliberate, narrow operation with a paper
  trail (CHANGELOG + PR + ADR — for ADR-level decisions only).
- Future phases inherit a precedent for how to navigate the
  frozen-core invariant rather than re-litigating it.
- The mechanical check (`cargo public-api diff`) becomes a
  per-release release-gate verification step, not a per-PR friction.

## Related ADRs

- [ADR-0002](0002-frozen-core-v0.2.md) — the original invariant.
- [ADR-0004](0004-resilient-caller.md) — reaffirmed the invariant for
  v0.4.
- [ADR-0006](0006-cost-aware-routing.md) — parallel disciplined lift
  for `crates/providers/src/`; its §2.2 is the structural template
  for rules (a)-(d) here.

## Changelog

This ADR is referenced from `CHANGELOG.md` under `[0.6.1]`.
