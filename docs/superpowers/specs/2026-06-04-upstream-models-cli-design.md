# `agent-shim models <name>` — per-upstream model discovery CLI

**Status:** Approved for planning.
**Date:** 2026-06-04.
**Scope:** new `models` subcommand plus the per-provider `list_models()`
implementations needed to make it work for every upstream type.

## 1. Problem

`agent-shim copilot models` prints the Copilot catalog as a fixed-width
table. Operators running other upstreams (GLM, DeepSeek, Anthropic,
Gemini, generic OpenAI-compatible) have no equivalent CLI — they have to
either start the gateway and curl `/v1/models`, or shell out to each
provider's own API. We want one command that works for every upstream
configured in `gateway.yaml`:

```
agent-shim models <upstream-name> [--config <path>] [--format table|json]
```

`<upstream-name>` is the key under `cfg.upstreams` (not the provider
type), so configs with multiple upstreams of the same type
(`glm-prod` and `glm-staging`) are unambiguous.

## 2. Non-Goals

- We do not deprecate or remove `agent-shim copilot models`. It keeps
  its current behaviour: read the device-flow credential, hit Copilot's
  `/models` directly, never touch `config/gateway.yaml`. The new command
  is additive.
- We do not add new model-discovery shapes (no caching, no diff against
  the route table, no `--strict`). `show-catalog` already covers the
  cross-upstream catalog view; this command is the single-upstream
  counterpart.
- We do not change the canonical `ModelMetadata` shape or the
  `BackendProvider::list_models()` trait signature.

## 3. CLI surface

```
agent-shim models <name>
    [--config <path>]    # default: same as `serve` — config/gateway.yaml,
                         #          env AGENT_SHIM_CONFIG
    [--format <fmt>]     # default: table; alternative: json
```

`<name>` is a positional argument. clap rejects unknown `--format`
values with a helpful message (matching the existing `copilot models`
error wording: `unknown --format {other:?}; expected `table` or `json``).

The existing CLI surface listed in `CLAUDE.md` gains one line:

```
models <name> [--format json|table]
```

## 4. Execution path

```
agent-shim models <name>
    │
    ├─ agent_shim_config::load_from_path(config)
    ├─ agent_shim_config::validate(&cfg)
    ├─ AppState::new(cfg).await
    │     └─ builds ProviderRegistry from cfg.upstreams (same code path
    │        as serve / show-catalog — zero divergence)
    ├─ state.core.providers.get(name)
    │     None → error:
    │       "no upstream named '{name}' in config (available: a, b, c)"
    │       plus, when name == "copilot":
    │       "  (hint: `agent-shim copilot models` uses the device-flow
    │        credential and does not require a config entry)"
    ├─ provider.list_models().await
    │     Err(e) → bubble up via anyhow
    │     Ok(None) → error:
    │       "upstream '{name}' (provider kind '{kind}') does not support
    │        model discovery"
    │       — {kind} is the UpstreamConfig variant tag, looked up from
    │       cfg.upstreams[name] (e.g. "glm", "github_copilot"). After
    │       the follow-up work in §6 lands, this branch is unreachable
    │       for every shipped UpstreamConfig variant; it stays as a
    │       safety net for future variants that haven't implemented
    │       list_models() yet.
    │     Ok(Some(map)) → render
    └─ render_models::render(&map, format, &header)
```

`AppState::new` does run discovery for every upstream during
construction — that work is wasted for this one-shot CLI but the cost
is bounded by the upstream discovery timeouts already in effect, and
the alternative (a parallel match-on-`UpstreamConfig` factory) doubles
the number of places that must learn about a new upstream type. We
accept the wasted discovery as the price of one source of truth.

## 5. Rendering — shared module

`crates/gateway/src/commands/copilot_models.rs` already owns the
fixed-width table renderer for `BTreeMap<String, ModelMetadata>`. We
lift it into a new sibling module so both commands share it:

```
crates/gateway/src/commands/render_models.rs   ← new
    pub fn render(models: &BTreeMap<String, ModelMetadata>,
                  format: &str,
                  header: &RenderHeader) -> anyhow::Result<()>
    pub struct RenderHeader {
        pub line1: String,   // e.g. "Copilot API base: https://..."
                             //  or  "Upstream 'glm-prod' (type: glm)"
    }
    fn render_json(...)
    fn render_table(header, ...)
    fn format_effort(m: &ModelMetadata) -> String
    fn format_thinking(m: &ModelMetadata) -> String
    fn bool_label(v: Option<bool>) -> String
    fn short_num(n: u32) -> String
```

The header is the only line that differs between the two callers, so it
becomes the only parameter. Everything else — column widths, separator
length, effort / thinking compaction rules, numeric formatting — is
identical.

`copilot_models.rs` shrinks to: load credential → exchange token → call
`github_copilot::models::list_models()` → call
`render_models::render(&map, format, RenderHeader { line1: format!("Copilot API base: {}", token.api_base) })`.

The six existing unit tests in `copilot_models.rs`
(`format_effort_compacts_long_values`,
`format_effort_handles_empty_list_via_bool_fallback`,
`format_effort_em_dash_when_unknown`,
`format_thinking_combines_adaptive_and_range`,
`format_thinking_range_only_when_no_adaptive_flag`,
`format_thinking_em_dash_when_truly_absent`,
`short_num_handles_thresholds`) move to `render_models.rs` unchanged
— they exercise helpers that move with them.

## 6. Provider work — fill in `list_models()`

Four providers currently use the default trait impl that returns
`Ok(None)`. We add real implementations so the new CLI works for every
upstream type without case-by-case errors.

All four follow the same pattern that GLM and DeepSeek already use:
`GET {base_url}/<models-path>`, deserialise the response, extract IDs
into a `BTreeMap<String, ModelMetadata::default()>`, return `Ok(None)`
on any non-2xx so the CLI surfaces a clean "does not support discovery"
message rather than leaking a raw HTTP error.

| Provider             | Path                              | Auth                          | Response shape         |
|----------------------|-----------------------------------|-------------------------------|------------------------|
| `anthropic`          | `GET {base_url}/v1/models`        | `x-api-key: {api_key}` + `anthropic-version` header | `{ data: [{ id, ... }] }` |
| `gemini`             | `GET {base_url}/v1beta/models?key={api_key}` | query string | `{ models: [{ name: "models/gemini-…", ... }] }` — strip `models/` prefix |
| `openai_compatible`  | `GET {base_url}/models`           | `Authorization: Bearer {api_key}` | `{ data: [{ id, ... }] }` |
| `github_copilot` (cfg path) | reuses `github_copilot::models::list_models()` | reuses the existing device-flow credential via `CopilotTokenManager` (the cfg has no api_key — Copilot upstreams authenticate from the on-disk credential, same as today) | full capability map (Copilot already returns rich metadata) |

The Copilot config-path implementation reuses
`CopilotTokenManager::get()` to obtain a fresh token, then calls
`github_copilot::models::list_models(&http, &token)` — the same
function the device-flow `copilot models` command uses today. The cfg
side does not introduce a second auth mechanism; the credential still
lives on disk and `agent-shim copilot login` is still how it gets
written. The new code path is purely a `BackendProvider` adapter that
gives `agent-shim models <copilot-upstream-name>` something to call.

The Copilot path keeps the rich `ModelMetadata` (family, context
window, reasoning effort values, thinking budget) because that's what
`models::parse_models_response` already returns. The other three
providers populate only `id` for now; richer metadata extraction is
intentionally deferred (`ModelMetadata::default()` is a valid
renderable shape — the `format_*` helpers already render `"—"` / `"?"`
for absent fields).

Each new `list_models()` ships with mockito coverage:
- 200 success → returns the expected map
- 404 → returns `Ok(None)`
- 401 / 5xx → returns `Ok(None)` (consistent with GLM/DeepSeek precedent)

## 7. Wiring

```
crates/gateway/src/cli.rs
    Commands::Models {
        name: String,
        #[arg(short, long, env = "AGENT_SHIM_CONFIG", default_value = "config/gateway.yaml")]
        config: PathBuf,
        #[arg(long, default_value = "table")]
        format: String,
    }

crates/gateway/src/main.rs
    Commands::Models { name, config, format } =>
        commands::models::run(&name, &config, &format).await,

crates/gateway/src/commands/mod.rs
    pub mod models;
    pub mod render_models;
```

## 8. Error wording (exact strings)

```
no upstream named 'NAME' in config (available: a, b, c)
no upstream named 'copilot' in config (available: a, b, c)
  (hint: `agent-shim copilot models` uses the device-flow credential
   and does not require a config entry)
upstream 'NAME' (provider kind 'KIND') does not support model discovery
unknown --format "FMT"; expected `table` or `json`
```

The "available" list is sorted alphabetically and comma-separated. The
hint line is shown only when the missing name is exactly `copilot`.
The "provider kind" is the `UpstreamConfig` variant tag (`glm`,
`deepseek`, `anthropic`, `gemini`, `openai_compatible`,
`github_copilot`), not the result of `provider.name()` — the latter
returns the upstream key for most providers and `"github_copilot"` for
Copilot, which would be confusing when the user already supplied the
upstream key.

## 9. Test plan

- `render_models.rs` — the six migrated unit tests, plus one new test
  that renders a generic (non-Copilot) header line and asserts the
  table body row matches the existing Copilot output.
- `anthropic::list_models` / `gemini::list_models` /
  `openai_compatible::list_models` / `github_copilot::list_models`
  (cfg path) — mockito success / 404 / 401 cases per provider.
- CLI integration test (`assert_cmd` + mockito server in a
  `tokio::spawn`):
  - `agent-shim models nonexistent` → exit 1, error contains
    `no upstream named 'nonexistent'`.
  - `agent-shim models copilot` with empty config → exit 1, error
    contains the device-flow hint.
  - `agent-shim models my-glm --format json` against a mockito-backed
    glm upstream → exit 0, stdout parses as JSON object with the
    expected model IDs.
  - `agent-shim models my-glm` → exit 0, stdout contains the table
    header `Model ID` and the expected model IDs.
- Regression: existing `copilot_models` integration test (if any)
  continues to pass byte-for-byte — the renderer move is meant to be
  invisible.

## 10. Out of scope (follow-ups, not blockers)

- Rich capability extraction for anthropic / gemini /
  openai_compatible. These providers return varying levels of metadata;
  doing them properly is its own design.
- Caching of `list_models()` responses. The CLI runs once and exits;
  caching only matters when the gateway is serving traffic, which is
  already handled by `ModelIndex`.
- A `models --all` flag that loops every upstream. `show-catalog`
  already covers that view.
