# Phase 7 P06b1 — pii_scrubber Built-in Plugin (Design)

**Date:** 2026-05-22
**Predecessor:** P06a (`docs/superpowers/specs/2026-05-18-phase-7-p06a-registry-builder-design.md`)
**Branch:** `worktree-phase-7-p06b-builtins`
**Frozen-core invariant:** Preserved — `crates/core/` must not change.

## 1. Goal

Ship `pii_scrubber` as the second built-in plugin in `agent-shim-plugins`. The plugin redacts personally identifiable information from both inbound prompts (H2 hook) and outbound stream responses (H5 hook) using operator-supplied regex rules. P06b1 is the first of three sibling sub-projects originally deferred from P06a; siblings (P06b2 = `prompt_compressor`, P06b3 = Prometheus sink for `usage_recorder`) land in separate spec → plan cycles.

## 2. Scope

In scope:
- `pii_scrubber` plugin with separate `inbound` and `outbound` rule lists.
- Eager regex compilation at startup (Layer B).
- H2 + H5 hook subscription, scoped by which rule lists are populated.
- Per-request streaming buffer for H5 (256-byte sliding window) to catch matches that straddle delta boundaries.
- Counter metric `agent_shim_plugin_pii_scrubber_matches_total{rule, direction}` declared in the observability catalog.
- New `PluginContext.scratch` field for plugin-managed per-request state.
- Cargo feature `pii_scrubber = ["dep:regex"]`, default-on.
- One gateway integration test exercising the full path.

Out of scope (deferred):
- `prompt_compressor` (P06b2).
- Prometheus sink for `usage_recorder` (P06b3).
- Tool-call argument scanning, ToolResult scanning, RedactedReasoning scanning.
- User-supplied factories (P06c).
- Per-rule disable/enable via runtime API.

## 3. Architecture & Data Flow

```
YAML config: plugins: { pii_scrub: { type: pii_scrubber, config: {...} } }
   ↓
Layer A (config crate): structural validation, undeclared-reference rejection.
   ↓
Layer B (PluginRegistry::build, from P06a):
  - Calls PiiScrubberFactory::instantiate(name, config_json)
  - serde_json::from_value -> PiiScrubberConfig (deny_unknown_fields)
  - Name uniqueness + format checks per direction
  - Compile every Regex
  - Returns Box<dyn Plugin> = PiiScrubber { inbound, outbound }
   ↓
Pipeline per request:
  H2: scan Message.content[].Text + ReasoningBlock; apply inbound rules sequentially.
  H5: per-event buffered scrub; flush on non-text events.
   ↓
Counter increments labeled by {rule, direction}.
```

The plugin sits entirely inside `crates/plugins/`. The only crate-graph addition is a workspace `regex` dependency, pulled into `agent-shim-plugins` behind the new `pii_scrubber` feature.

## 4. PluginContext.scratch (architectural change)

H5 needs per-request state to maintain a sliding-window buffer across multiple `on_stream_event` invocations. The trait's `&self` makes plugin-internal state impossible (without bespoke `Arc<Mutex<HashMap<RequestId, ...>>>` that risks leaks). We extend `PluginContext` instead.

### 4.1 New field

```rust
// crates/plugins/src/context.rs
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

pub struct PluginContext {
    pub request_id: RequestId,
    pub frontend: FrontendKind,
    pub route_label: String,
    /// Per-request scratch storage for plugins that need to maintain
    /// state across hook invocations within a single request. Keys are
    /// plugin `kind_name()` literals; plugins MUST namespace their
    /// entries by their own kind to avoid collisions.
    ///
    /// Cloning a `PluginContext` Arc-shares the same scratch map, so
    /// the pipeline's `ctx.clone()` between hooks preserves state.
    /// The map is dropped with the context — after H7 has been
    /// dispatched and the pipeline scope ends.
    pub(crate) scratch: Arc<parking_lot::RwLock<HashMap<&'static str, Box<dyn Any + Send + Sync>>>>,
}
```

### 4.2 Construction & access

```rust
impl PluginContext {
    /// Construct a new `PluginContext` with an empty scratch map.
    /// Used by the gateway pipeline; not part of the plugin-facing API.
    pub fn new(request_id: RequestId, frontend: FrontendKind, route_label: String) -> Self {
        Self {
            request_id, frontend, route_label,
            scratch: Arc::new(parking_lot::RwLock::new(HashMap::new())),
        }
    }

    /// Acquire a mutable reference to the per-request scratch slot for
    /// the given plugin kind. The init closure runs only when the slot
    /// is empty for this request.
    ///
    /// Returns an `RwLockWriteGuard`-wrapped reference to the typed
    /// state. Holding the guard blocks other lookups for the same
    /// request — fine in practice because H5 hooks are serialized per
    /// request by GuardedH5Stream.
    ///
    /// # Panics
    /// If the slot exists but holds a type other than `T`. This is a
    /// programmer error: two plugins claiming the same kind key. The
    /// registry's `kind_name()` uniqueness check (P06a
    /// `DuplicateFactoryKind`) makes this unreachable in production.
    pub fn scratch_get_or_init<T, F>(&self, kind: &'static str, init: F) -> ScratchGuard<'_, T>
    where
        T: Send + Sync + 'static,
        F: FnOnce() -> T,
    {
        let mut map = self.scratch.write();
        map.entry(kind).or_insert_with(|| Box::new(init()) as Box<dyn Any + Send + Sync>);
        ScratchGuard { map, kind, _t: std::marker::PhantomData }
    }
}

pub struct ScratchGuard<'a, T: 'static> {
    map: parking_lot::RwLockWriteGuard<'a, HashMap<&'static str, Box<dyn Any + Send + Sync>>>,
    kind: &'static str,
    _t: std::marker::PhantomData<T>,
}

impl<'a, T: 'static> std::ops::Deref for ScratchGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.map.get(self.kind).expect("inserted above")
            .downcast_ref::<T>()
            .unwrap_or_else(|| panic!(
                "scratch key collision: kind `{}` holds wrong type", self.kind
            ))
    }
}

impl<'a, T: 'static> std::ops::DerefMut for ScratchGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.map.get_mut(self.kind).expect("inserted above")
            .downcast_mut::<T>()
            .unwrap_or_else(|| panic!(
                "scratch key collision: kind `{}` holds wrong type", self.kind
            ))
    }
}
```

### 4.3 Migration impact

Every callsite that constructs `PluginContext` via struct-init must switch to `PluginContext::new(...)`. Audit list:

- `crates/gateway/src/pipeline.rs` (production construction site)
- `crates/plugins/src/context.rs` (one test)
- `crates/plugins/src/registry.rs` (multiple tests + P06a `build_*` test helpers)
- `crates/plugins/src/builtin/usage_recorder.rs` (test helper `make_ctx`)
- `crates/plugins/src/supervisor.rs` (tests)
- `crates/plugins/src/log_fields.rs` (tests, if any)
- `crates/gateway/tests/*.rs` (integration tests, if any directly construct)

Mechanical change. No semantic shift.

## 5. Config schema

### 5.1 YAML

```yaml
plugins:
  pii_scrub:
    type: pii_scrubber
    on_error: skip
    timeout_ms: 50
    enabled: true
    config:
      inbound:
        - name: email
          pattern: "[\\w.+-]+@[\\w.-]+\\.[a-z]{2,}"
          replacement: "[REDACTED-EMAIL]"
        - name: ssn
          pattern: "\\b\\d{3}-\\d{2}-\\d{4}\\b"
          replacement: "[REDACTED-SSN]"
          case_insensitive: false
          multiline: false
      outbound:
        - name: api_key
          pattern: "sk-[A-Za-z0-9]{32,}"
          replacement: "[REDACTED-KEY]"
```

### 5.2 Rust types (in `crates/plugins/src/builtin/pii_scrubber.rs`)

```rust
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PiiScrubberConfig {
    #[serde(default)]
    pub inbound: Vec<RuleConfig>,
    #[serde(default)]
    pub outbound: Vec<RuleConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    pub name: String,
    pub pattern: String,
    pub replacement: String,
    #[serde(default)]
    pub case_insensitive: bool,
    #[serde(default)]
    pub multiline: bool,
}
```

### 5.3 Compiled internal types (private)

```rust
struct CompiledRule {
    name: String,
    regex: regex::Regex,
    replacement: String,
}

pub struct PiiScrubber {
    inbound: Vec<CompiledRule>,
    outbound: Vec<CompiledRule>,
}
```

### 5.4 Factory

```rust
pub struct PiiScrubberFactory;

impl PluginFactory for PiiScrubberFactory {
    fn kind_name(&self) -> &'static str { "pii_scrubber" }

    fn instantiate(&self, plugin_name: &str, config: serde_json::Value)
        -> Result<Box<dyn Plugin>, PluginConfigError>
    {
        let cfg: PiiScrubberConfig = serde_json::from_value(config)
            .map_err(|e| PluginConfigError::Deserialize(e, plugin_name.to_string()))?;

        let inbound  = compile_rules(plugin_name, "inbound",  &cfg.inbound)?;
        let outbound = compile_rules(plugin_name, "outbound", &cfg.outbound)?;

        if inbound.is_empty() && outbound.is_empty() {
            tracing::warn!(
                plugin = plugin_name,
                "pii_scrubber instantiated with empty inbound and outbound — no scrubbing will occur"
            );
        }

        Ok(Box::new(PiiScrubber { inbound, outbound }))
    }
}

fn compile_rules(
    plugin_name: &str,
    direction: &'static str,
    rules: &[RuleConfig],
) -> Result<Vec<CompiledRule>, PluginConfigError> {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(rules.len());

    for (i, r) in rules.iter().enumerate() {
        // Name format check.
        if !is_valid_rule_name(&r.name) {
            return Err(PluginConfigError::InvalidValue {
                plugin: plugin_name.to_string(),
                field: format!("{direction}[{i}].name"),
                reason: format!(
                    "rule name must match ^[A-Za-z0-9_-]{{1,64}}$, got `{}`",
                    r.name
                ),
            });
        }
        // Name uniqueness check.
        if !seen.insert(r.name.clone()) {
            return Err(PluginConfigError::InvalidValue {
                plugin: plugin_name.to_string(),
                field: format!("{direction}[{i}].name"),
                reason: format!("duplicate rule name `{}`", r.name),
            });
        }
        // Compose flag prefix.
        let mut flag = String::new();
        if r.case_insensitive { flag.push('i'); }
        if r.multiline        { flag.push('m'); }
        let final_pattern = if flag.is_empty() {
            r.pattern.clone()
        } else {
            format!("(?{flag}){}", r.pattern)
        };
        let regex = regex::Regex::new(&final_pattern).map_err(|e| {
            PluginConfigError::InvalidValue {
                plugin: plugin_name.to_string(),
                field: format!("{direction}[{i}].pattern"),
                reason: format!("invalid regex `{}`: {e}", r.pattern),
            }
        })?;
        out.push(CompiledRule {
            name: r.name.clone(),
            regex,
            replacement: r.replacement.clone(),
        });
    }
    Ok(out)
}

fn is_valid_rule_name(s: &str) -> bool {
    if s.is_empty() || s.len() > 64 { return false; }
    s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
```

**Note on `PluginConfigError::InvalidValue.field`**: P06a defined this field as `&'static str` because all earlier callsites used static literals (e.g. `"sink"`). For pii_scrubber we need dynamic field paths like `inbound[3].name`. Two options were considered:

- **Leak a `Box<str>`** at factory time (small bounded leak, but accumulates in tests).
- **Change `field` to `String`**.

We choose option (b) — change `PluginConfigError::InvalidValue.field` from `&'static str` to `String` as part of this task. The change is mechanical: ~5 callsites in `usage_recorder.rs` get `"sink".to_string()`. No behavioural change. Documented as a callout in §14 (touching P06a-era code).

### 5.5 Hook subscription

```rust
impl Plugin for PiiScrubber {
    fn kind_name(&self) -> &'static str { "pii_scrubber" }

    fn hooks(&self) -> HookSet {
        let mut h = HookSet::empty();
        if !self.inbound.is_empty()  { h |= HookSet::DECODED_REQUEST; }
        if !self.outbound.is_empty() { h |= HookSet::STREAM_EVENT; }
        h
    }
    // hooks below
}
```

A config with only inbound rules subscribes only to H2 — no H5 dispatch cost on the stream path. Likewise for outbound-only.

## 6. H2 — inbound scrubbing

Iterate `req.messages`; for each `Message`, walk `content`; for `Text` and `Reasoning` blocks, run `apply_rules` over the text.

```rust
async fn on_decoded_request(
    &self,
    ctx: &PluginContext,
    mut req: CanonicalRequest,
) -> PluginResult<CanonicalRequest> {
    for msg in &mut req.messages {
        for block in &mut msg.content {
            match block {
                ContentBlock::Text(t)      => t.text = apply_rules(&self.inbound, &t.text, ctx, "inbound"),
                ContentBlock::Reasoning(r) => r.text = apply_rules(&self.inbound, &r.text, ctx, "inbound"),
                _ => {}
            }
        }
    }
    Ok(req)
}

fn apply_rules(
    rules: &[CompiledRule],
    text: &str,
    _ctx: &PluginContext,
    direction: &'static str,
) -> String {
    let mut current = std::borrow::Cow::Borrowed(text);
    for rule in rules {
        let match_count = rule.regex.find_iter(&current).count() as u64;
        if match_count > 0 {
            metrics::counter!(
                catalog::PluginPiiScrubberMatchesTotal::NAME,
                "rule" => rule.name.clone(),
                "direction" => direction,
            ).increment(match_count);
            let replaced = rule.regex.replace_all(&current, rule.replacement.as_str()).into_owned();
            current = std::borrow::Cow::Owned(replaced);
        }
    }
    current.into_owned()
}
```

Non-text content blocks (Image, Audio, File, ToolCall, ToolResult, RedactedReasoning, Unsupported) pass through unchanged. Rationale: tool-call argument JSON can contain valid PII (e.g. `{"recipient_email": "x@y.com"}`) but a regex applied to JSON risks breaking quote/brace structure; operator should write a domain-specific plugin for that.

## 7. H5 — outbound streaming with sliding buffer

### 7.1 Buffer type (per-request scratch slot)

```rust
#[derive(Default)]
struct PiiBuffer {
    text: String,
    reasoning: String,
}

const MAX_PATTERN_TAIL: usize = 64;
const FLUSH_THRESHOLD: usize  = 256;
```

`MAX_PATTERN_TAIL = 64` is the safety margin retained after each scrub pass — long enough for typical PII patterns (email/phone/SSN/IPv6/credit card all ≤ 39 bytes) to be reassembled if they span deltas. `FLUSH_THRESHOLD = 256` is the buffer size that triggers a scrub-and-emit; values below this stay in buffer awaiting more text.

### 7.2 Hook impl

```rust
async fn on_stream_event(
    &self,
    ctx: &PluginContext,
    event: StreamEvent,
) -> PluginResult<Vec<StreamEvent>> {
    let mut buf = ctx.scratch_get_or_init::<PiiBuffer, _>("pii_scrubber", PiiBuffer::default);

    match event {
        StreamEvent::TextDelta { index, text } => {
            buf.text.push_str(&text);
            if buf.text.len() > FLUSH_THRESHOLD {
                let emitted = flush_safe_prefix(&mut buf.text, &self.outbound, ctx);
                Ok(if emitted.is_empty() { vec![] }
                   else { vec![StreamEvent::TextDelta { index, text: emitted }] })
            } else {
                Ok(vec![])
            }
        }
        StreamEvent::ReasoningDelta { index, text } => {
            buf.reasoning.push_str(&text);
            if buf.reasoning.len() > FLUSH_THRESHOLD {
                let emitted = flush_safe_prefix(&mut buf.reasoning, &self.outbound, ctx);
                Ok(if emitted.is_empty() { vec![] }
                   else { vec![StreamEvent::ReasoningDelta { index, text: emitted }] })
            } else {
                Ok(vec![])
            }
        }
        other => {
            let mut out = Vec::with_capacity(3);
            if !buf.text.is_empty() {
                let scrubbed = scrub_full(&buf.text, &self.outbound, ctx);
                buf.text.clear();
                if !scrubbed.is_empty() {
                    out.push(StreamEvent::TextDelta {
                        index: index_from_event(&other).unwrap_or(0),
                        text: scrubbed,
                    });
                }
            }
            if !buf.reasoning.is_empty() {
                let scrubbed = scrub_full(&buf.reasoning, &self.outbound, ctx);
                buf.reasoning.clear();
                if !scrubbed.is_empty() {
                    out.push(StreamEvent::ReasoningDelta {
                        index: index_from_event(&other).unwrap_or(0),
                        text: scrubbed,
                    });
                }
            }
            out.push(other);
            Ok(out)
        }
    }
}
```

### 7.3 Helpers

```rust
/// Scrub everything except the last `MAX_PATTERN_TAIL` bytes of the buffer.
/// Returns the scrubbed prefix; leaves the tail in `buf`.
fn flush_safe_prefix(buf: &mut String, rules: &[CompiledRule], ctx: &PluginContext) -> String {
    let split_at = floor_char_boundary(buf, buf.len().saturating_sub(MAX_PATTERN_TAIL));
    let prefix: String = buf.drain(..split_at).collect();
    apply_rules(rules, &prefix, ctx, "outbound")
}

fn scrub_full(buf: &str, rules: &[CompiledRule], ctx: &PluginContext) -> String {
    apply_rules(rules, buf, ctx, "outbound")
}

/// Find the largest char boundary ≤ idx. Std's `floor_char_boundary` is
/// unstable as of 1.85; polyfill here.
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() { return s.len(); }
    let bytes = s.as_bytes();
    let mut i = idx;
    while i > 0 && (bytes[i] & 0b1100_0000) == 0b1000_0000 { i -= 1; }
    i
}

fn index_from_event(e: &StreamEvent) -> Option<u32> {
    match e {
        StreamEvent::ContentBlockStart { index, .. }
        | StreamEvent::ContentBlockStop { index }
        | StreamEvent::ToolCallStart { index, .. }
        | StreamEvent::ToolCallArgumentsDelta { index, .. }
        | StreamEvent::ToolCallStop { index } => Some(*index),
        _ => None,
    }
}
```

### 7.4 Behavioral notes

- **End-of-stream tail.** `MessageStop` / `ResponseStop` are non-text events that trigger the final flush — buffered tail never gets stranded.
- **Block ordering.** Flushed `TextDelta` emissions land BEFORE the triggering non-text event in the output `Vec`. `ContentBlockStop` therefore sees the proper sequence: scrubbed text → block close.
- **Cancellation.** If the upstream stream drops mid-flight, no `MessageStop` arrives. The tail bytes never reach the client — exactly the safe outcome (PII not leaked).
- **Char boundaries.** `floor_char_boundary` ensures `split_at` lands on a valid UTF-8 boundary. Worst case the buffer retains an extra 1-3 bytes of the partially-cut character.
- **Index fallback.** Non-text trigger events that lack an `index` field (e.g. `UsageDelta`, `MessageStop`) cause flushed text to emit at index 0. Documented as a known limitation: in practice the very last text always emits within a block scope (block-level events do carry index).

## 8. Cargo wiring

### 8.1 Workspace Cargo.toml

Add to `[workspace.dependencies]`:

```toml
regex = "1"
```

The `regex` crate at 1.x is the stable Rust regex implementation. Approximate added build footprint: ~1MB of compiled deps (including `regex-automata`, `regex-syntax`, `aho-corasick`). Acceptable cost behind a Cargo feature flag.

### 8.2 plugins crate Cargo.toml

Add a new feature in `crates/plugins/Cargo.toml`:

```toml
[features]
default = ["usage_recorder", "pii_scrubber"]
usage_recorder = []
pii_scrubber = ["dep:regex"]

[dependencies]
# ... existing entries ...
regex = { workspace = true, optional = true }
```

The `dep:regex` syntax means the `pii_scrubber` Cargo feature owns the `regex` dep visibility; disabling the feature also drops `regex` from the build. (Per Cargo feature unification, if any OTHER workspace member pulls `regex` non-optionally, disabling the feature still allows the dep — but currently no other crate uses it, so disabling pii_scrubber slims the binary cleanly.)

## 9. Metrics

### 9.1 Catalog declaration

Add to `crates/observability/src/metrics/catalog.rs`:

```rust
#[derive(Metric)]
#[metric(
    name = "agent_shim_plugin_pii_scrubber_matches_total",
    kind = "counter",
    help = "Total PII scrub rule matches, labeled by rule name and direction (inbound|outbound)."
)]
pub struct PluginPiiScrubberMatchesTotal;
```

### 9.2 Emission contract

- `agent_shim_plugin_pii_scrubber_matches_total{rule, direction}` where:
  - `rule` is the operator-supplied rule `name` (validated against `^[A-Za-z0-9_-]{1,64}$`).
  - `direction` is the static string `"inbound"` or `"outbound"`.
- Incremented by the **match count** per rule per text body (one `replace_all` pass counts how many positions matched, not how many distinct calls). Calls with zero matches do NOT increment.

## 10. Errors & edge cases

| Stage | Source | Error | Behavior |
|-------|--------|-------|----------|
| Layer A | Malformed YAML for `config:` | `serde_yaml::Error` | Gateway exits at startup. |
| Layer B (deserialize) | Unknown field, missing required field, wrong type | `PluginConfigError::Deserialize` → `RegistryBuildError::Instantiation` | Gateway exits at boot, error names the plugin. |
| Layer B (name format) | Rule name with whitespace, `.`, etc. | `PluginConfigError::InvalidValue { field: "inbound[i].name", reason }` | Gateway exits at boot. |
| Layer B (name uniqueness) | Two rules in same direction share a name | `PluginConfigError::InvalidValue { ..., reason: "duplicate rule name `X`" }` | Gateway exits at boot. |
| Layer B (regex compile) | `regex::Regex::new` failure | `PluginConfigError::InvalidValue { field: "inbound[i].pattern", reason }` | Gateway exits at boot. |
| Layer B (empty config) | both `inbound: []` AND `outbound: []` | `tracing::warn!` log only | Plugin instantiates with `HookSet::empty()`, never runs. |
| H2 runtime | none — `replace_all` is infallible | — | — |
| H5 runtime | none — `apply_rules` is infallible | — | — |
| H5 scratch | type collision on `"pii_scrubber"` key (two plugins claim same kind) | `panic!` | Programmer error. `DuplicateFactoryKind` from P06a prevents this in production. |

**Capture group safety**: `Regex::replace_all` parses replacement strings; references to non-existent groups (`$5` when 2 exist) expand to empty string per the `regex` crate's documented semantics. Acceptable.

**Empty TextDelta** (`text: ""`): grows buffer by 0; threshold check fails; returns empty `Vec`. Downstream sees no spurious empty deltas.

**Zero-length regex matches**: `regex` crate handles these without infinite loops; no special-case needed.

## 11. Testing strategy

**Unit tests in `crates/plugins/src/builtin/pii_scrubber.rs`** (~12 tests):

1. `config_defaults` — empty `config: {}` parses to empty rule lists.
2. `config_deny_unknown_field` — `config: { extra: 1 }` rejected.
3. `factory_compiles_valid_regex` — happy path instantiation.
4. `factory_rejects_invalid_regex` — pattern `"[unclosed"` → `InvalidValue` naming the offending rule.
5. `factory_rejects_duplicate_rule_name` — two `email` rules in inbound.
6. `factory_rejects_invalid_name_char` — name `"my rule"` (space).
7. `factory_rejects_overlong_name` — 65-char name.
8. `hooks_minimal_subscription` — outbound-only config returns `HookSet::STREAM_EVENT` only.
9. `h2_scrubs_text_blocks` — request with one `Text` block; verify post-scrub content.
10. `h2_scrubs_reasoning_blocks` — same for `Reasoning`.
11. `h2_skips_other_blocks` — `Image` + `ToolCall` pass through unchanged.
12. `h2_counter_increments_per_match` — use `metrics-util::debugging` recorder to assert the counter fired.
13. `h5_buffer_below_threshold_returns_empty` — small delta, buffer < 256, returns `vec![]`.
14. `h5_buffer_above_threshold_emits_scrubbed_prefix` — large delta, verify emission.
15. `h5_pattern_spans_deltas_caught` — split a credit card number across two deltas; verify final emission has it redacted.
16. `h5_non_text_event_flushes_buffer` — `MessageStop` after pending text; verify scrubbed text emits before MessageStop.
17. `h5_utf8_boundary_safe` — multi-byte char straddling 256-byte mark; no panic, no corruption.

**Unit tests in `crates/plugins/src/context.rs`** (~3 tests):

1. `scratch_empty_by_default`
2. `scratch_get_or_init_returns_typed_handle`
3. `scratch_cloned_context_shares_map` — Arc-clone semantics.

**Catalog test in `crates/observability/src/metrics/catalog.rs`** (existing structural-parity test picks up the new entry automatically).

**Integration test in `crates/gateway/tests/pii_scrubber_integration.rs`** (1 test):

- YAML with one inbound rule + mocked-upstream that returns a streaming response carrying outbound PII.
- Drive one HTTP request via `app.oneshot(req)`.
- Assert (a) upstream received scrubbed prompt; (b) client received scrubbed response; (c) Prometheus `/metrics` text contains `agent_shim_plugin_pii_scrubber_matches_total{...}` non-zero counter; (d) frozen-core invariant holds.

**Total**: 17 unit tests in pii_scrubber.rs + 3 unit tests in context.rs + 1 gateway integration = 21 new tests. Baseline 855 → expected ~876.

## 12. Known limitations

- **Tool-call JSON not scanned.** PII in `ToolCallBlock.arguments_json` or `ToolResultBlock` content passes through. Operators needing this should write a custom plugin.
- **Index fallback for orphan flushes.** When a non-text event without an `index` field triggers a flush (e.g. `UsageDelta`, `MessageStop`), the emitted scrubbed text uses index 0. In practice the last text emission always lands inside a block scope (block-level events do carry an index), so this only matters for adversarially crafted streams.
- **No regex hot-reload.** Pattern changes require a gateway restart. Hot-reload of plugin config is out of scope for P06b1.
- **Scratch slot dead-code possible on H2-only plugins.** Plugins that subscribe to H2 only never use the scratch field — small memory overhead per request (~64 bytes for the empty `Arc<RwLock<HashMap>>`). Acceptable given the unified PluginContext shape.

## 13. Acceptance criteria

1. `pii_scrubber` factory registers under `builtin_plugins()` when the `pii_scrubber` Cargo feature is on (default-on).
2. `kind: pii_scrubber` parses YAML with `deny_unknown_fields` and accepts empty rule lists with a startup warning (no error).
3. Eager regex compile: malformed regex fails Layer B at boot with `PluginConfigError::InvalidValue` naming the offending rule by direction + index + field.
4. Eager name validation: duplicates within a direction fail; format `^[A-Za-z0-9_-]{1,64}$` enforced.
5. H2 inbound scrubs `Text` and `Reasoning` blocks; non-text blocks pass through unchanged.
6. H5 outbound: buffered 256-byte sliding window; matches that span deltas get caught; non-text events flush.
7. H5 outbound: `TextDelta` and `ReasoningDelta` buffers independent.
8. UTF-8 char-boundary safety in `flush_safe_prefix`.
9. Counter `agent_shim_plugin_pii_scrubber_matches_total{rule, direction}` declared in observability catalog and increments per match.
10. `PluginContext` gains `scratch` field and `scratch_get_or_init::<T, _>` helper.
11. `hooks()` returns minimal subscription set based on rule list emptiness.
12. All P06a tests still pass; total workspace test count ≈ 855 + 21 = ~876.
13. Frozen-core invariant: `git diff master..HEAD -- crates/core/` is empty.
14. Gateway integration test exercises full YAML → AppState::new → HTTP request → metric increment.
15. Clippy clean (`-D warnings`), rustfmt clean.

## 14. Spec dependencies and follow-ups

**Depends on (already landed):**
- P06a `PluginRegistry::build` (Layer B) — `pii_scrubber` factory plugs in via `builtin_plugins()`.
- P05 `GuardedH5Stream` — H5 hook dispatch + span aggregation.
- Observability metrics catalog (`crates/observability/src/metrics/catalog.rs`) — provides the `Metric` derive macro.

**Touches P06a-era code:**
- `PluginConfigError::InvalidValue.field` changes from `&'static str` to `String`. Five existing callsites in `crates/plugins/src/builtin/usage_recorder.rs` get a mechanical `.to_string()` append. Rationale: pii_scrubber needs dynamic field paths (`inbound[3].name`), and `String` is cleaner than leaking `Box<str>` at factory time.

**Follow-ups (not in P06b1):**
- P06b2: `prompt_compressor` plugin (H2 mutation, token-aware compression). Will share the new `regex` workspace dep through its own feature gate.
- P06b3: Prometheus sink for `usage_recorder`. Extends `Sink::Log` → `Sink::{ Log, Prometheus }`, integrates with `metrics::counter!` / `metrics::histogram!`.
- P06c: User-supplied plugin factories (operators provide their own `dyn PluginFactory` at build time).
