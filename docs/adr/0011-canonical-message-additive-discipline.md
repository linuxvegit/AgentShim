# ADR-0011: Canonical message types — additive lift discipline

**Status:** Accepted, 2026-06-10.
**Amends:** ADR-0007 (frozen-core lift discipline) — adds categories
(iv) and (v) to the rule-(a) list and a scope clarification to
category (iii).
**Related:** ADR-0002 (frozen-core v0.2), ADR-0006 (cost-aware routing —
the parallel disciplined lift for `providers/src/`).

## Context

The 2026-06-10 spec
`docs/superpowers/specs/2026-06-10-positional-system-messages-design.md`
fixes a customer-facing decode failure (`400 unknown role: system` from
Claude Code → `claude-opus-4-8`) by teaching the canonical model to
express system instructions that appear at any position inside the
`messages` array.

That fix requires two changes inside `crates/core/`:

1. Add a `System` variant to the existing
   `agent_shim_core::message::MessageRole` enum (`User`, `Assistant`,
   `Tool`, `System`).
2. Add a `source: Option<SystemSource>` field to the existing
   `agent_shim_core::message::Message` struct so the outbound encoder
   can distinguish `OpenAiSystem` from `OpenAiDeveloper` wire roles.

Neither change fits any of the categories ADR-0007 §(a) currently
authorises:

- (i) New `pub trait` — no new trait.
- (ii) New variant on an `#[non_exhaustive]` enum — `MessageRole` is not
  `#[non_exhaustive]`.
- (iii) New test-only type — change is production.

ADR-0007 §"hypothetical v0.7" explicitly anticipates this situation:

> If instead v0.7 wanted to add a `family: ModelFamily` field to the
> existing `pub struct ModelMetadata` (defined in
> `agent-shim-core::catalog`), that would violate rule (a) ("no field
> additions to existing structs") — the change would need its own ADR
> proposing a wider invariant change, OR a refactoring approach (e.g. a
> new struct `ModelMetadataV2` and a deprecation of `ModelMetadata`
> over multiple releases).

The "wider invariant change" route is what this ADR takes. The "new
struct with deprecation" route was considered and rejected on three
grounds: (a) it would force every frontend and provider to know about
both `Message` and `MessageV2` for multiple releases, multiplying the
surface that has to round-trip correctly; (b) ADR-0007 has never
actually exercised the deprecation route, so the cost of the first
attempt is unbounded; (c) the change we want is structurally a
backwards-compatible additive — wrapping it in a versioned-struct
migration is theatre.

## Decision

Two new categories extend ADR-0007 §(a)'s list. They take effect
2026-06-10 and apply alongside ADR-0007's existing (i), (ii), (iii).

### (iv) Variant addition to a role-class enum

A new variant may be added to an existing `pub enum` in
`agent_shim_core` without requiring `#[non_exhaustive]`, provided all
of the following hold:

1. **Internal-only consumer set.** The enum is consumed exclusively
   within the agent-shim workspace. No external crate (`Cargo.toml`
   path-dep, published crate consumer) takes a `match` on it.
2. **Exhaustive-match discipline.** Every `match` site on the enum
   inside the workspace is exhaustive (no wildcard `_ =>` arm). The
   compiler enforces "every dispatch site updated" at the moment the
   new variant is introduced — there is no silent fall-through path
   that would let a missing arm ship.
3. **Atomic dispatch update.** The same diff that adds the variant
   updates every `match` site in the enum's owning crate to handle the
   new variant explicitly. This is treated as one atomic operation: the
   diff is not partial-mergeable. Adding a variant in one commit and
   patching `match` sites in a later commit is not allowed.
4. **Wire-shape safety.** If the enum derives `Serialize` /
   `Deserialize`, the new variant's serialised form must not collide
   with an existing variant's form, and the enum must not be embedded
   in any struct whose container carries `#[serde(deny_unknown_fields)]`.

**Rationale.** `#[non_exhaustive]`'s real value is forcing external
crate `match` sites to add a wildcard arm so a future variant addition
won't break their build. For an enum consumed only inside the workspace,
the compiler's exhaustive-match check is a *stronger* guarantee than
`#[non_exhaustive]`'s warn-style protection — every dispatch site must
update before the code compiles. The attribute would only add friction
to internal match arms.

If invariant (1) ever ceases to hold — i.e. an enum currently in scope
of (iv) is exposed to an external crate — re-evaluation is required
before the next variant addition; (iv) does not auto-extend across the
boundary change.

### (v) Optional field addition to a conversation/protocol struct

A new field may be added to an existing `pub struct` in
`agent_shim_core` provided all of the following hold:

1. **Optional and skip-serializable.** The field's type is
   `Option<T>`, declared with both
   `#[serde(default, skip_serializing_if = "Option::is_none")]`.
2. **No `deny_unknown_fields` on the struct.** The owning struct does
   not carry `#[serde(deny_unknown_fields)]`. If it does, the struct
   sits behind a stricter contract that this discipline does not
   relax. (Verified at decision time: only `policy::*` types carry
   `deny_unknown_fields` in `agent_shim_core`.)
3. **Conversation/protocol scope.** The owning struct represents
   conversation or protocol data, not configuration or policy. In
   scope today: `Message`, the `ContentBlock` variants
   (`TextBlock`, `ImageBlock`, `ToolCallBlock`, `ToolResultBlock`,
   `ReasoningBlock`, `RedactedReasoningBlock`, `UnsupportedBlock`,
   `AudioBlock`, `FileBlock`, `SystemBlock`-equivalents),
   `CanonicalRequest`, `CanonicalResponse`, `Usage`,
   `SystemInstruction`. Out of scope: `RoutePolicy`, `ResolvedPolicy`,
   anything in `agent_shim_core::policy::*`, anything in
   `agent_shim_core::catalog` (catalog types follow ADR-0007 (a)
   directly).
4. **Atomic constructor update.** The same diff that adds the field
   may add or extend a constructor on the same struct whose signature
   accepts the new field's value (e.g. `Message::system(source,
   content)` accompanying `source: Option<SystemSource>` on `Message`).
   Existing constructors (`Message::user`, `Message::assistant`)
   continue to compile and default the new field to `None`.

**Rationale.** `Option<T>` with `skip_serializing_if = "None"` and
`default` is the canonical Rust pattern for safe additive fields:
- Old version reading new wire: serde ignores unknown fields (because
  the struct lacks `deny_unknown_fields`) → no failure.
- New version reading old wire: missing field → `Option::default()` =
  `None` → no failure.

The conversation/protocol-vs-policy boundary keeps strict-validation
types (route policy, config) bound by ADR-0007 (a) unchanged. The
boundary is enforced *mechanically* by precondition (2): a future
struct that needs strict validation will carry `deny_unknown_fields`
and automatically fall out of (v)'s scope.

### Scope clarifications

**(iii) test-only type — clarification.** ADR-0007 §(a) forbids
"Removing or renaming any existing public item". This rule applies to
production surface. Items behind `#[cfg(test)]` — including unit-test
function names, helper structs, and test-mod-private types — are
covered by category (iii) and may be added, modified, renamed, or
removed under that classification. The hunk classification table per
ADR-0007 §(b) marks such a hunk as `(iii) test-only`.

**Worked example for the 2026-06-10 spec:**
`crates/core/src/mapping/anthropic_wire.rs::role_unknown_returns_none`
asserts `role_from_anthropic("system")` returns `None`. After the
canonical change, that test becomes wrong: `"system"` will return
`Some(MessageRole::System)`. The test is renamed to
`role_system_returns_system_role` and its assertion flipped; a new
`role_unknown_returns_none` (with `"human"`) replaces the original
coverage. Both edits fall under (iii) per this clarification.

## Hunk classification — the 2026-06-10 spec

CHANGELOG entry for v0.10.0 must include this table per ADR-0007 §(b):

| Hunk | File | Category |
|------|------|----------|
| `MessageRole` adds `System` variant + match sites updated | `crates/core/src/message.rs`, `crates/core/src/mapping/anthropic_wire.rs` | (iv) |
| `Message` adds `source: Option<SystemSource>` + `Message::system(...)` constructor | `crates/core/src/message.rs` | (v) |
| `role_to_anthropic` adds `System => "system"` arm | `crates/core/src/mapping/anthropic_wire.rs` | (iv) — atomic dispatch update per invariant 3 |
| `role_from_anthropic` adds `"system" => Some(System)` arm | `crates/core/src/mapping/anthropic_wire.rs` | (iv) — atomic dispatch update per invariant 3 |
| `role_unknown_returns_none` test renamed and assertion flipped | `crates/core/src/mapping/anthropic_wire.rs` | (iii) per scope clarification |
| `proptest_roundtrip.rs` extends `arb_message_role` generator to cover `System` | `crates/core/tests/proptest_roundtrip.rs` | (iii) |

No struct fields removed. No existing methods' signatures changed.
No derives modified on existing types. `crates/frontends/` and
`crates/providers/src/` see their own additive changes governed by
their respective lift disciplines and are not covered by this ADR.

## Relationship with ADR-0007

ADR-0007 §(a)'s list of permitted additions is extended by (iv) and
(v); §(b)'s hunk-classification requirement now includes those
categories. §(c) ("re-state the new frozen-core baseline in the next
phase spec") and §(d) ("other frozen crates stay frozen") apply
unchanged.

ADR-0007 itself is **not edited**. ADRs are immutable; this ADR amends
the rule set going forward. Future readers cross-reference ADR-0007
and ADR-0011 together to obtain the complete rule list.

§(e) ("Removing items never used in production is permitted") is
unaffected — it remains the authority for removing dead production
surface.

## Why not require `#[non_exhaustive]` on every internal-only enum?

A blanket policy of "every `core` enum must be `#[non_exhaustive]`"
would force every workspace-internal `match` to add a wildcard arm
that the compiler-exhaustiveness check already prevents from being
useful. The wildcard either becomes `_ => unreachable!()` (silently
turning a compile error into a runtime panic when a future variant is
added) or `_ => default_behavior()` (silently letting a new variant
take the default path without considered handling). Both outcomes
are worse than the current state where adding a variant produces a
compile error at every dispatch site.

`#[non_exhaustive]` is the right tool when the enum's consumer set
includes code we don't control (downstream crate, plugin SDK, foreign
language binding). For agent-shim's workspace-internal canonical
types, the compiler is already enforcing the stronger property; the
attribute would be ceremony without protection.

## Why not introduce a `MessageV2` and deprecate `Message`?

ADR-0007 §"hypothetical v0.7" lists this as an alternative ("a new
struct `ModelMetadataV2` and a deprecation of `ModelMetadata` over
multiple releases"). For the present change we reject it because:

1. `Message` is referenced by `CanonicalRequest.messages`,
   `CanonicalResponse`, every frontend decoder, every provider
   request builder, every parser, and ~30 tests. Forcing all of them
   to know about `Message` AND `MessageV2` for multiple releases
   multiplies test surface and round-trip-correctness obligations.
2. The deprecation route has never been exercised in this codebase.
   Pioneering it for a backwards-compatible additive change is
   disproportionate to the risk being managed.
3. The change is genuinely additive: every existing serialised
   `Message` continues to deserialise correctly into the new struct
   (with `source: None`), and every new serialised `Message` with
   `source: Some(...)` continues to deserialise into older versions
   (which ignore unknown fields per precondition 2). The "compat
   bridge" is the serde mechanism, not a versioned-struct migration.

If a future change is not backwards-compatible in this way — for
example a rename of an existing field, or a change of `Option<String>`
to `String` (removing the no-value path) — the deprecation route is
the right answer and this ADR does not cover it.

## Consequences

- The 2026-06-10 spec is unblocked: the canonical changes it requires
  are authorised by (iv) and (v).
- Future "safe additive optional field" changes on conversation/protocol
  structs no longer require a per-change ADR — they cite (v) and
  classify the hunk in CHANGELOG.
- Future "internal-only role/dispatch enum gets a new variant"
  changes likewise cite (iv).
- The boundary between (v)-scoped and ADR-0007-(a)-scoped struct types
  is enforced by the `#[serde(deny_unknown_fields)]` attribute on the
  latter. A struct that wants protection from this discipline declares
  it explicitly.
- ADR-0007 §(c) requires the post-lift release (v0.10.0) to re-state
  the new frozen-core baseline in its design spec. The
  2026-06-10 positional-system-messages spec satisfies this by citing
  ADR-0011 in its rollout section and committing to the v0.10.0 release
  tag as the new diff target for v0.11+.

## Changelog

This ADR is referenced from `CHANGELOG.md` under `[0.10.0]`.
