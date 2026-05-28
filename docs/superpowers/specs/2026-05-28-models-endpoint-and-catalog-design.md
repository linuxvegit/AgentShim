# `/v1/models` Endpoint and Model Catalog Enrichment — Design Spec

**Date:** 2026-05-28
**Status:** Draft (pending implementation plan)
**Scope:** Expose a rich `/v1/models` endpoint from AgentShim that
surfaces the configured routes alongside the live capability data
discovered from each upstream's own `/models` endpoint. Extends
`ModelIndex` to carry the metadata required to back this endpoint and to
sharpen routing decisions downstream.

## Motivation

AgentShim today is a black box from the client's perspective:

- There is no way for an upstream client (Claude Code, Codex, a custom
  agent, an IDE) to ask the gateway "what models can I send you?".
  OpenAI clients call `GET /v1/models`; Anthropic clients call no
  catalog endpoint (Anthropic does not publish one). With AgentShim
  fronting both dialects, the OpenAI `/v1/models` route is missing
  outright, and the Anthropic-side answer ("look at the gateway YAML")
  forces the user to read configuration files they may not have access
  to.

- `ModelIndex` discovers upstream model names but **throws away every
  other field** the upstream returns. Concrete evidence: Copilot's
  `/models` response (captured 2026-05-28) carries `family`, `billing`,
  `capabilities.limits.max_context_window_tokens`,
  `capabilities.supports.{vision, tool_calls, streaming, structured_outputs}`,
  `model_picker_enabled`, and `version`. AgentShim retains only the
  `id` string. The remaining metadata would let routing make
  context-size choices (`claude-opus-4.6` vs `claude-opus-4.6-1m`,
  `gpt-5.5` vs `gpt-5.2`), capability gating could verify upstream
  truly supports tool calls before forwarding them, and operators could
  see what is actually available without re-running `curl`.

- Operators have no human-readable catalog at startup or via admin to
  sanity-check that the route table makes sense against the live
  upstream catalog. A typo in `upstream_model: claude-opus-47` only
  shows up at request time, at which point fuzzy resolution either
  silently succeeds (matching `claude-opus-4.7` by accident) or fails
  with a generic upstream error.

This spec addresses all three: a new public `/v1/models` endpoint, a
richer `ModelIndex` to feed it, and a one-shot human-readable catalog
exposed via admin (and optionally printed at startup).

## Domain language additions

Three terms enter `CONTEXT.md`:

- **Model catalog** (`ModelCatalog`): The composed view of (a) the route
  table — which model aliases AgentShim itself accepts on which
  frontend, and (b) the upstream-discovered capabilities for each
  alias's `BackendTarget`. Built once per refresh cycle from the same
  inputs `ModelResolver` already consumes.

- **Model record** (`ModelRecord`): One entry in the catalog. Carries
  the alias (the name clients use), the resolved upstream provider +
  model id, and the upstream-reported capabilities. Distinct from
  `BackendTarget` (which is the routing decision output) and from raw
  upstream JSON (which is what `Provider::list_models` returns).

- **Model metadata** (`ModelMetadata`): The structured upstream
  capability blob — context size, output limits, supported features,
  family, optional billing tier hints. The same struct backs both
  `ModelRecord.metadata` and the enriched `ModelIndex` entries.

The existing terms **Frontend**, **Provider**, **Route**,
**BackendTarget**, **Router**, and **Model index** keep their meanings.
**Model index** is extended: previously a "set of upstream model name
strings keyed by provider", it becomes "a map of provider →
(upstream-model-id → `ModelMetadata`)".

## Public surface

### `GET /v1/models` (default port)

Returns the OpenAI-shape catalog of model aliases AgentShim accepts on
**any** frontend. Aliases are deduplicated by name (an alias that
appears on multiple frontends collapses to one record with a `frontends`
array).

```json
{
  "object": "list",
  "data": [
    {
      "id": "claude-opus-4-7",
      "object": "model",
      "owned_by": "agent-shim",
      "frontends": ["anthropic_messages"],
      "upstream": {
        "provider": "copilot",
        "model": "claude-opus-4.7"
      },
      "capabilities": {
        "context_window_tokens": 200000,
        "max_output_tokens": 32000,
        "supports": {
          "vision": true,
          "tool_calls": true,
          "streaming": true,
          "structured_outputs": false,
          "reasoning_effort": true
        },
        "family": "claude-opus-4.7"
      },
      "long_context_variant": "claude-opus-4-7-1m"
    },
    {
      "id": "gpt-5.5",
      "object": "model",
      "owned_by": "agent-shim",
      "frontends": ["openai_chat", "openai_responses"],
      "upstream": { "provider": "copilot", "model": "gpt-5.5" },
      "capabilities": {
        "context_window_tokens": 1050000,
        "max_output_tokens": 128000,
        "supports": { "vision": true, "tool_calls": true, ... },
        "family": "gpt-5.5"
      }
    }
  ]
}
```

Schema:

| Field | Type | Source | Required |
|---|---|---|---|
| `id` | string | Route alias (`routes[].model`) | yes |
| `object` | `"model"` | Constant | yes |
| `owned_by` | `"agent-shim"` | Constant | yes |
| `frontends` | `[string]` | Distinct `routes[].frontend` for this alias | yes |
| `upstream.provider` | string | `BackendTarget.provider` (chain head) | yes |
| `upstream.model` | string | `BackendTarget.model` (chain head, post-fuzzy) | yes |
| `capabilities` | object \| null | From `ModelIndex` metadata if present | nullable |
| `capabilities.context_window_tokens` | int | Upstream `max_context_window_tokens` | nullable |
| `capabilities.max_output_tokens` | int | Upstream `max_output_tokens` | nullable |
| `capabilities.supports.*` | bool | Upstream `supports.*` | nullable per key |
| `capabilities.family` | string | Upstream `family` | nullable |
| `long_context_variant` | string \| null | Same-family alias with larger context, if route table has one | nullable |
| `upstreams` | array | Only set when route is a multi-upstream fallback chain | optional |

For multi-upstream fallback routes (Plan 02 array form), `upstream`
remains the chain head and an additional `upstreams: [{provider,
model}, ...]` array surfaces the full chain.

Aliases that resolve via a `model: "*"` wildcard route are NOT returned
— the catalog enumerates explicit aliases only. A wildcard appears
once as a synthetic entry with `id: "*"` and a `wildcard: true` marker
so clients know "this gateway accepts arbitrary names on this
frontend".

**Error model**: `200 OK` with empty `data` when no routes are
configured. `503 Service Unavailable` only when AgentShim itself is
not ready (config-load failure path).

**Filtering query params (v1, optional):**
- `?frontend=anthropic_messages` — restrict to one frontend.
- `?capability=vision` — only models whose `capabilities.supports.vision`
  is `true`.

No pagination in v1 — catalogs are small (low tens of entries).

### `GET /admin/catalog` (admin port)

Returns the same data as `/v1/models` but in a structured form that
includes operator-only fields:

- Per-route resilience config (`retry`, `breaker`)
- Whether the upstream's `/models` discovery is **stale**
  (`discovery.last_refresh_unix`, `discovery.status: ok | stale | failed`)
- `reasoning_mapping` table contents (the table from the effort spec —
  shows operators what rewrites are active)
- Routes that have no upstream metadata yet (discovery hasn't run or
  the provider doesn't support `list_models`) — flagged so operators
  notice silent fallbacks

Distinct endpoint because (a) admin-only fields don't belong on the
public surface, and (b) the admin endpoint is on a different listener
(`admin_addr`) with separate auth.

### `agent-shim show-catalog` CLI subcommand

Reads the configured gateway YAML, performs one round of upstream
discovery, and prints a human-readable table to stdout. Useful for
operators sanity-checking a config before starting the server. Same
data as `/admin/catalog` but rendered as a table:

```
Frontend                Alias                Provider    Upstream model              Family               Ctx     Out     Vision  Tools
─────────────────────── ──────────────────── ─────────── ─────────────────────────── ──────────────────── ─────── ─────── ─────── ───────
anthropic_messages      claude-opus-4-7      copilot     claude-opus-4.7             claude-opus-4.7      200K    32K     ✓       ✓
anthropic_messages      claude-opus-4-7-1m   copilot     claude-opus-4.6-1m          claude-opus-4.6-1m   1M      64K     ✓       ✓
openai_chat             gpt-5.5              copilot     gpt-5.5                     gpt-5.5              1.05M   128K    ✓       ✓
openai_chat             deepseek-reasoner    deepseek    deepseek-reasoner           ?                    ?       ?       ?       ?  (no metadata: provider lacks /models)
```

Exit non-zero if any route references an alias the upstream catalog
doesn't contain (typo detection).

## Data structures

### `agent-shim-core::catalog` (new module)

```rust
use std::collections::BTreeMap;
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub context_window_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub family: Option<String>,
    #[serde(default)]
    pub supports: ModelSupports,
    /// Upstream version string, if reported.
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelSupports {
    pub vision: Option<bool>,
    pub tool_calls: Option<bool>,
    pub streaming: Option<bool>,
    pub structured_outputs: Option<bool>,
    pub reasoning_effort: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRecord {
    pub id: String,                              // route alias
    pub frontends: Vec<crate::FrontendKind>,
    pub upstream_provider: String,
    pub upstream_model: String,
    pub upstreams_chain: Vec<UpstreamRef>,       // length 1 for singular
    pub metadata: Option<ModelMetadata>,
    pub long_context_variant: Option<String>,    // sibling alias on same family with bigger ctx
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpstreamRef {
    pub provider: String,
    pub model: String,
}

pub type ModelCatalog = Vec<ModelRecord>;
```

`ModelMetadata` fields are all `Option<>` because not every provider
returns every field. The struct is intentionally narrow — only
operator/client-visible facts, not the raw JSON blob. Provider-specific
quirks (Copilot's `billing.token_prices`, Gemini's safety settings)
stay inside the provider crate.

### `agent-shim-router::model_index` (extended)

Replace the `BTreeSet<String>` payload with `BTreeMap<String,
ModelMetadata>`:

```rust
pub struct ModelIndex {
    /// provider name → (upstream model id → metadata)
    providers: HashMap<String, BTreeMap<String, ModelMetadata>>,
    /// Same tokenized fuzzy-match precomputation as today.
    fuzzy_entries: HashMap<String, Vec<ModelEntry>>,
}

impl ModelIndex {
    /// Existing API: name-only fuzzy resolution. Unchanged.
    pub fn resolve(&self, provider: &str, requested: &str) -> Option<&str> { ... }

    /// New: read upstream metadata for a (provider, exact upstream id).
    pub fn metadata(&self, provider: &str, model: &str) -> Option<&ModelMetadata> { ... }

    /// New: enumerate all (model id, metadata) for a provider.
    pub fn provider_models(&self, provider: &str) -> impl Iterator<Item = (&str, &ModelMetadata)> { ... }
}
```

Existing `ModelIndex::new(HashMap<String, BTreeSet<String>>)` is kept
as a shim (`pub fn from_ids(...)`) for tests that only need names; the
new primary constructor takes the metadata-bearing shape.

### `BackendProvider::list_models` — signature change

```rust
// Current
async fn list_models(&self) -> Result<Option<BTreeSet<String>>, ProviderError>;

// New
async fn list_models(&self)
    -> Result<Option<BTreeMap<String, ModelMetadata>>, ProviderError>;
```

`Option<>` semantics unchanged: `None` = "provider doesn't expose a
catalog" (e.g. raw Ollama deployments); `Some(map)` = "here are the
models I see and their attributes". Providers that lack capability data
return entries with `ModelMetadata::default()`.

## Provider-side changes

### `providers/github_copilot/models.rs` — full parse

Today's `list_models` extracts only `id`. Rewrite to populate the full
`ModelMetadata`:

```rust
// Mapping from Copilot /models response to ModelMetadata:
//
//   capabilities.limits.max_context_window_tokens  → context_window_tokens
//   capabilities.limits.max_output_tokens          → max_output_tokens
//   capabilities.family                            → family
//   capabilities.supports.vision                   → supports.vision
//   capabilities.supports.tool_calls               → supports.tool_calls
//   capabilities.supports.streaming                → supports.streaming
//   capabilities.supports.structured_outputs       → supports.structured_outputs
//   capabilities.supports.thinking                 → supports.reasoning_effort
//   version                                        → version
```

Captured 2026-05-28 fields from Copilot Enterprise tenant include all of
the above; rows in the data array that omit a field deserialise to
`None` cleanly.

The function also writes a structured log line for each discovered
model so `agent-shim discover` and the startup catalog print can show
"discovered 41 models from copilot, 12 wired to routes".

### `providers/openai_compatible::list_models`

Same signature change. Implementation reads any `data[*].id` and emits
`ModelMetadata::default()` for each (OpenAI's `/v1/models` does not
return capability data). No-op for upstreams that 404 the endpoint.

### `providers/deepseek`, `providers/gemini`, `providers/anthropic`

Implement `list_models` returning `None` for now — they don't have a
useful enumeration. (Anthropic in particular has no public catalog
endpoint.) Tracked as a future task per provider.

## Router-side changes

### `ModelResolver::list_catalog`

New method on `ModelResolver` that composes a `ModelCatalog`:

```rust
impl ModelResolver {
    pub fn list_catalog(&self) -> ModelCatalog {
        // 1. Walk the static route table (all explicit aliases).
        // 2. For each alias, resolve via existing `resolve(frontend, alias)`.
        // 3. Look up metadata for chain head via `model_index.metadata(...)`.
        // 4. Compute `long_context_variant` by scanning aliases on the
        //    same family with strictly larger context_window_tokens.
        // 5. Group by alias to collapse same-id-on-multiple-frontends.
    }
}
```

Pure function over the router state plus the model index — no I/O.
Cheap enough to call per request to `/v1/models`; cached on
`AppState` only if profiling shows hot-path overhead.

### `StaticRouter` — typo detection

`StaticRouter::from_config` already tolerates routes that reference
unknown upstreams. Add a post-construction validation pass that, given
a populated `ModelIndex`, warns (not errors) when
`routes[].upstream_model` is not found in the index — gated by a new
config flag `validation.strict_upstream_models: false` (default false)
so existing configs don't break. Set to `true` makes the warning a
hard error at startup.

Surfaced via the same admin reload pipeline — a stale catalog won't
fail reloads, but will populate the catalog's `discovery.status:
stale`.

## Gateway-side changes

### `crates/gateway/src/handlers/models.rs` (new)

`pub async fn handle(State(state): State<AppState>, query: Query<...>) -> Response`.

Reads the catalog via `state.resolver.list_catalog()`, filters by query
params, serialises as the OpenAI-shape JSON in the **Public surface**
section. Cache-friendly: `Cache-Control: private, max-age=60`.

### `crates/gateway/src/server.rs` — route registration

Add to the public router:

```rust
.route("/v1/models", get(handlers::models::handle))
.route("/v1/models/:id", get(handlers::models::get_one))   // OpenAI parity
```

### `crates/gateway/src/admin/handlers.rs` — `/admin/catalog`

Same data, augmented with admin-only fields (resilience config,
discovery freshness, reasoning_mapping contents).

### `crates/gateway/src/cli.rs` — `show-catalog` subcommand

```
agent-shim show-catalog --config config/gateway.yaml [--format table|json] [--strict]
```

Performs one synchronous round of model discovery (calls each
provider's `list_models`), builds the catalog, prints. `--strict` exits
non-zero if any alias has no upstream metadata or fails the typo check.

### `crates/gateway/src/main.rs` — optional startup print

A new config field `logging.print_catalog_on_start: false` (default
false) causes the `serve` command to print the catalog to stdout once
right after model discovery completes. Operators who want it can
opt in; CI/automation use cases default to silent.

## Discovery lifecycle

`ModelIndex` is currently populated **once at startup** from each
provider's `list_models`. This stays — the catalog is not real-time.
Refreshing happens on:

1. Server startup (existing).
2. SIGHUP / `POST /admin/reload` (existing — config-reload path
   re-runs discovery for newly-added upstreams).
3. New explicit `POST /admin/discover` endpoint that re-runs discovery
   without otherwise touching config. v1 includes this so operators
   can react to upstream model additions without bouncing the gateway.

Each `ModelIndex` carries a `last_refresh: Instant` and per-provider
`discovery_status: Ok | Stale(reason) | Failed(reason)`. The catalog
surfaces this on `/admin/catalog` but not on `/v1/models`.

## Configuration changes

`config::schema::GatewayConfig`:

```yaml
logging:
  # existing fields...
  print_catalog_on_start: false   # NEW (default false)

validation:                       # NEW section
  strict_upstream_models: false   # NEW (default false); true makes catalog typos fatal
```

`validation` is a new top-level section because there will likely be
more validation toggles to put there (we have a few latent ones
scattered today). `deny_unknown_fields` requires we add the section
even if it stays empty for users who don't set it.

## Testing matrix

| Case | Where |
|---|---|
| `ModelMetadata` (de)serialise round-trips | `core::catalog` |
| Copilot `/models` JSON → full `ModelMetadata` (vision/tools/limits/family) | `providers::github_copilot::models` |
| Copilot `/models` 404 path returns `None` | `providers::github_copilot::models` (unchanged) |
| Provider without `list_models` returns `None` (deepseek/gemini/anthropic) | `providers::*` |
| `ModelIndex::metadata` reads back what was written | `router::model_index` |
| `ModelIndex::from_ids` shim path | `router::model_index` |
| `ModelResolver::list_catalog` groups same alias across multiple frontends | `router::resolver` |
| `ModelResolver::list_catalog` populates `long_context_variant` when configured | `router::resolver` |
| `ModelResolver::list_catalog` wildcards: only one synthetic entry per frontend | `router::resolver` |
| `GET /v1/models` returns valid OpenAI-shape JSON | `gateway::handlers::models` |
| `GET /v1/models?frontend=...` filter | same |
| `GET /v1/models?capability=vision` filter | same |
| `GET /v1/models/:id` returns 404 for unknown alias | same |
| `GET /admin/catalog` includes `retry`/`breaker`/`discovery_status` | `gateway::admin::handlers` |
| `agent-shim show-catalog --format json` matches `/admin/catalog` | `gateway::cli` |
| `show-catalog --strict` exits non-zero on typo route | `gateway::cli` |
| `validation.strict_upstream_models: true` rejects bad config at load | `config::validation` |
| Startup print emits when `logging.print_catalog_on_start: true` | `gateway::main` |

## Out of scope (deferred)

- **Real-time model discovery refresh**: `/models` upstream calls
  remain startup-time + on-demand. Continuous polling has no current
  driver — Copilot model lists change rarely (~weeks).
- **OpenAI Responses-style "endpoints" array**: `data[*].endpoints`
  was a 2024 OpenAI experiment, hasn't shipped widely. Omit.
- **Anthropic `/v1/models` parity**: Anthropic does not document a
  public catalog, and Claude clients don't ask for one. AgentShim
  exposes the OpenAI-shape endpoint only; Anthropic users either look
  at the YAML or hit the new endpoint regardless of frontend dialect.
- **Per-model pricing surface**: Copilot's `/models` includes
  `billing.token_prices`; we read it for ourselves (already on the
  Phase 6 cost path) but don't echo it on the public surface. Operators
  can read it via `/admin/catalog`.
- **Catalog plugin hook**: a hook that lets plugins mutate the catalog
  before it's returned (e.g. to hide aliases by tenant). Probably
  natural in Phase 7+ — out of scope here.
- **Auto-route generation from `/models`**: tempting (turn every
  Copilot model into a route automatically) but explicit routes are a
  feature, not a chore — they make the access control story tractable.

## Implementation order

1. Add `agent-shim-core::catalog` module with `ModelMetadata`,
   `ModelSupports`, `ModelRecord`, `UpstreamRef`, `ModelCatalog`. Unit
   tests for serialisation.
2. Change `BackendProvider::list_models` signature in
   `crates/providers/src/lib.rs`. Update all impls to return
   `Ok(None)` initially. Fix tests that exercise the signature.
3. Rewrite `providers/github_copilot/models.rs` to parse the full
   capability blob into `ModelMetadata`. Test against the
   `Q:\Downloads\models.saz` capture.
4. Update `providers/openai_compatible::list_models` to the new
   signature (metadata defaults).
5. Extend `ModelIndex` to store `ModelMetadata`. Keep `from_ids` shim
   for back-compat tests. Update `ModelResolver` construction sites.
6. Add `ModelResolver::list_catalog`. Add `long_context_variant`
   sibling lookup.
7. Add `crates/gateway/src/handlers/models.rs` and wire to public
   router.
8. Add `/admin/catalog` to admin router.
9. Add `POST /admin/discover` (refresh without config reload).
10. Add `agent-shim show-catalog` CLI subcommand.
11. Add `validation.strict_upstream_models` config flag and post-load
    validation pass.
12. Add `logging.print_catalog_on_start` and main.rs startup hook.
13. Update README.md, CONTEXT.md, docs/configuration.md,
    `config/gateway.example.yaml`.

## Risks and trade-offs

- **Breaking trait signature change.** Existing `BackendProvider`
  impls in third-party code (if any) need to update their
  `list_models` return type. Mitigation: it's an internal crate trait
  with no external users on record; downstream impactors are all
  in-repo.

- **Catalog freshness vs accuracy.** A model added to Copilot after
  AgentShim startup won't show on `/v1/models` until a discover trigger
  fires. Mitigation: `/admin/discover` exists; documented in admin
  guide. Same risk as today's fuzzy resolution — not a regression.

- **Frontend grouping in `ModelRecord.frontends`.** If two routes
  with the same alias point to different upstreams (rare but legal —
  same name on Anthropic frontend vs OpenAI frontend can mean different
  backends), we surface only the chain head of the first one in
  `upstream`. The full picture lives in `/admin/catalog`. We accept
  the trade-off because clients of `/v1/models` care about "what can I
  send", not "how is it routed".

- **OpenAI `/v1/models/:id`** parity expectations: callers may
  expect 404 on unknown aliases but 200 + minimal JSON on known
  aliases. The wildcard-route case is awkward — `:id` could match a
  wildcard route, but a wildcard says "any model goes" rather than
  "this specific model is known to exist". Decision: `:id` against a
  pure-wildcard frontend returns 404 unless the alias also appears in
  the model index's discovered set for the wildcard's provider.
  Documented in handler.

- **Provider-side enrichment cost.** Parsing Copilot's full `/models`
  blob is a few hundred microseconds more than today's `id`-only path.
  Negligible at startup; not on any hot request path.
