# Fuzzy Model Discovery

Automatically discover available models from upstream providers at startup and use token-based fuzzy matching to resolve agent-requested model names to the closest supported model.

## Problem

Agents request models by name (e.g. `claude-sonnet-4-5`), but upstream providers may list them differently (e.g. `claude-sonnet-4-5-20250514`). Today, the gateway either requires explicit per-model route entries or blindly passes the name through with a wildcard route. A mismatch means a failed request with no useful feedback.

## Solution

1. At startup, call each provider's model discovery endpoint to get available models.
2. Build a `ModelIndex` that tokenizes and indexes these model names.
3. After static route resolution, fuzzy-match the resolved `upstream_model` against the provider's discovered models.
4. If a good match is found, substitute it. Otherwise, pass through unchanged.

## Provider Trait Extension

Add an optional method to `BackendProvider`:

```rust
async fn list_models(&self) -> Result<Option<BTreeSet<String>>, ProviderError>;
```

Default implementation returns `Ok(None)`. Providers that support discovery implement it:

- **CopilotProvider**: already has `list_models()` in `models.rs` — wire it to the trait.
- **OpenAiCompatibleProvider**: call `GET {base_url}/models`, extract model IDs from the standard OpenAI list response.

## ModelIndex (new module in router crate)

```rust
pub struct ModelIndex {
    providers: HashMap<String, Vec<ModelEntry>>,
}

struct ModelEntry {
    original: String,       // original casing from provider
    normalized: String,     // lowercase
    tokens: Vec<String>,    // split on '-', '_', '.'
}

impl ModelIndex {
    pub fn new(discovered: HashMap<String, BTreeSet<String>>) -> Self;
    pub fn resolve(&self, provider: &str, requested: &str) -> Option<&str>;
}
```

## Token-Based Scoring Algorithm

1. **Exact match** (case-insensitive) → return immediately, score 1.0.
2. **Prefix match** → if one name is a prefix of the other, boost score. Handles `claude-sonnet-4-5` → `claude-sonnet-4-5-20250514`.
3. **Token overlap** → split both names into tokens. Score = weighted matching tokens / max weighted tokens. Earlier tokens (vendor, family) weighted higher than trailing tokens (date stamps, variant suffixes).
4. **Threshold** → if best score < 0.4, return `None` (pass through unchanged).
5. **Tie-breaking** → prefer shorter name (canonical ID), then alphabetical.
6. **Casing** → normalize to lowercase for comparison, return original casing from discovered list.

## Startup Sequence

In `serve.rs`, after building providers and static router:

1. Iterate all registered providers, call `list_models()` on each.
2. Collect results into `HashMap<String, BTreeSet<String>>` (provider name → models).
3. Providers that return `Ok(None)` or `Err(...)` are skipped. Errors log a warning but do not prevent startup.
4. Build `ModelIndex::new(discovered)`.
5. Store in `AppState`.

## Request-Time Flow

1. Decode request → `CanonicalRequest` (unchanged).
2. `router.resolve(frontend, model_alias)` → `BackendTarget` (unchanged).
3. `model_index.resolve(&target.provider, &target.model)` → `Option<&str>`.
4. If `Some(matched)`, substitute `target.model`. If `None`, keep original.
5. `provider.complete(req, target)` → stream (unchanged).

When fuzzy matching substitutes a model, log at `info`: `"fuzzy model match: '{requested}' → '{resolved}' (provider: {provider}, score: {score})"`.

## Edge Cases

- **Discovery failure**: log warning, skip provider, no fuzzy matching for it.
- **Empty model list**: treated as no discovery — passthrough only.
- **Explicit upstream_model in route**: if a route pins `upstream_model: "gpt-4o"` (not `"*"`), the model name is already set and fuzzy matching still applies (resolves against discovered models).

## Scope

- Applies to any provider that implements `list_models()`, not just Copilot.
- Fetch once at startup. No periodic refresh — restart to pick up new models.
- No config changes required. Existing wildcard routes get smarter automatically.

## Testing

**Unit tests (router crate):**
- Tokenizer: various model name formats.
- Scoring: exact match = 1.0, prefix match scores high, unrelated models score low, below-threshold returns `None`.
- `ModelIndex::resolve`: exact hit, fuzzy hit, no match passthrough, empty index.

**Integration tests (protocol-tests crate):**
- Realistic Copilot model list, verify resolution for common agent model names.

**Property tests (proptest):**
- Exact match always wins (if requested name is in discovered set, it must be returned as-is).
