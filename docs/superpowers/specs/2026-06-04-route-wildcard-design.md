# Route Model Prefix Wildcard — Design Spec

**Date:** 2026-06-04
**Status:** Approved (brainstorming → writing-plans)
**Scope:** `crates/config` (schema + validation), `crates/router` (`StaticRouter`)
**Owners:** gateway routing layer
**Related:** `docs/configuration.md` §Routes, `docs/architecture.md` §Router

---

## 1. Motivation

Today `RouteEntry.model` supports exactly two forms:

- **Exact** — `claude-sonnet-4-5`
- **Catch-all wildcard** — `"*"` (one per frontend)

Operators who want to route a whole *family* of models (every `claude-*`
through Copilot, every `glm*` through the GLM upstream) have only two
working options: enumerate every concrete alias by hand, or fall back to
the single `"*"` catch-all and lose the per-family upstream split.

Real-world example that motivated this spec:

```yaml
routes:
  - frontend: anthropic_messages
    model: claude-*
    upstream: copilot
    upstream_model: "*"
    reasoning_effort: xhigh
  - frontend: anthropic_messages
    model: glm*
    upstream: glm
    upstream_model: "*"
```

The agent sends any `claude-sonnet-4-5` / `claude-opus-4-7` / etc. and
hits Copilot; any `glm-4-plus` / `glm-4-air` / etc. hits the GLM
upstream. No per-model enumeration required.

This spec adds a **third form** for `model`: a **prefix pattern**, a
literal string ending in a single `*`. It keeps the existing `upstream_model: "*"`
pass-through semantics, makes routing deterministic via longest-prefix-wins,
and changes zero behavior for already-deployed configs.

---

## 2. Configuration schema

### 2.1 `RouteEntry.model` — allowed forms (after this spec)

| Form               | Example                  | Match semantics                                                                           |
|--------------------|--------------------------|-------------------------------------------------------------------------------------------|
| Exact string       | `claude-sonnet-4-5`      | Hits iff inbound `model` equals this string exactly.                                      |
| **Prefix wildcard (new)** | `claude-*`, `glm*` | A literal followed by a single `*` at the very end. Hits when inbound `model` starts with the literal prefix (`"claude-"`, `"glm"`). |
| Full wildcard      | `*`                      | Any inbound `model` hits; sole catch-all per frontend (unchanged behavior).               |

### 2.2 `RouteEntry.upstream_model` — unchanged

- `"*"` — pass the inbound `model` through verbatim to the upstream.
- Any other string — literal override sent to the upstream.

**No template / capture syntax is introduced.** A prefix route with
`upstream_model: "claude-3-5-sonnet-20241022"` rewrites every
`claude-*` request to that single upstream model.

### 2.3 New schema validation (in `validate_routes`)

A single new rule, enforced at config load before any route enters the
router. Stated as one predicate so there is exactly one way to read it:

> `model` is valid iff **at least one** of the following holds:
>
> 1. `model == "*"` (the full-wildcard catch-all), OR
> 2. `model` does not contain `*` at all (an exact alias), OR
> 3. `model` ends with exactly one `*`, that `*` is the only `*` in the
>    string, and the literal prefix (everything before the trailing
>    `*`) is non-empty.

Any other shape is rejected. Rejected examples and the reason each one
fails the predicate:

| `model` value         | Fails because                                                            |
|-----------------------|--------------------------------------------------------------------------|
| `claude-*-thinking`   | Contains `*` but does not end with `*` (clause 3 fails)                  |
| `*claude*`            | Contains two `*` characters (clause 3 requires exactly one)              |
| `*claude`             | Contains `*` but does not end with `*` (clause 3 fails)                  |
| `claude-**`           | Contains two `*` characters (clause 3 requires exactly one)              |
| `**`                  | Contains two `*` characters; also clause 1 requires the whole string to be a single `*` |

Error message format (sourced from existing `validate_routes` style,
returned as `Result<(), String>` and wrapped by `ValidationError::InvalidRoute`):

```
route[{i}] '{frontend}/{model}': '*' is only allowed at the end of the
model pattern (e.g. "claude-*") or as the sole character ("*")
```

### 2.4 Intentional non-errors (write into docs)

These are **not** validation failures by design. They are documented
here and in `docs/configuration.md` so operators reading either source
get the same answer:

- **Overlapping prefixes are allowed.** `claude-*` and `claude-3-*` may
  coexist on the same frontend; the router picks the longest-prefix
  match at lookup time (§3).
- **Duplicate prefixes (or duplicate exact models) are allowed and
  silently overwrite.** Two routes with identical `(frontend, model)`
  on the same frontend follow the existing `HashMap`-insertion rule:
  the later entry replaces the earlier one. This matches today's
  behavior for duplicate exact routes — no new uniqueness check is
  introduced. Operators get neither warning nor error; the only signal
  is that the earlier route's settings (retry, breaker, upstreams)
  vanish from the running config.

### 2.5 Backward compatibility

- Every existing config keeps working unchanged. Configs without `*` in
  `model` go to the exact table; configs with `model: "*"` go to the
  wildcard table; neither path is touched.
- No new serde fields. `RouteEntry` shape is byte-for-byte the same on
  the wire.

---

## 3. Matching and priority algorithm

### 3.1 Lookup order in `StaticRouter::resolve_route(frontend, model)`

1. **Exact** — `exact.get(&(frontend, model))`. Hit returns immediately,
   skipping all subsequent steps. O(1).
2. **Prefix** — fetch `prefixes.get(&frontend)` (the per-frontend prefix
   route list, sorted at startup by literal-prefix length descending).
   Linear-scan; the first entry with `model.starts_with(&p.prefix)` wins.
   Because the list is length-descending, the first hit IS the longest
   prefix. O(N) where N = number of prefix routes on this frontend
   (real-world N is < 10).
3. **Wildcard** — `wildcards.get(&frontend)` (the existing `"*"`
   catch-all, at most one per frontend).
4. **No match** — `Err(RouteError::NoRoute { frontend, model: model.to_string() })`.
   Identical error type and fields as today.

The three tiers' overall priority is the natural consequence of the
lookup order:

> **exact > prefix (longest first) > wildcard**

### 3.2 Tie-breaking within step 2

When two prefix routes match the same inbound `model`:

- **Primary key: literal prefix length, descending.**
  `claude-3-` (length 9) beats `claude-` (length 7) for inbound
  `claude-3-haiku`.
- **Secondary key: YAML appearance order**, used only when two prefix
  routes have the **same literal length on the same frontend** — first
  in YAML wins. Per §2.4 duplicate prefixes silently overwrite, so this
  case effectively only fires when two *distinct but same-length*
  prefixes are written by an operator (`abc-*` and `xyz-*` both length
  4, both registered on the same frontend, both matching `abcxyz-...`).
  This is a corner case; the secondary key exists to make ordering
  deterministic rather than HashMap-iteration-dependent.

### 3.3 Internal data structure changes (`StaticRouter`)

```rust
pub struct StaticRouter {
    // Renamed from `routes` for clarity now that there are three tiers.
    exact: HashMap<(FrontendKind, String), Vec<BackendTarget>>,
    exact_entries: HashMap<(FrontendKind, String), RouteEntry>,

    // NEW: per-frontend prefix table, sorted prefix-length descending at startup.
    prefixes: HashMap<FrontendKind, Vec<PrefixRoute>>,

    wildcards: HashMap<FrontendKind, WildcardTarget>,
    wildcard_entries: HashMap<FrontendKind, RouteEntry>,
}

struct PrefixRoute {
    prefix: String,                    // `model` with the trailing `*` stripped
    chain: Vec<BackendTarget>,         // identical shape to exact-route chains
    upstream_model_passthrough: bool,  // true when source `upstream_model == "*"`
    entry: RouteEntry,                 // mirror of exact_entries / wildcard_entries
}
```

`from_config` partitions each `RouteEntry` into exactly one of the three
buckets based on its `model` field shape (exact / prefix / `"*"`).

### 3.4 `upstream_model: "*"` pass-through under prefix routes

When a prefix route hits and `upstream_model_passthrough == true`, the
router overwrites `chain[0].model` with the inbound `model` string
before returning. Same semantics as today's wildcard pass-through, just
plumbed through the new tier.

This rewrite happens **before** `ModelResolver::apply_fuzzy_upgrades`,
so the passed-through name still gets upgraded to the upstream's
discovered canonical form (e.g. `claude-sonnet-4-5` →
`claude-sonnet-4-5-20250514`). The fuzzy upgrade is per-chain-element
and unaware of which tier produced the chain.

### 3.5 `route_label`

Prefix routes construct `route_label` from the **inbound** model name
(not the prefix pattern), matching the existing wildcard behavior. This
keeps metrics keyed by real model identity rather than by routing
pattern, which is what operators want when slicing latency/cost dashboards.

### 3.6 Complexity

- Exact hit (≥99% of production traffic against a well-tuned config):
  O(1), identical to today.
- Prefix hit: O(P) where P is the per-frontend prefix-route count;
  `starts_with` is O(prefix length). In practice, both factors are
  small constants.
- Wildcard / NoRoute: unchanged.

The hot path (exact hits) does not regress.

---

## 4. Catalog / `list_routes` / `/v1/models` behavior

Decision: **all wildcard forms (both `"*"` and `claude-*`) are excluded
from every public catalog surface.** Public endpoints continue to list
only concretely enumerable model ids.

### 4.1 `StaticRouter::list_routes` — zero code change

Today's implementation iterates `exact_entries.keys()`. After this
spec, prefix routes live in `self.prefixes` (a separate bucket and
NOT in `exact_entries`), so `list_routes` is automatically free of
prefix-pattern entries. Bucket-based partitioning means the filter is
structural; there is no `if pattern { skip }` check to forget.

### 4.2 Downstream surfaces — zero code change

All of these go through `ModelResolver::list_catalog → StaticRouter::list_routes`
and therefore inherit the exclusion automatically:

| Surface                                            | Code change | Behavior                                                      |
|----------------------------------------------------|-------------|---------------------------------------------------------------|
| `GET /v1/models` (public)                          | none        | Prefix wildcards (and `"*"`) absent                            |
| `GET /v1/models/:id` (public)                      | none        | `GET /v1/models/claude-*` → 404, same as `GET /v1/models/*`    |
| `GET /admin/catalog` (admin)                       | none        | Same as public catalog                                         |
| `agent-shim show-catalog`                          | none        | Same exclusion                                                 |
| `logging.print_catalog_on_start` startup dump      | none        | Same exclusion                                                 |

### 4.3 `long_context_variant` / fuzzy upgrade

Both operate per `ModelRecord`. Since prefix routes never become
`ModelRecord`s, neither mechanism needs special-casing.

### 4.4 Operator visibility (deferred, not in this spec)

A future `StaticRouter::list_prefix_routes() -> Vec<(FrontendKind, String)>`
could surface prefix patterns in `/admin/catalog` or a new
`show-catalog --include-patterns` flag. This is **explicitly out of
scope** for this spec — operators read prefix routes from `gateway.yaml`
today, and `print_catalog_on_start` already shows the exact-route
outcome of the config. We will add the helper when a concrete operator
need surfaces.

---

## 5. `strict_upstream_models` behavior

`validate_with_index` is **not modified by this spec**. The existing
implementation reads only `entry.upstream_model` and `entry.upstream`;
it never reads `entry.model`. Therefore prefix routes are validated
exactly the same way as exact and wildcard routes — automatically and
without any code change.

The decision rule, stated explicitly so operators don't have to derive
it from code:

> `validate_with_index` decides whether to check a route by looking
> **only at `upstream_model`**:
>
> - `upstream_model == "*"` → skipped (the literal value is not known
>   until request time)
> - `upstream_model == <literal>` → enforced: `(upstream, upstream_model)`
>   must appear in the discovered catalog
>
> The `model` field's shape (exact / prefix / `"*"`) is irrelevant to
> this decision.

### 5.1 Behavior matrix

| `model`             | `upstream_model`                | strict checks? | Rationale                          |
|---------------------|---------------------------------|----------------|------------------------------------|
| `claude-sonnet-4-5` (exact)     | `claude-sonnet-4-5-20241022` (literal) | ✅ yes | unchanged from today               |
| `claude-sonnet-4-5` (exact)     | `*` (pass-through)              | ⏭️ no          | unchanged from today               |
| `claude-*` (**prefix, new**)    | `claude-3-5-sonnet-20241022` (literal) | ✅ yes | literal typos still caught at startup |
| `claude-*` (**prefix, new**)    | `*` (pass-through)              | ⏭️ no          | matches motivating example         |
| `*` (full wildcard) | `*` (pass-through)              | ⏭️ no          | unchanged from today               |
| `*` (full wildcard) | `gpt-4o` (literal)              | ✅ yes         | unchanged from today               |

---

## 6. Error handling

### 6.1 Runtime errors — sole shape unchanged

| Scenario                                                                  | Detected at                          | Error                                                                  |
|---------------------------------------------------------------------------|--------------------------------------|------------------------------------------------------------------------|
| Inbound `model` matches no exact, no prefix, no wildcard route            | `StaticRouter::resolve_route` end    | `RouteError::NoRoute { frontend, model: inbound.to_string() }` — same enum, same fields, same HTTP mapping as today |
| Prefix route hits but `chain[0].provider` is not in `cfg.upstreams`       | impossible                           | Already caught by `validate_routes` rule 3 at startup                  |

**No new `RouteError` variant.** All call sites that pattern-match on
`RouteError { ... }` (including `handlers.rs` HTTP error mapping)
continue to compile and behave identically.

### 6.2 Startup errors — one new rule

| Scenario                              | Detected at         | Error                                                                                                                                |
|---------------------------------------|---------------------|--------------------------------------------------------------------------------------------------------------------------------------|
| `model` has `*` in an illegal position | `validate_routes`   | `Err(format!("route[{i}] '{frontend}/{model}': '*' is only allowed at the end of the model pattern (e.g. \"claude-*\") or as the sole character (\"*\")"))` — wrapped by `ValidationError::InvalidRoute`, same pipe as rules 1–7 today |

The message names a working example so the operator can fix the typo
without consulting documentation.

### 6.3 Intentional non-errors

- Overlapping prefixes — feature, resolved by longest-prefix-wins (§3.2).
- Duplicate prefixes (or duplicate exact models) — silently overwrite,
  matching today's exact-route behavior (§2.4).
- Prefix hit + fuzzy upgrade miss — silent no-op, matching today's
  `ModelResolver::apply_fuzzy_upgrades` behavior on `None`.
- `upstream_model: "*"` pass-through value missing from the upstream
  catalog at runtime — surfaces as the upstream's own 4xx, same as
  today's `"*"` wildcard. `strict_upstream_models` cannot catch this
  by design.

### 6.4 Logging

- **Prefix hit:** `tracing::debug!` with `frontend`, `inbound_model`,
  `matched_prefix`, `provider`, `upstream_model`. Debug-level (not
  info) so a busy gateway doesn't drown its log stream; switch to
  `agent_shim_router=debug` when investigating routing issues.
- **Startup validation failure:** existing `agent-shim serve` error
  path — gateway exits before `AppState::new` returns, with the
  human-readable `InvalidRoute` message.

---

## 7. Testing strategy

Four layers, mirroring today's test organization.

### 7.1 `config::validation` unit tests (`crates/config/src/validation.rs`)

Schema-validation coverage, added to the existing `validate_routes`
test module:

- `validate_routes_accepts_prefix_pattern` — `model: "claude-*"` and
  `model: "glm*"` both pass.
- `validate_routes_accepts_exact_and_full_wildcard` — regression guard
  for `model: "claude-sonnet-4-5"` and `model: "*"`.
- `validate_routes_rejects_star_in_middle` — `model: "claude-*-thinking"`
  rejected with message containing `'*' is only allowed at the end`.
- `validate_routes_rejects_star_at_start` — `model: "*claude*"` and
  `model: "*claude"` rejected.
- `validate_routes_rejects_multiple_stars` — `model: "claude-**"` and
  `model: "**"` rejected (`**` is not equivalent to `*`).
- `validate_routes_allows_overlapping_prefixes` — `claude-*` and
  `claude-3-*` on the same frontend both pass.
- `validate_routes_allows_duplicate_prefix_silent_override` — two
  identical `claude-*` entries on the same frontend pass; the second
  wins at router level.
- `validate_with_index_checks_literal_upstream_model_under_prefix_route`
  — `model: "claude-*"` + `upstream_model: "claude-typo"` +
  `strict_upstream_models: true` → fails (regresses §5 matrix).
- `validate_with_index_skips_wildcard_upstream_model_under_prefix_route`
  — `model: "claude-*"` + `upstream_model: "*"` + strict → passes.

### 7.2 `router::static_routes` unit tests (`crates/router/src/static_routes.rs`)

Matching algorithm and priority coverage:

- `prefix_route_passes_inbound_model_through_when_upstream_model_is_star`
  — `claude-*` + `*`, inbound `claude-sonnet-4-5` →
  `chain[0].model == "claude-sonnet-4-5"`.
- `prefix_route_overrides_model_when_upstream_model_is_literal` —
  `claude-*` + `claude-3-5-sonnet-20241022`, inbound `claude-anything`
  → `chain[0].model == "claude-3-5-sonnet-20241022"`.
- `exact_route_beats_prefix_route` — both `claude-sonnet-4-5` (exact)
  and `claude-*` exist; inbound `claude-sonnet-4-5` hits exact.
- `longest_prefix_wins` — `claude-*`, `claude-3-*`, `claude-3-5-*` on
  same frontend; inbound `claude-3-5-sonnet` hits `claude-3-5-*`;
  `claude-3-haiku` hits `claude-3-*`; `claude-opus-4-7` hits
  `claude-*`.
- `prefix_falls_back_to_full_wildcard_on_miss` — `claude-*` and `"*"`
  coexist; inbound `gpt-4o` hits `"*"`.
- `prefix_route_returns_no_route_on_complete_miss` — no route matches
  → `RouteError::NoRoute` with original enum shape preserved.
- `prefix_route_carries_route_label_with_inbound_model` — `route_label`
  uses inbound model (not prefix), matching wildcard behavior.
- `prefix_route_uses_route_specific_retry_and_breaker` — per-route
  retry/breaker propagates correctly under prefix routes.
- `prefix_route_carries_reasoning_mapping_to_policy` — `reasoning_mapping`
  propagates correctly under prefix routes.
- `duplicate_prefix_uses_last_definition` — two identical `claude-*`
  entries; the second's chain wins at runtime.
- `cross_frontend_prefix_isolation` — `anthropic_messages: claude-*`
  does NOT catch `openai_chat: claude-foo`.

### 7.3 `router::resolver` unit tests (`crates/router/src/resolver.rs`)

Integration of prefix routing with fuzzy upgrade and catalog:

- `prefix_route_then_fuzzy_upgrades_per_chain_element` — `claude-*` +
  Copilot + `*`, inbound `claude-sonnet-4-5`, ModelIndex holds
  `claude-sonnet-4-5-20250514` → final `chain[0].model` is the upgraded
  canonical name.
- `list_catalog_excludes_prefix_routes` — config with one prefix route
  and one exact route → catalog returns only the exact one.

### 7.4 Property test (one, intentionally)

- `proptest_longest_prefix_is_deterministic` — random N ≤ 8 distinct
  prefixes on one frontend + one inbound model. Assert: the winning
  prefix is the longest among all that satisfy `inbound.starts_with(prefix)`.
  Guards against an accidental switch to `BTreeMap` iteration order or
  any other change that would break length-descending stability.

### 7.5 Out of scope (YAGNI)

- **No E2E tests.** Existing `protocol-tests` cover the full request
  path on exact/wildcard routes. Prefix routing is transparent to
  every downstream layer (provider call, SSE encoding); E2E adds no
  signal.
- **No gateway-crate tests.** `serve.rs`/`handlers.rs` go through
  `ModelResolver::resolve`, which returns the same `Vec<BackendTarget>`
  shape regardless of routing tier.

### 7.6 Pass criteria

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo deny check
```

All four must be green.

---

## 8. Documentation updates

`docs/configuration.md` §Routes is updated to:

1. List `prefix wildcard` as a third allowed shape for `model` (the
   table in §2.1 of this spec, condensed).
2. State the priority rule: `exact > longest-prefix > "*"`, with a
   one-sentence note that same-length prefixes break ties by YAML order.
3. State the duplicate-route rule: identical `(frontend, model)` (any
   shape) silently overwrites; the later entry wins.
4. State the `strict_upstream_models` rule: the check looks only at
   `upstream_model`, regardless of `model` shape.

No new top-level configuration sections.

---

## 9. Out of scope (explicit deferrals)

- **Full glob syntax** (`*` anywhere, `?`, character classes). Rejected
  during brainstorming as YAGNI; both motivating use cases are pure
  prefix matches.
- **Template / capture in `upstream_model`** (e.g. `${1}`). Rejected as
  YAGNI; pass-through and literal override cover the motivating cases.
- **Operator-visible prefix-route listing** in `/admin/catalog` or
  `show-catalog`. Deferred until concrete operator demand surfaces;
  `gateway.yaml` is the source of truth in the meantime.
- **Trie / radix-tree storage** for prefixes. Real-world per-frontend
  prefix count is tiny; a sorted `Vec` is simpler, faster to build,
  and zero new dependencies.

---

## 10. Acceptance checklist

- [ ] `RouteEntry.model` accepts prefix patterns (`literal*`) in
      addition to exact and `"*"`.
- [ ] `validate_routes` rejects `*` in any non-terminal position with a
      message naming a valid example.
- [ ] `StaticRouter::from_config` partitions routes into three buckets
      and sorts prefixes by length descending.
- [ ] `StaticRouter::resolve_route` performs exact → longest-prefix →
      wildcard lookup in that order.
- [ ] `upstream_model: "*"` under a prefix route passes the inbound
      model through verbatim, then receives fuzzy upgrade.
- [ ] `route_label` for a prefix-routed request uses the inbound model
      string (not the prefix).
- [ ] `list_routes` and all catalog surfaces exclude prefix routes
      (zero code change, structural).
- [ ] `validate_with_index` (strict_upstream_models) is unchanged;
      regression tests pin the literal-vs-pass-through behavior matrix.
- [ ] `RouteError` enum unchanged; `NoRoute` shape preserved on full miss.
- [ ] `docs/configuration.md` updated per §8.
- [ ] All four pass-criteria commands green.
