# Performance Deepening Opportunities

Generated from a static perf scan on 2026-06-03 against baseline commit `f599854`.

This is a candidate list, not an implementation plan. Goal: surface modules
whose hot paths spend visible CPU, memory, or contention budget that could be
reduced without changing externally observable behavior.

## Methodology

**Scope.** Three classes of cost: per-request **CPU time/latency**,
**throughput/concurrency** (lock contention, Arc churn, runtime scheduling),
and **memory/allocation** (per-request alloc count, stable RSS shape).
Explicitly **excluded**: cold-start time, binary size, compile time, upstream
token efficiency (already covered by `prompt_compressor` + cost filter).

**Source of judgement.** Pure static read of the hot-path code at
`f599854`, plus a sweep of historical perf-related commits and ADRs. No
instrumentation, no profiling, no benchmarks were run for this scan — every
`Suspicion` cites code, not measurements.

**Field template** (per candidate):

- **Where** — file + line range
- **Suspicion** — what the code does that costs CPU/throughput/memory, with the specific pattern observed
- **Cost class** — CPU / Throughput / Memory (≤ 2)
- **Measurement effort** — S (one `tracing::debug!` or counter, ~1h) / M (a criterion micro-bench, ~half day) / L (needs the harness from the 2026-06-03 spec, ~1-2 days)
- **Confidence** — High (clear pattern, named anti-pattern) / Medium (looks like, needs measurement) / Low (gut suspicion)

**Caps** (from grilling decisions): per-stage ≤ 2 candidates, cross-stage 3-5,
total target 10-15 — but if `High` confidence exceeds 5, trim to the strongest
5-8. Each candidate ≤ 5 paragraphs as named above; nothing else.

**Out by design.** No `Solution`, no `Benefits`, no `Suspected magnitude`.
Optimization work and benchmark execution are downstream artifacts that flow
from the items this list ranks.

---

## Per-stage scan

The five stages mirror the request lifecycle in `docs/architecture.md`:
`decode_request` → `Router::resolve` → `Admission::admit` → `provider.complete`
→ `encode_stream`. Auth, plugin hooks (H2/H3/H5/H7), and the gateway-level
spans/metrics layer thread through these stages but are addressed in the
cross-stage themes section so per-stage rows stay focused.

### Stage 1 — `decode_request`

#### 1. Anthropic passthrough double-decodes the body

**Where**
`crates/gateway/src/pipeline.rs:495-513` (`extract_model_from_body` to peek `model`) + `crates/gateway/src/pipeline.rs:602-605` (`decode_or_dump` for admission's canonical view) + `crates/providers/src/anthropic/passthrough.rs:78-93` (`rewrite_model` re-parses the entire body to swap one field).

**Suspicion**
On the Anthropic-passthrough hot path the inbound JSON body is parsed **three times**: once by `extract_model_from_body` (a single-field deserialize), once by `decode_or_dump` to build the canonical request the admission gates need (ADR-0009 PR3 chose this over byte-identity capability skip), and once by `rewrite_model` which `serde_json::from_slice` → mutate one key → `serde_json::to_vec` allocates a full new `Bytes`. For a 30 KB request body, that's three full JSON parses + one full serialize, all dropping the result almost immediately. No part of the system reads more than 1-2 fields from the second and third passes.

**Cost class** CPU / Memory

**Measurement effort** S — wrap the three parse sites with `tracing::debug!` carrying `body.len()` and `Instant::elapsed`, run any Anthropic-passthrough integration test, read the totals.

**Confidence** High — three independent parses on the same byte buffer is a named anti-pattern, and `rewrite_model`'s round-trip is documented in its own doc comment.

#### 2. `decode_or_dump` always builds a new `String` on failure even when no decode error has occurred

**Where**
`crates/gateway/src/pipeline.rs:1248-1306`

**Suspicion**
The success path is allocation-clean, but the helper closes over a `snapshot: &AppSnapshot` reference for the **sole purpose** of locating a fallback dump directory that is only used on `Err`. The borrow itself is cheap, but the call site duplicates the same `decode_or_dump(spec.frontend, &body, &snapshot)?` invocation twice (L509 and L604) on the passthrough path — both go through the success arm under normal traffic, but the second one is logically redundant: `decoded` from L509 is `Some(_)` and immediately re-decoded into the local `canonical` because the borrow checker forced `decoded.take()` earlier. Each redundant decode allocates everything the canonical model needs (Vec messages, Vec content blocks, String fields).

**Cost class** CPU / Memory

**Measurement effort** S — `tracing::debug!` counter per `decode_or_dump` call; expect 1× per non-passthrough request, 2× per passthrough request.

**Confidence** Medium — the dual call is real and visible, but the second one only fires when `decoded.take()` returned `None` (passthrough that pre-decoded model only). The exact frequency depends on branch order; needs counting to confirm.

### Stage 2 — `Router::resolve` / `ModelResolver::resolve_route`

#### 3. `resolve_route` clones every chain element on every fuzzy upgrade

**Where**
`crates/router/src/resolver.rs:77-91` (`apply_fuzzy_upgrades`) + `crates/router/src/static_routes.rs` (chain construction)

**Suspicion**
`apply_fuzzy_upgrades` calls `canonical.to_string()` to assign back into `target.model: String` for every chain element where the discovered canonical model differs from the configured one. Most production traffic hits this path because `ModelIndex` upgrades short aliases (e.g. `gpt-4o` → `gpt-4o-2024-11-20`) on every request. The upgrade target string is a `&str` from `ModelIndex`'s `BTreeMap` (lives forever) — it gets allocated as a new `String` per chain element per request. A 1-element chain pays 1 alloc/req; a 3-element fallback chain pays 3. The `target.model` field could be `Cow<'static, str>` or `Arc<str>` to reuse the index's existing storage.

**Cost class** Memory / CPU

**Measurement effort** M — needs a criterion bench measuring `ModelResolver::resolve_route` under realistic fallback-chain sizes; the alloc count is the headline.

**Confidence** High — the `to_string()` is unconditional and on the hot path; `ModelIndex` already stores the canonical form as a shared resource.

#### 4. Three `match frontend { FrontendKind::AnthropicMessages => "anthropic_messages", … }` formatters per request

**Where**
`crates/gateway/src/pipeline.rs:85-89` (root span), `:486-490` (dispatch_inner), `:1146-1155` (`find_route_entry`'s `frontend_str` + `alt`)

**Suspicion**
The same `FrontendKind → &'static str` mapping is hand-coded at three call sites per request. Each is cheap individually (3-arm match returning `&'static str`), but `find_route_entry` does it twice (once for `frontend_str`, once for `alt`) on every request, and then walks `config.routes` with a string-compare predicate. Three issues compound: (a) the match is duplicated code (not a perf bug per se but signals that `FrontendKind::as_str()` is missing); (b) `find_route_entry`'s O(N) scan over routes runs **per request** even though the route table is startup-frozen — could be a `HashMap<(FrontendKind, &str), &RouteEntry>` built once; (c) the `r.frontend == frontend_str || r.frontend == alt` short-circuit re-runs full string compares against every route entry.

**Cost class** CPU

**Measurement effort** S — wrap `find_route_entry` in a counter + elapsed micro-trace; with N typically 5-30 routes the absolute cost is small but the per-request multiplier is real.

**Confidence** Medium — clearly O(N) per request where a precomputed lookup would be O(1), but absolute magnitude depends on route table size; until measured, this could be 100ns or 10µs.

### Stage 3 — `Admission::admit`

#### 5. `resolved.chain.clone()` + per-chain-element `target.provider.clone()` + `route_label.to_string()` on every admit

**Where**
`crates/router/src/admission.rs:167` (`route_label.to_string()`), `:181` (`resolved.chain.clone()`), `:193,202` (`route_label.clone()` per skip/note), `:240` (`open_breakers.push(target.provider.clone())`), `:255` (`identity.clone()`).

**Suspicion**
`admit` is the cleanest "many small clones in one function" pattern in the codebase. The chain is cloned to hand to `cost_filter::filter_chain` (which then returns `survivors: Vec<BackendTarget>` — another full allocation). `route_label` is converted from `Arc<str>` to `String` once for metric labels, then cloned again on every skip and note. `identity` (an `AgentIdentity` enum, but `KeyHash` variant holds a `String`) is cloned to hand to the ticket. Per-request total under a 3-element fallback chain with no cost-filter activity: ~6 small string clones plus one `Vec<BackendTarget>` deep-clone. The metric counters' label closures further multiply this — `metrics::counter!(.., "upstream" => skip.upstream.clone(), "route" => route_label.clone())` allocates two `String`s per emission.

**Cost class** Memory / CPU

**Measurement effort** M — a criterion bench targeting `Admission::admit` directly, varying chain length 1/3/5 and toggling cost-filter activity, with a custom global allocator counter (e.g. `cap` or `tracking-allocator`).

**Confidence** High — every clone is visible; `route_label: Arc<str>` was specifically introduced in v0.9 to avoid this pattern downstream but the admit code converts back to `String` immediately.

#### 6. `cost_filter::estimate_request_cost` walks the entire request text via tiktoken on every admit

**Where**
`crates/router/src/cost_estimate.rs:54-82` + `crates/router/src/cost_filter.rs:filter_chain` (called from admission)

**Suspicion**
Every admit that has cost-filter cap configured walks **every text block in every message** through tiktoken `cl100k_base.encode_ordinary()`. tiktoken is a BPE encoder; complexity is roughly linear in input bytes, but per-call overhead is non-trivial (initialize internal state, regex tokenize). Worse: the same canonical request is encoded **once per chain element** if the cost filter runs per-element (need to verify in `filter_chain`), or even once per request if only per-route. The result (a u32 token count) could be cached on `CanonicalRequest` after the first computation since the request is immutable from admit onward — the rest of the pipeline (Anthropic frontend `count_tokens` endpoint, plugin `prompt_compressor`) also runs the same encoder over the same text.

**Cost class** CPU

**Measurement effort** M — bench `estimate_request_cost` directly with realistic message sizes (1KB, 10KB, 100KB text); orthogonally, grep the pipeline for other tiktoken call sites to confirm duplication.

**Confidence** Medium — tiktoken is the dominant cost in `estimate_request_cost` and runs on the admission hot path, but whether it actually duplicates across modules in production traffic depends on which paths are enabled (cost filter, count_tokens, prompt_compressor).

### Stage 4 — `provider.complete` (canonical chain walk via `ResilientCaller`)

#### 7. `chat_sse_parser` wraps `ParserState` in `Arc<Mutex<...>>` for a single-consumer stream

**Where**
`crates/providers/src/oai_chat_wire/chat_sse_parser.rs:15-51`

**Suspicion**
`parse()` constructs `let state = Arc::new(Mutex::new(ParserState::default()));` and uses an inner clone (`state_for_items`) inside `flat_map`'s closure. Per SSE event, the closure does `state_for_items.lock().expect(...)`. There is **exactly one consumer** (the `flat_map` itself) and the drain tail (`futures::stream::once`) runs strictly after `event_stream` ends — so `Arc + Mutex` is paying for synchronization that never happens. A `RefCell<ParserState>` (or just `&mut state` via `unfold`) would compile equally and skip the atomic + mutex acquisition per event. For a 100-event SSE response the mutex acquire/release pair runs 100 times.

**Cost class** CPU / Throughput

**Measurement effort** S — wrap the lock acquisition with `tracing::trace!` + a counter, run any Copilot-Chat streaming test, read the per-request acquire count.

**Confidence** High — `Arc<Mutex>` for a single-owner state machine is a named over-synchronization anti-pattern, and the producer pattern (`flat_map` + later `chain`) gives exclusive access by construction.

#### 8. Anthropic encoder's per-event `Arc::clone(&state)` + lock

**Where**
`crates/frontends/src/anthropic_messages/encode_stream.rs:69-92`

**Suspicion**
Same pattern as #7 but on the encode side: `state: Arc<Mutex<EncoderState>>` is cloned (`Arc::clone(&state)`) inside `flat_map`'s closure once per upstream event, then `state.lock()` runs per event. The `done: Arc<AtomicBool>` adds a second atomic op per event. The stream is single-producer/single-consumer by axum's design (one body per response). For a 100-event canonical stream → SSE encode, that's 100 Arc bump + 100 mutex acquires + 100 atomic loads — none of which serve a real concurrency need.

**Cost class** CPU / Throughput

**Measurement effort** S — same shape as #7, instrument the lock and Arc bump.

**Confidence** High — same anti-pattern in the same shape; the two encoders likely share the underlying belief that `flat_map`'s closure needs `'static`, but `unfold` or hand-rolled `Stream::poll_next` sidesteps the constraint.

### Stage 5 — `encode_stream` / `encode_unary`

#### 9. `serde_json::to_string` followed by `Bytes::from(String)` on every SSE event

**Where**
`crates/frontends/src/anthropic_messages/encode_stream.rs:48-61` (`serialize_event`) — same shape in `openai_chat::encode_stream` and `openai_responses::encode_stream`.

**Suspicion**
`serialize_event` does `serde_json::to_string(ev).ok()?` then hands the `String` to `sse::event(name, &data)` which formats `"event: {name}\ndata: {data}\n\n"` into a new buffer (likely another `String` then `Bytes::from`). That's 2-3 allocations per SSE event: serde's internal Vec buffer (which `to_string` then `String::from_utf8`s), the sse::event format buffer, and the final `Bytes`. Using `serde_json::to_writer` against a pre-sized `BytesMut` and `writeln!`-style framing would cut to 1 allocation per event. For a 100-event response, this is 200-300 allocations vs 100.

**Cost class** Memory / CPU

**Measurement effort** M — bench `serialize_event` over realistic event payloads (small text delta, medium tool-use chunk, large reasoning delta). Headline metric is allocations per event.

**Confidence** High — the chain `to_string → format! → Bytes::from` is visible in the source; a `BytesMut` path is a standard Rust SSE pattern.

#### 10. `interleaved_reasoning` per-event state machine allocates fresh `Vec` for `ContentBlockStart/Stop` pairs

**Where**
`crates/providers/src/oai_chat_wire/interleaved_reasoning.rs` (need to confirm exact lines)

**Suspicion**
The interleaved reasoning state machine (documented in CONTEXT.md as the central reasoning-text-tool ordering glue) emits `ContentBlockStart` + `ContentBlockStop` events around every transition. Each emission allocates a small Vec or singleton when a `flat_map` returns its `iter()`. On chatty upstreams (DeepSeek interleaves heavily, Anthropic less so) this fires many times per request. The state machine itself is well-shaped, but the per-transition allocation pattern adds up.

**Cost class** Memory

**Measurement effort** M — bench the parser against a Anthropic-shape fixture with N reasoning-text transitions, count allocations.

**Confidence** Low — pattern is plausible from architecture docs, but exact code shape not read yet; could already be using `SmallVec` or `Option` for the common single-event case.

---

(Cross-stage themes, cross-reference with prior scans, and Top picks will land
in batches 2 + 3.)
