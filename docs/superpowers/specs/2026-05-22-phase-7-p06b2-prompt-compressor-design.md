# Phase 7 P06b2: `prompt_compressor` Built-in Plugin Design

**Status:** Draft
**Date:** 2026-05-22
**Owner:** AgentShim core
**Phase:** 7 P06b2 (second of three P06b sibling sub-projects after P06b1)

---

## 1. Goal

Ship `prompt_compressor` as the third built-in plugin in `agent-shim-plugins`. It runs in H2 (`on_decoded_request`) and modifies **only** `req.messages`, shortening conversation history according to an operator-configured strategy to avoid upstream context-window overflow or to control prompt token cost.

`prompt_compressor` is the second of three sibling P06b sub-projects originally deferred from P06a (P06b1 = `pii_scrubber` already landed; P06b3 = Prometheus sink for `usage_recorder` lands separately).

## 2. Scope

### 2.1 In-scope

- Three strategies behind one `#[serde(tag = "type")]` enum:
  - **`drop_old_turns`** — delete oldest user-led groups, keep the trailing `keep_last_n` groups.
  - **`truncate_to_tokens`** — delete oldest groups one-by-one until total messages token count <= `target_tokens`, subject to the trailing `keep_last_n` floor.
  - **`summarize_old_turns`** — replace the dropped groups with a single user-role message produced by calling a configured upstream provider; fall back to `drop_old_turns` behavior on any failure.
- Extend `PluginFactory::instantiate` signature to accept `FactoryDependencies { providers }` so factories can validate upstream references at startup and stash an `Arc<dyn BackendProvider>` for runtime use.
- Migrate the two existing built-in factories (`pii_scrubber`, `usage_recorder`) to the new signature (one extra `_deps: &FactoryDependencies` parameter each).
- Three new declarative metrics: `actions_total`, `messages_dropped_total`, `summary_duration_seconds`.
- Plugin-crate unit tests (~31) + one gateway integration test (mockito for both `main` and `summarizer` upstreams).
- New Cargo feature `prompt_compressor` (default-on) on the `agent-shim-plugins` crate; brings in `agent-shim-tokens` as a dependency.

### 2.2 Out-of-scope (deferred)

- Operator-supplied summary prompt template / placeholder rendering (deferred until a real request appears).
- Multiple summarizer providers per plugin instance (A/B selection).
- Mutation of `req.system` or `req.tools` (operator's hard contract).
- Per-route override of compressor config (route-level inheritance is P06c/P07 territory).
- Recursive compression of an over-budget summary message (warn-log only; no second pass).
- Non-English summarizer prompts (English-only v1).

### 2.3 Frozen-core invariant

No changes to `crates/core/`. Acceptance check: `git diff master..HEAD -- crates/core/` must be empty.

---

## 3. YAML schema (operator-facing)

### 3.1 `drop_old_turns`

```yaml
plugins:
  compressor:
    type: prompt_compressor
    on_error: skip
    config:
      strategy:
        type: drop_old_turns
        keep_last_n: 4
```

### 3.2 `truncate_to_tokens`

```yaml
plugins:
  compressor:
    type: prompt_compressor
    config:
      strategy:
        type: truncate_to_tokens
        target_tokens: 8192
        keep_last_n: 2     # hard floor even if total tokens still > target
```

### 3.3 `summarize_old_turns`

```yaml
plugins:
  compressor:
    type: prompt_compressor
    config:
      strategy:
        type: summarize_old_turns
        keep_last_n: 2
        summarizer:
          upstream: cheap-haiku    # MUST match an entry under top-level `upstreams:`
          model: claude-haiku-4-5  # backend_model string passed through to provider
          max_summary_tokens: 300  # also sets generation.max_tokens on the summary request
          timeout_ms: 5000         # total summarizer call deadline
```

---

## 4. Rust types

### 4.1 Config layer (`crates/plugins/src/builtin/prompt_compressor/config.rs`)

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptCompressorConfig {
    pub strategy: Strategy,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Strategy {
    DropOldTurns(DropOldTurnsConfig),
    TruncateToTokens(TruncateToTokensConfig),
    SummarizeOldTurns(SummarizeOldTurnsConfig),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DropOldTurnsConfig {
    pub keep_last_n: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TruncateToTokensConfig {
    pub target_tokens: u32,
    pub keep_last_n: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummarizeOldTurnsConfig {
    pub keep_last_n: usize,
    pub summarizer: SummarizerConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummarizerConfig {
    pub upstream: String,
    pub model: String,
    #[serde(default = "default_max_summary_tokens")]
    pub max_summary_tokens: u32,
    #[serde(default = "default_summary_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_max_summary_tokens() -> u32 { 300 }
fn default_summary_timeout_ms() -> u64 { 5000 }
```

### 4.2 Compiled internal state (`crates/plugins/src/builtin/prompt_compressor/mod.rs`)

```rust
pub struct PromptCompressor {
    strategy: CompiledStrategy,
}

enum CompiledStrategy {
    DropOldTurns {
        keep_last_n: usize,
    },
    TruncateToTokens {
        target_tokens: u32,
        keep_last_n: usize,
    },
    SummarizeOldTurns {
        keep_last_n: usize,
        provider: Arc<dyn BackendProvider>,
        target: BackendTarget,
        max_summary_tokens: u32,
        timeout: Duration,
    },
}
```

**Why split compiled state from raw config?** Raw config holds a `String` upstream name; compiled state holds an `Arc<dyn BackendProvider>`. The lookup happens once at startup; every H2 invocation thereafter is a direct method call with zero HashMap traversal — same pattern as `pii_scrubber`'s `CompiledRule`.

### 4.3 Config-layer validation

Run inside `PromptCompressorFactory::instantiate(...)` after `serde_json::from_value`. Any failure returns `PluginConfigError::InvalidValue`:

| Failure | Field | Reason |
|---|---|---|
| `keep_last_n == 0` (any strategy) | `strategy.keep_last_n` | `must be >= 1` |
| `target_tokens == 0` | `strategy.target_tokens` | `must be >= 1` |
| `summarizer.max_summary_tokens == 0` | `strategy.summarizer.max_summary_tokens` | `must be >= 1` |
| `summarizer.timeout_ms == 0` | `strategy.summarizer.timeout_ms` | `must be >= 1` |
| `summarizer.upstream` not in `deps.providers` | `strategy.summarizer.upstream` | ``unknown upstream `xxx`; known: [...]`` |

The unknown-upstream check is the load-bearing reason for upgrading the factory signature: typos in YAML fail at gateway startup instead of on the first H2 invocation.

---

## 5. Factory signature upgrade

### 5.1 Trait change

```rust
// crates/plugins/src/trait_def.rs
pub trait PluginFactory: Send + Sync + 'static {
    fn kind_name(&self) -> &'static str;
    fn instantiate(
        &self,
        plugin_name: &str,
        config: serde_json::Value,
        deps: &FactoryDependencies,   // NEW
    ) -> Result<Box<dyn Plugin>, PluginConfigError>;
}

pub struct FactoryDependencies<'a> {
    pub providers: &'a HashMap<String, Arc<dyn BackendProvider>>,
}
```

### 5.2 Callsite migration

- `crates/plugins/src/builtin/pii_scrubber.rs::PiiScrubberFactory::instantiate` — add `_deps: &FactoryDependencies` parameter; body unchanged.
- `crates/plugins/src/builtin/usage_recorder.rs::UsageRecorderFactory::instantiate` — same.
- `crates/plugins/src/registry.rs::PluginRegistry::build(...)` — accept `deps: FactoryDependencies` and forward it to each `factory.instantiate(...)` call.
- `crates/gateway/src/state.rs::AppCore::build` — construct `FactoryDependencies { providers: &self.providers }` before calling `PluginRegistry::build`.
- Test fixtures and `for_testing_*` helpers — add `deps` parameter; provide a `FactoryDependencies::empty()` test helper (returns a `FactoryDependencies` pointing at a `static` empty `HashMap`).

### 5.3 Forward compatibility

`FactoryDependencies` is a struct, not a trait — adding fields later (e.g., metrics handles for P06c user-supplied factories) is non-breaking. Keeping it single-field today.

---

## 6. Algorithms

### 6.1 Group-aware scanning (`groups.rs`)

```rust
/// Split messages into groups. Each group is a contiguous [start, end) range
/// starting at a user-role message and extending up to (but not including)
/// the next user-role message.
fn group_boundaries(messages: &[Message]) -> Vec<Range<usize>> {
    if messages.is_empty() { return Vec::new(); }
    let mut groups = Vec::new();
    let mut start = 0usize;
    for (i, msg) in messages.iter().enumerate().skip(1) {
        if msg.role == MessageRole::User {
            groups.push(start..i);
            start = i;
        }
    }
    groups.push(start..messages.len());
    groups
}
```

**Properties (test-covered):**

- `groups.iter().map(|r| r.len()).sum::<usize>() == messages.len()` (no gaps, no overlaps).
- Adjacent groups do not overlap.
- Every group's first message is user-role **except** for the degenerate case where `messages[0]` itself is non-user (defensive — preserve input, do not panic).
- `messages.is_empty() ⇒ groups.is_empty()`.

### 6.2 Token counting

`count_messages_tokens` sums `agent_shim_tokens::count_text(text)` over every `ContentBlock::Text(t)` and `ContentBlock::Reasoning(r)` in the given range. All other content variants contribute 0 (image/audio/file/tool_call/tool_result/redacted_reasoning/unsupported are not token-billed via cl100k_base; their cost is router/billing territory).

Spec note to be embedded in the code: **"text-based token estimate; image/binary contributions ignored"**.

### 6.3 `drop_old_turns` algorithm

```text
groups = group_boundaries(messages)
if groups.len() <= keep_last_n:
    return CompressionResult { new_messages: messages, dropped: 0, action: "skipped" }
drop_until = groups.len() - keep_last_n
dropped_count = groups[drop_until].start
new_messages = messages[groups[drop_until].start..].to_vec()
return CompressionResult {
    new_messages, dropped: dropped_count, action: "compressed"
}
```

### 6.4 `truncate_to_tokens` algorithm

```text
groups = group_boundaries(messages)
if groups.len() <= keep_last_n:
    return CompressionResult { new_messages: messages, dropped: 0, action: "skipped" }
total = sum(count_messages_tokens(groups[i]) for i in 0..groups.len())
if total <= target_tokens:
    return CompressionResult { new_messages: messages, dropped: 0, action: "skipped" }
drop_idx = 0
while total > target_tokens and drop_idx < groups.len() - keep_last_n:
    total -= count_messages_tokens(groups[drop_idx])
    drop_idx += 1
dropped_count = groups[drop_idx].start
action = if total > target_tokens { "compressed_but_over_budget" } else { "compressed" }
new_messages = messages[groups[drop_idx].start..].to_vec()
return CompressionResult { new_messages, dropped: dropped_count, action }
```

**Invariants:**

- If `groups.len() <= keep_last_n` at entry, the result is `messages` unchanged regardless of total token count (the trailing floor is the hardest constraint).
- If trimming down to `keep_last_n` groups still exceeds `target_tokens`, the result reports `"compressed_but_over_budget"` — operator's signal that their target is unreachable for the given conversation shape.

### 6.5 `summarize_old_turns` algorithm

```text
groups = group_boundaries(messages)
if groups.len() <= keep_last_n + 1:
    # need at least 1 old group to summarize
    return CompressionResult { messages, dropped: 0, action: "skipped" }

drop_count = groups.len() - keep_last_n
old_range = groups[0].start..groups[drop_count].start
kept_start = groups[drop_count].start

serialized = serialize_groups_to_text(&messages[old_range])
summary_req = build_summary_request(
    model = compiled.target.model,
    user_text = SUMMARY_PROMPT_PREFIX
        .replace("{N}", &max_summary_tokens.to_string())
        + "\n\nConversation:\n" + serialized,
    max_tokens = max_summary_tokens,
)

start = Instant::now()
result = tokio::time::timeout(
    timeout,
    compiled.provider.complete(summary_req, compiled.target.clone()),
).await
elapsed_secs = start.elapsed().as_secs_f64()

match result {
    Ok(Ok(stream)) => {
        summary_text = drain_text_from_stream(stream).await
        if summary_text.is_empty():
            goto fallback
        emit summary_duration_seconds{outcome="success"} = elapsed_secs
        new_first_msg = Message::user(vec![ContentBlock::text(
            format!("[Prior conversation summary]: {}", summary_text)
        )])
        new_messages = [new_first_msg, ...messages[kept_start..]]
        return CompressionResult {
            new_messages, dropped: kept_start, action: "compressed"
        }
    }
    Err(_timeout) | Ok(Err(_provider_err)) | empty_text =>
        # FALLBACK to drop_old_turns behavior
        emit summary_duration_seconds{outcome="fallback"} = elapsed_secs
        tracing::warn!(
            target: "agent_shim::prompt_compressor",
            "summarize_old_turns failed; falling back to drop_old_turns"
        )
        new_messages = messages[kept_start..].to_vec()
        return CompressionResult {
            new_messages, dropped: kept_start, action: "summary_fallback"
        }
}
```

**`SUMMARY_PROMPT_PREFIX`** (hard-coded constant in `summarize.rs`):

```text
Summarize the prior conversation between user and assistant in <= {N} tokens.
Preserve key facts, decisions, and unresolved questions.
Output plain text only, no greetings.
```

**`serialize_groups_to_text` format** — one stanza per message, blank line between stanzas:

- `ContentBlock::Text` / `ContentBlock::Reasoning` in user message → contributes to `User: <text>` stanza (multi-block text joined with `\n`).
- Same blocks in assistant message → contributes to `Assistant: <text>`.
- `ContentBlock::ToolCall(c)` → own line `[tool_call: <name>(<json_args>)]`.
- `ContentBlock::ToolResult(r)` → own line `[tool_result: <text or "<binary>">]`.
- `ContentBlock::{Image, Audio, File, RedactedReasoning, Unsupported}` → own line `[<kind> omitted]`.

Example for `[user("hi"), assistant(toolcall calc), user(toolresult "4"), assistant("answer is 4")]`:

    User: hi

    Assistant:
    [tool_call: calc({"a":2,"b":2})]

    User:
    [tool_result: 4]

    Assistant: answer is 4

Empty `User:` / `Assistant:` stanzas are valid (when the message has only tool blocks).

`drain_text_from_stream` reads the `BackendProvider::complete` stream until it ends; concatenates all `StreamEvent::TextDelta { text }` payloads; ignores Tool/Reasoning/Error events. Returns the joined string. Empty result -> fallback.

---

## 7. File layout

```text
crates/plugins/src/builtin/prompt_compressor/
  mod.rs              # PromptCompressor struct, Plugin trait impl, Factory, hooks(),
                      # Strategy re-export, CompressionResult, dispatch + metric emit.
  config.rs           # PromptCompressorConfig + Strategy enum + sub-configs + validation.
  groups.rs           # group_boundaries() + count_messages_tokens() (pure helpers).
  drop_old_turns.rs   # apply_drop_old_turns() -> CompressionResult.
  truncate.rs         # apply_truncate_to_tokens() -> CompressionResult.
  summarize.rs        # apply_summarize_old_turns() async + SUMMARY_PROMPT_PREFIX +
                      # serialize_groups_to_text() + drain_text_from_stream() +
                      # build_summary_request().
```

`CompressionResult` is the unified return type each strategy module produces:

```rust
struct CompressionResult {
    new_messages: Vec<Message>,
    dropped: usize,            // number of dropped messages (not groups) for metric
    action: &'static str,      // "compressed" | "skipped" | "compressed_but_over_budget" | "summary_fallback"
}
```

`mod.rs` is responsible for:

1. Dispatching to the per-strategy `apply_*` function.
2. Emitting `actions_total{strategy, action}` based on result.action.
3. Emitting `messages_dropped_total{strategy}` += result.dropped (skip if 0).
4. If `result.action == "skipped"`, returning original `req` unchanged (no message swap, no metric).
5. Otherwise, swapping `req.messages = result.new_messages` and returning `Ok(req)`.

`summary_duration_seconds` is the one metric that lives inside `summarize.rs` (not `mod.rs`) because only the summarize path knows the provider call duration. All other metrics are centralized in `mod.rs` for consistency.

---

## 8. Error handling

### 8.1 Plugin runtime behavior

| Situation | Action | Metric / Log |
|---|---|---|
| Config validation failure | `PluginConfigError` from factory | Gateway startup fails fast |
| `messages.is_empty()` | Return `Ok(req)` unchanged | None |
| Strategy returns `action == "skipped"` | Return `Ok(req)` unchanged | None |
| `count_messages_tokens` BPE encode | Cannot fail (cl100k uses `encode_ordinary` which is panic-free) | N/A |
| Summarize provider fails (Err / timeout / empty text) | Fall back to drop_old_turns; return `Ok(req with truncated messages)` | `summary_duration_seconds{outcome=fallback}`, `actions_total{action=summary_fallback}`, `warn!` |

**Plugin never returns `Err(PluginError::*)`.** Every failure mode has a deterministic in-plugin fallback. Returning `Err` would trigger the registry's `on_error: skip | fail` which contradicts the plugin's purpose:

- `skip` -> plugin appears to not run -> context overflow hits upstream -> 400/413
- `fail` -> 502 -> operator wanted "shorter prompt", not "502"

Falling back to `drop_old_turns` on summarize failure is the correct semantic: the operator's compression intent ("don't blow context") is honored, just with a less sophisticated mechanism than they requested.

### 8.2 Factory startup behavior

All `instantiate(plugin_name, config, deps)` failures return `PluginConfigError` (see §4.3). The gateway's `AppCore::build` propagates this up, causing `serve` to exit with a non-zero status. Operator sees the failure in the first log line; no first-request surprise.

---

## 9. Metrics

### 9.1 Catalog additions (`crates/observability/src/metrics/catalog.rs`)

```rust
#[derive(Metric)]
#[metric(
    name = "agent_shim_plugin_prompt_compressor_actions_total",
    kind = "counter",
    help = "prompt_compressor actions by strategy and outcome (Plan 07 P06b2)"
)]
pub struct PluginPromptCompressorActionsTotal;

#[derive(Metric)]
#[metric(
    name = "agent_shim_plugin_prompt_compressor_messages_dropped_total",
    kind = "counter",
    help = "Total messages dropped by prompt_compressor by strategy (Plan 07 P06b2)"
)]
pub struct PluginPromptCompressorMessagesDroppedTotal;

#[derive(Metric)]
#[metric(
    name = "agent_shim_plugin_prompt_compressor_summary_duration_seconds",
    kind = "histogram",
    help = "summarize_old_turns provider call duration by outcome (Plan 07 P06b2)"
)]
pub struct PluginPromptCompressorSummaryDurationSeconds;
```

### 9.2 Runtime labels

| Metric | Labels | Cardinality |
|---|---|---|
| `actions_total` | `strategy` (3) × `action` (4) | 12 |
| `messages_dropped_total` | `strategy` (3) | 3 |
| `summary_duration_seconds` | `outcome` (2: success, fallback) | 2 |

`strategy` values: `drop_old_turns | truncate_to_tokens | summarize_old_turns`.
`action` values: `compressed | skipped | compressed_but_over_budget | summary_fallback`.

No `plugin_name` dimension — `plugin_invocations_total` (P05) already provides that breakdown; joining via Prometheus query reaches per-instance attribution.

### 9.3 Baseline JSON

`crates/observability/tests/v060_metrics_baseline.json` gets 3 new entries (sorted by name). `distributed_slice_collects_all_markers` count moves from 19 -> 22.

---

## 10. Tests

### 10.1 `config.rs` (5 tests)

| Test | Verifies |
|---|---|
| `config_drop_old_turns_round_trips` | YAML -> `Strategy::DropOldTurns { keep_last_n: 4 }` |
| `config_truncate_to_tokens_round_trips` | `Strategy::TruncateToTokens { target_tokens: 8192, keep_last_n: 2 }` |
| `config_summarize_round_trips_with_defaults` | omitted `max_summary_tokens`/`timeout_ms` -> 300 / 5000 |
| `config_unknown_strategy_type_rejected` | `strategy.type: bogus` -> serde error |
| `config_unknown_field_rejected` | `strategy.foo: 1` -> `deny_unknown_fields` |

### 10.2 `groups.rs` (6 tests)

| Test | Verifies |
|---|---|
| `empty_messages_no_groups` | `[]` -> `[]` |
| `single_user_message_one_group` | `[user]` -> `[0..1]` |
| `user_assistant_pair_one_group` | `[user, assistant]` -> `[0..2]` |
| `two_user_turns_two_groups` | `[u, a, u, a]` -> `[0..2, 2..4]` |
| `tool_turn_grouped_with_assistant` | `[u, a(toolcall), u(toolresult), a, u]` -> `[0..1, 1..4, 4..5]` |
| `count_messages_tokens_text_only` | text+reasoning counted; other blocks contribute 0 |

### 10.3 `drop_old_turns.rs` (4 tests)

| Test | Verifies |
|---|---|
| `drop_skipped_when_groups_le_keep_last_n` | 2 groups, keep_last_n=2 -> action="skipped" |
| `drop_keeps_exactly_n_groups` | 5 groups, keep_last_n=2 -> 2 left, dropped=count_of_3_groups |
| `drop_preserves_kept_messages_verbatim` | role / content / extensions unchanged |
| `drop_with_only_one_group_when_keep_is_one` | edge: 1 group, keep_last_n=1 -> skipped |

### 10.4 `truncate.rs` (5 tests)

| Test | Verifies |
|---|---|
| `truncate_skipped_when_already_under_budget` | total < target -> skipped |
| `truncate_skipped_when_groups_le_keep_last_n` | hard floor wins over budget |
| `truncate_drops_oldest_until_under_budget` | 4 groups @ ~100tok each, target=250, keep=1 -> drop 2 oldest |
| `truncate_compressed_but_over_budget` | trimmed to keep_last_n yet still > target -> action="compressed_but_over_budget" |
| `truncate_dropped_count_matches_message_count` | metric attribution accurate |

### 10.5 `summarize.rs` (7 tests)

| Test | Verifies |
|---|---|
| `summarize_skipped_when_too_few_groups` | `groups.len() <= keep_last_n + 1` -> skipped |
| `summarize_success_replaces_old_groups` | mock provider returns text -> new msg[0] is summary; rest unchanged |
| `summarize_summary_message_has_prefix` | first new message text starts with `[Prior conversation summary]: ` |
| `summarize_provider_error_falls_back` | mock returns Err -> action="summary_fallback", dropped messages cleared |
| `summarize_timeout_falls_back` | mock sleeps > timeout_ms -> action="summary_fallback" |
| `summarize_empty_response_falls_back` | mock returns empty text -> action="summary_fallback" |
| `serialize_groups_to_text_format` | sample conversation -> expected serialized string |

`MockProvider` is defined inside the test module: implements `BackendProvider` with configurable `result: Result<&[StreamEvent], ProviderError>` and `delay: Duration`.

### 10.6 `mod.rs` (6 tests)

| Test | Verifies |
|---|---|
| `factory_kind_name` | `"prompt_compressor"` |
| `factory_rejects_unknown_upstream` | summarize.upstream not in deps -> `InvalidValue` w/ field path |
| `factory_rejects_keep_last_n_zero` | -> `InvalidValue` |
| `hooks_returns_decoded_request_only` | `HookSet::DECODED_REQUEST` and nothing else |
| `h2_skipped_no_metric_emission` | small prompt -> `DebuggingRecorder` shows zero prompt_compressor metrics |
| `h2_compressed_emits_metrics` | large prompt -> `actions_total` + `messages_dropped_total` incremented |

### 10.7 Gateway integration test

`crates/gateway/tests/prompt_compressor_integration.rs`:

- Two mockito servers: `main` (returns canned `chat.completion`) and `summarizer` (returns `[FAKE SUMMARY]`).
- YAML config wires both as upstreams; `compressor` plugin uses `summarize_old_turns` with `keep_last_n: 2`.
- Send a request with 6 user+assistant pairs (12 messages).
- Assertions:
  - `summarizer` upstream hit exactly once (any body).
  - `main` upstream hit exactly once with body matching:
    - `PartialJsonString({"messages": [{"role": "user"}]})` (first msg is user).
    - `Regex(\[Prior conversation summary\]: \[FAKE SUMMARY\])`.
  - Gateway returns HTTP 200.

drop_old_turns / truncate_to_tokens paths are fully covered by plugin-crate unit tests; only summarize_old_turns needs the full wiring path verified end-to-end.

---

## 11. Acceptance criteria

1. `cargo nextest run --workspace` passes — baseline 878 + ~31 plugin unit tests + 1 integration -> ~910.
2. `cargo fmt --all -- --check` shows no diff.
3. `cargo clippy --workspace --all-targets -- -D warnings` returns zero warnings.
4. `git diff master..HEAD -- crates/core/` is empty (frozen-core invariant).
5. Each YAML example in §3 has a config-round-trip test.
6. Startup-time validation: misspelled `summarizer.upstream` in YAML causes gateway boot failure with an error message naming the field and listing known upstreams.
7. `v060_metrics_baseline.json` gains 3 entries; `distributed_slice_collects_all_markers` test count moves 19 -> 22.
8. `cargo build --no-default-features --features usage_recorder` still compiles (feature gate isolation).

---

## 12. Risk register

| Risk | Mitigation |
|---|---|
| `summarize_old_turns` triggers a recursive H2 invocation | The provider is invoked directly via `Arc<dyn BackendProvider>::complete()`, bypassing the gateway pipeline entirely — physically impossible to re-enter the plugin chain |
| Slow summarizer upstream blows up P50 latency | Hard `timeout_ms` deadline (default 5s) + automatic fallback to drop_old_turns -> worst-case +5s tail, never indefinite hang |
| Mid-summarize gateway shutdown | Provider task aborts on shutdown signal; plugin observes `Err` -> fallback path; original request still receives compressed prompt |
| BPE encoder CPU cost on very long messages | `cl100k_base` via `count_text` is ~10 ms for 100 KB text on commodity hardware; only invoked in `truncate_to_tokens` path |
| Operator misconfigures `keep_last_n: 1` + `target_tokens: 100` -> nearly all conversations land in `compressed_but_over_budget` | Dedicated action label gives operator a Prometheus signal; spec documents the "hard floor wins" rule |
| `serialize_groups_to_text` for very large old-history triggers > `max_summary_tokens` summary | Spec explicitly notes "no recursive compression of summary"; out-of-scope per §2.2; operator picks smaller `keep_last_n` if hit |

---

## 13. Roll-forward / roll-back

- **Roll-forward**: feature is default-on; operator activates by adding a `prompt_compressor` plugin in YAML. Existing deployments without the YAML entry observe no behavior change.
- **Roll-back**: disable per route (remove from `on_decoded_request` list) or remove the `plugins:` entry. Cargo feature can be turned off at compile time (`--no-default-features --features usage_recorder,pii_scrubber`) to strip the code entirely.

## 14. Forward references

- **P06b3**: Prometheus sink for `usage_recorder` (extends `Sink::Log` to `Sink::{ Log, Prometheus }`).
- **P06b4** (hypothetical): operator-supplied summary prompt template / Jinja placeholders.
- **P06c**: user-supplied factories (operators provide their own `Box<dyn PluginFactory>`); the `FactoryDependencies` struct introduced here is the basis for additional injection (metrics handles, config snapshots, etc.).
