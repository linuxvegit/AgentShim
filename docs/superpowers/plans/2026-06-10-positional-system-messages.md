# Positional System Messages Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Teach the agent-shim canonical model to express system instructions that appear at any position inside the `messages` array, so Claude Code → `claude-opus-4-8` requests (which append a `role:"system"` SessionStart hook message) stop failing with `400 unknown role: system`.

**Architecture:** Add `MessageRole::System` and `Message.source: Option<SystemSource>` to the canonical model under ADR-0011's authorisation. Update three inbound frontend decoders to share one prelude-fold rule (leading run of system/developer → top-level `Vec<SystemInstruction>`; later → positional `Message{role: System}`). Update four outbound providers: Anthropic & OpenAI-Chat-shape preserve positional system in place; OpenAI Responses & Gemini downgrade to top-level instructions / systemInstruction with debug log.

**Tech Stack:** Rust 2021, `serde` / `serde_json`, `proptest`, `tracing`, `cargo nextest`. Workspace lives at `Q:/src/AgentShim`. Reference docs: `docs/superpowers/specs/2026-06-10-positional-system-messages-design.md`, `docs/adr/0011-canonical-message-additive-discipline.md`, `docs/adr/0007-frozen-core-lift-discipline.md`.

**Prerequisites checked before starting:**
- Working tree clean; on branch `master` at HEAD `6c50f3f` (spec + ADR-0011 already committed).
- `cargo nextest` installed (`cargo install cargo-nextest` if not).
- `cargo deny check` passes baseline.

---

## File Structure

### Created
None — every change is to an existing file.

### Modified (production)
- `crates/core/src/message.rs` — add `MessageRole::System` variant; add `Message.source: Option<SystemSource>`; add `Message::system(...)` constructor.
- `crates/core/src/mapping/anthropic_wire.rs` — extend `role_to_anthropic` / `role_from_anthropic` with `System` ↔ `"system"`; rewrite `role_unknown_returns_none` test.
- `crates/frontends/src/anthropic_messages/decode.rs` — fold leading-run `role:"system"` into `Vec<SystemInstruction>`; preserve later as `Message{role:System}`.
- `crates/frontends/src/openai_chat/decode.rs` — add `prelude_phase` walk; positional `system`/`developer` → `Message{role:System}` with source preserved.
- `crates/frontends/src/openai_responses/decode.rs` — same `prelude_phase` rule; `instructions` field content enters vec first, then `input` array runs prelude walk.
- `crates/providers/src/anthropic/request.rs` — no semantic code change (role_to_anthropic handles `System` automatically); add tests only.
- `crates/providers/src/oai_chat_wire/canonical_to_chat.rs` — add `MessageRole::System` arm in role match (source decides `"system"` vs `"developer"`).
- `crates/providers/src/openai_compatible/responses_api/encode_request.rs` — skip System messages from input array; append their text to `instructions` with debug log.
- `crates/providers/src/gemini/request.rs` — in `build_system_instruction`, also walk `req.messages` collecting System message text; in `build_contents`, skip System messages.

### Modified (tests / compile fallout)
- `crates/core/tests/proptest_roundtrip.rs` — extend `arb_message_role`; supply `source: None` in `arb_message`.
- Any file with an explicit `Message { role, content, name, extensions }` struct literal — add `source: None`. Compiler driver:
  - `crates/providers/src/anthropic/request.rs` (in `#[cfg(test)]`)
  - `crates/providers/src/oai_chat_wire/canonical_to_chat.rs` (`#[cfg(test)]`)
  - `crates/frontends/src/openai_responses/encode_stream.rs`
  - `crates/frontends/src/openai_responses/wire.rs`
  - `crates/frontends/src/openai_chat/wire.rs`
  - `crates/frontends/src/openai_chat/encode_unary.rs`
  (Exact list discovered by the compiler in Task 3; do not hand-edit ahead.)

### Modified (release artifacts)
- `CHANGELOG.md` — v0.10.0 entry with the hunk classification table from spec §2.8.

---

## Conventions

- Commit per task. Commit messages follow `type(scope): summary` (e.g. `feat(core): add MessageRole::System variant`).
- `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` must pass before every commit. Run `cargo fmt --all` (without `--check`) if formatting drift is found.
- Test runner: `cargo nextest run`; fall back to `cargo test` only if nextest unavailable.
- On Windows, if `cargo build --release` fails with `Access is denied (os error 5)` because a running `agent-shim.exe` holds the binary, rename the locked exe per CLAUDE.md and retry.
- "RTK prefix" reminder from CLAUDE.md applies to all git / cargo / shell commands the human runs interactively (`rtk git ...`, `rtk cargo ...`); inside this plan we write raw commands for clarity, but the executor should prefix `rtk` per project preference.

---

## Task Index

1. Add `MessageRole::System` variant
2. Update `role_to_anthropic` / `role_from_anthropic` and rewrite affected test
3. Add `Message.source` field + `Message::system` constructor; fix compile fallout
4. Extend proptest generator to cover `MessageRole::System`
5. Anthropic Messages frontend: prelude-fold + positional rule
6. OpenAI Chat frontend: prelude_phase walk
7. OpenAI Responses frontend: prelude_phase walk with `instructions` priming
8. Anthropic provider: tests for positional `Message{role:System}` flowing through
9. OpenAI Chat wire provider: dispatch `MessageRole::System` per source
10. OpenAI Responses provider: collapse positional System into instructions
11. Gemini provider: collapse positional System into systemInstruction
12. Cross-protocol integration tests (`crates/protocol-tests/`)
13. Reproduce the original bug end-to-end and confirm fix
14. CHANGELOG entry + final release-gate check

---

## Task 1: Add `MessageRole::System` variant

**Files:**
- Modify: `crates/core/src/message.rs`
- Test: `crates/core/src/message.rs` (`#[cfg(test)] mod tests`)

Authorised by ADR-0011 §(iv). After this task `MessageRole` has four variants and dispatch sites that use it elsewhere will fail to compile — Task 2 fixes the one in `anthropic_wire.rs`, Tasks 5–11 fix the rest.

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/message.rs` inside `#[cfg(test)] mod tests`:

```rust
#[test]
fn message_role_system_serializes_snake_case() {
    let j = serde_json::to_string(&MessageRole::System).unwrap();
    assert_eq!(j, "\"system\"");
}

#[test]
fn message_role_system_round_trips() {
    let role: MessageRole = serde_json::from_str("\"system\"").unwrap();
    assert_eq!(role, MessageRole::System);
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo nextest run -p agent-shim-core message_role_system
```

Expected: compile error `no variant or associated item named System found for enum MessageRole`.

- [ ] **Step 3: Add the variant**

Modify `crates/core/src/message.rs` at the `pub enum MessageRole` block. Replace:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    /// Used for tool result turns (OpenAI "tool" role).
    Tool,
}
```

with:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    /// Used for tool result turns (OpenAI "tool" role).
    Tool,
    /// A system instruction whose position within the conversation
    /// matters. Distinct from the top-level
    /// `CanonicalRequest::system` vec, which expresses session-level
    /// standing instructions with no temporal anchor.
    System,
}
```

- [ ] **Step 4: Run tests to verify the new tests pass and the rest of the core crate still compiles**

```bash
cargo nextest run -p agent-shim-core message_role_system
```

Expected: both new tests PASS.

Now check whether dispatch sites in the rest of the workspace compile. Expected: they DO NOT — many providers and frontends `match msg.role { User, Assistant, Tool }` exhaustively. Those failures are addressed in later tasks. For this task we only require the `agent-shim-core` crate itself to compile.

```bash
cargo build -p agent-shim-core
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/message.rs
git commit -m "feat(core): add MessageRole::System variant

Authorised by ADR-0011 (iv). Dispatch sites in other crates will be
updated in the same PR per ADR-0011 (iv) invariant 3 (atomic dispatch
update)."
```

---

## Task 2: Update `role_to_anthropic` / `role_from_anthropic` and rewrite affected test

**Files:**
- Modify: `crates/core/src/mapping/anthropic_wire.rs:26-40`
- Test: `crates/core/src/mapping/anthropic_wire.rs:97-100`

- [ ] **Step 1: Write the new tests (and prepare the rename)**

Replace the existing test block at lines 90-100 with:

```rust
    #[test]
    fn role_round_trip_system() {
        let s = role_to_anthropic(MessageRole::System);
        assert_eq!(s, "system");
        assert_eq!(role_from_anthropic(s), Some(MessageRole::System));
    }

    #[test]
    fn role_system_string_decodes_to_system_variant() {
        // Was previously `role_unknown_returns_none` asserting "system" -> None.
        // ADR-0011 (iv) + 2026-06-10 spec flip this: "system" is now a valid role.
        assert_eq!(role_from_anthropic("system"), Some(MessageRole::System));
    }

    #[test]
    fn role_unknown_returns_none() {
        assert_eq!(role_from_anthropic("human"), None);
        assert_eq!(role_from_anthropic("developer"), None);
    }
```

- [ ] **Step 2: Run tests to verify the new ones fail**

```bash
cargo nextest run -p agent-shim-core anthropic_wire
```

Expected: `role_round_trip_system` and `role_system_string_decodes_to_system_variant` FAIL (the production code still routes `"system"` to `None`). `role_unknown_returns_none` PASSES (already true that `"human"` is None).

- [ ] **Step 3: Extend the production match arms**

In `crates/core/src/mapping/anthropic_wire.rs`, replace:

```rust
pub fn role_to_anthropic(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "user",
    }
}

pub fn role_from_anthropic(s: &str) -> Option<MessageRole> {
    match s {
        "user" => Some(MessageRole::User),
        "assistant" => Some(MessageRole::Assistant),
        _ => None,
    }
}
```

with:

```rust
pub fn role_to_anthropic(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "user",
        MessageRole::System => "system",
    }
}

pub fn role_from_anthropic(s: &str) -> Option<MessageRole> {
    match s {
        "user" => Some(MessageRole::User),
        "assistant" => Some(MessageRole::Assistant),
        "system" => Some(MessageRole::System),
        _ => None,
    }
}
```

- [ ] **Step 4: Run tests to verify all three new tests pass**

```bash
cargo nextest run -p agent-shim-core anthropic_wire
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/mapping/anthropic_wire.rs
git commit -m "feat(core): map MessageRole::System to/from Anthropic 'system' wire role

ADR-0011 (iv) atomic dispatch update for the MessageRole::System variant.
Rewrites the role_unknown_returns_none test to assert against 'human'
since 'system' is now a valid role."
```

---

## Task 3: Add `Message.source` field + `Message::system` constructor; fix compile fallout

**Files:**
- Modify: `crates/core/src/message.rs`
- Modify (compile fallout): all files with `Message { role, content, name, extensions }` struct literals.

Authorised by ADR-0011 §(v). Atomic per (v) invariant 4: same diff adds the field and provides a configured constructor.

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/message.rs` inside `#[cfg(test)] mod tests`:

```rust
#[test]
fn message_system_constructor_sets_role_and_source() {
    let msg = Message::system(
        SystemSource::AnthropicSystem,
        vec![ContentBlock::text("hello")],
    );
    assert_eq!(msg.role, MessageRole::System);
    assert_eq!(msg.source, Some(SystemSource::AnthropicSystem));
    assert!(msg.name.is_none());
}

#[test]
fn message_serializes_source_only_when_present() {
    let msg = Message::system(
        SystemSource::OpenAiDeveloper,
        vec![ContentBlock::text("dev hint")],
    );
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"source\":\"openai_developer\""));

    let user_msg = Message::user(vec![ContentBlock::text("hi")]);
    let user_json = serde_json::to_string(&user_msg).unwrap();
    assert!(!user_json.contains("\"source\""));
}

#[test]
fn message_deserializes_old_wire_without_source() {
    let old_wire = r#"{"role":"user","content":[{"type":"text","text":"hi"}]}"#;
    let msg: Message = serde_json::from_str(old_wire).unwrap();
    assert_eq!(msg.role, MessageRole::User);
    assert!(msg.source.is_none());
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo nextest run -p agent-shim-core message::tests
```

Expected: compile error — `no field source on type Message` / `no associated function system`.

- [ ] **Step 3: Add the field and constructor**

In `crates/core/src/message.rs`, modify the `pub struct Message` block. Replace:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}
```

with:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Origin tag for `MessageRole::System` messages. Meaningful only
    /// when `role == System`; ignored for other roles. Lets the outbound
    /// encoder choose between `"system"` and `"developer"` wire role on
    /// OpenAI upstreams that distinguish them. Authorised by ADR-0011 (v).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SystemSource>,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}
```

Then update the existing `impl Message` block (`Message::user`, `Message::assistant`) to set `source: None`, and add a new `system` constructor. Replace:

```rust
impl Message {
    pub fn user(content: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::User,
            content,
            name: None,
            extensions: ExtensionMap::new(),
        }
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content,
            name: None,
            extensions: ExtensionMap::new(),
        }
    }
}
```

with:

```rust
impl Message {
    pub fn user(content: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::User,
            content,
            name: None,
            source: None,
            extensions: ExtensionMap::new(),
        }
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content,
            name: None,
            source: None,
            extensions: ExtensionMap::new(),
        }
    }

    pub fn system(source: SystemSource, content: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::System,
            content,
            name: None,
            source: Some(source),
            extensions: ExtensionMap::new(),
        }
    }
}
```

- [ ] **Step 4: Build the workspace and let the compiler enumerate fallout sites**

```bash
cargo build --workspace 2>&1 | grep -E "^error" | head -80
```

Expected: a list of `missing field source in initializer of Message` errors. Note every `<crate>/<path>:<line>` that appears.

- [ ] **Step 5: Mechanically add `source: None` at every fallout site**

For each file the compiler flagged, locate every literal `Message { role: ..., content: ..., name: ..., extensions: ... }` and insert `source: None,` between `name` and `extensions`. Examples of expected sites:

```rust
// before
Message {
    role: MessageRole::Tool,
    content: vec![...],
    name: None,
    extensions: ExtensionMap::new(),
}
// after
Message {
    role: MessageRole::Tool,
    content: vec![...],
    name: None,
    source: None,
    extensions: ExtensionMap::new(),
}
```

Files known from `Grep` survey at plan time (verify against compiler output):
- `crates/providers/src/oai_chat_wire/canonical_to_chat.rs` (tests)
- `crates/providers/src/anthropic/request.rs` (tests)
- `crates/providers/src/gemini/request.rs` (tests)
- `crates/frontends/src/openai_responses/encode_stream.rs`
- `crates/frontends/src/openai_responses/wire.rs`
- `crates/frontends/src/openai_chat/wire.rs`
- `crates/frontends/src/openai_chat/encode_unary.rs`
- `crates/core/tests/proptest_roundtrip.rs`
- `crates/protocol-tests/tests/responses_to_gemini_tools.rs`
- `crates/protocol-tests/tests/responses_to_anthropic_tools.rs`
- `crates/protocol-tests/tests/cancellation_fuzz.rs`
- `crates/router/tests/cost_estimate_image.rs`
- `crates/gateway/tests/plugins_observability.rs`
- `crates/gateway/tests/plugins_h5_stream.rs`
- `crates/gateway/tests/plugins_pipeline.rs`

(Compiler output is authoritative — use the list above only as a starting estimate.)

- [ ] **Step 6: Rebuild and verify the workspace compiles**

```bash
cargo build --workspace
```

Expected: PASS. If new `missing field source` errors appear in files NOT in the list above, add `source: None` and recompile.

- [ ] **Step 7: Run the new Message tests and the full test suite**

```bash
cargo nextest run -p agent-shim-core message::tests
cargo nextest run --workspace --no-fail-fast 2>&1 | tail -30
```

Expected: the three new tests PASS. The full suite may now reveal pre-existing or downstream issues — make a note of any new failures but defer fixing test-content drift to the per-frontend / per-provider tasks (5–11) where they belong; fix here ONLY if the failure is a struct-literal compile-time issue.

- [ ] **Step 8: Commit**

```bash
git add crates/core/src/message.rs crates/core/tests/proptest_roundtrip.rs crates/providers/ crates/frontends/ crates/protocol-tests/ crates/router/ crates/gateway/
git commit -m "feat(core): add Message.source field for system message origin

Authorised by ADR-0011 (v). Adds Message::system(source, content)
constructor; existing Message::user / Message::assistant default the
new field to None. Mechanical source: None additions across all
explicit struct literals in tests."
```

---

## Task 4: Extend proptest generator to cover `MessageRole::System`

**Files:**
- Modify: `crates/core/tests/proptest_roundtrip.rs`

Hunk classified as (iii) per spec §2.8.

- [ ] **Step 1: Update the generators**

Replace the `arb_message` function (and add a `arb_system_source` helper) in `crates/core/tests/proptest_roundtrip.rs`:

```rust
fn arb_system_source() -> impl Strategy<Value = SystemSource> {
    prop_oneof![
        Just(SystemSource::AnthropicSystem),
        Just(SystemSource::OpenAiSystem),
        Just(SystemSource::OpenAiDeveloper),
    ]
}

fn arb_message() -> impl Strategy<Value = Message> {
    let role = prop_oneof![
        Just(MessageRole::User),
        Just(MessageRole::Assistant),
        Just(MessageRole::System),
    ];
    let content = prop::collection::vec(arb_content_block(), 0..4);
    let source = prop_oneof![Just(None), arb_system_source().prop_map(Some)];
    (role, content, source).prop_map(|(role, content, source)| {
        // Only `System` carries a meaningful source; for User/Assistant
        // round-trip we still set the field explicitly (None vs Some
        // both must round-trip).
        Message {
            role,
            content,
            name: None,
            source: if role == MessageRole::System {
                source.or(Some(SystemSource::AnthropicSystem))
            } else {
                source
            },
            extensions: ExtensionMap::new(),
        }
    })
}
```

- [ ] **Step 2: Run the proptest**

```bash
cargo nextest run -p agent-shim-core --test proptest_roundtrip
```

Expected: PASS. The existing `canonical_request_json_round_trip` invariant now covers `System` and the new `source` field.

- [ ] **Step 3: Commit**

```bash
git add crates/core/tests/proptest_roundtrip.rs
git commit -m "test(core): proptest round-trip covers MessageRole::System + source"
```

---

## Task 5: Anthropic Messages frontend — prelude-fold + positional rule

**Files:**
- Modify: `crates/frontends/src/anthropic_messages/decode.rs`
- Test: same file's `#[cfg(test)] mod tests`

- [ ] **Step 1: Write failing tests**

In `crates/frontends/src/anthropic_messages/decode.rs::mod tests`, replace `decode_bad_role_is_rejected` and add the new tests:

```rust
#[test]
fn decode_truly_unknown_role_is_rejected() {
    let body = br#"{"model":"m","max_tokens":1,"messages":[{"role":"human","content":"hi"}]}"#;
    let err = decode(body).unwrap_err();
    assert!(matches!(err, FrontendError::InvalidBody(_)));
}

#[test]
fn decode_leading_system_in_messages_folds_to_session_vec() {
    let body = br#"{
        "model":"claude-opus-4-8",
        "max_tokens":1024,
        "messages":[
            {"role":"system","content":"You are a poet."},
            {"role":"user","content":"go"}
        ]
    }"#;
    let req = decode(body).unwrap();
    assert_eq!(req.system.len(), 1);
    assert_eq!(req.system[0].source, SystemSource::AnthropicSystem);
    assert_eq!(req.messages.len(), 1);
    assert_eq!(req.messages[0].role, MessageRole::User);
}

#[test]
fn decode_tail_system_in_messages_preserved_as_positional() {
    let body = br#"{
        "model":"claude-opus-4-8",
        "max_tokens":1024,
        "messages":[
            {"role":"user","content":"hi"},
            {"role":"system","content":"hook injection"}
        ]
    }"#;
    let req = decode(body).unwrap();
    assert!(req.system.is_empty());
    assert_eq!(req.messages.len(), 2);
    assert_eq!(req.messages[0].role, MessageRole::User);
    assert_eq!(req.messages[1].role, MessageRole::System);
    assert_eq!(req.messages[1].source, Some(SystemSource::AnthropicSystem));
    match &req.messages[1].content[0] {
        ContentBlock::Text(t) => assert_eq!(t.text, "hook injection"),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn decode_mid_system_in_messages_preserved_as_positional() {
    let body = br#"{
        "model":"claude-opus-4-8",
        "max_tokens":1024,
        "messages":[
            {"role":"user","content":"a"},
            {"role":"system","content":"shift gears"},
            {"role":"user","content":"b"}
        ]
    }"#;
    let req = decode(body).unwrap();
    assert!(req.system.is_empty());
    assert_eq!(req.messages.len(), 3);
    assert_eq!(req.messages[1].role, MessageRole::System);
}

#[test]
fn decode_top_level_system_plus_leading_messages_system_concatenate() {
    let body = br#"{
        "model":"claude-opus-4-8",
        "max_tokens":1024,
        "system":"Standing X",
        "messages":[
            {"role":"system","content":"Leading Y"},
            {"role":"user","content":"hi"}
        ]
    }"#;
    let req = decode(body).unwrap();
    assert_eq!(req.system.len(), 2);
    // Top-level "system" field entries come first.
    match &req.system[0].content[0] {
        ContentBlock::Text(t) => assert_eq!(t.text, "Standing X"),
        other => panic!("expected Text, got {other:?}"),
    }
    match &req.system[1].content[0] {
        ContentBlock::Text(t) => assert_eq!(t.text, "Leading Y"),
        other => panic!("expected Text, got {other:?}"),
    }
    assert_eq!(req.messages.len(), 1);
    assert_eq!(req.messages[0].role, MessageRole::User);
}

#[test]
fn decode_system_with_blocks_content_yields_text_blocks() {
    let body = br#"{
        "model":"claude-opus-4-8",
        "max_tokens":1024,
        "messages":[
            {"role":"user","content":"a"},
            {"role":"system","content":[{"type":"text","text":"shift"}]}
        ]
    }"#;
    let req = decode(body).unwrap();
    assert_eq!(req.messages[1].role, MessageRole::System);
    match &req.messages[1].content[0] {
        ContentBlock::Text(t) => assert_eq!(t.text, "shift"),
        other => panic!("expected Text, got {other:?}"),
    }
}
```

Remove the OLD `decode_bad_role_is_rejected` test (it asserted that `"system"` is rejected; that's no longer true).

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo nextest run -p agent-shim-frontends decode::tests
```

Expected: most new tests FAIL with `unknown role: system` or `assertion failed: req.system.len()` because the decoder still rejects `"system"`.

- [ ] **Step 3: Implement the prelude-fold rule**

In `crates/frontends/src/anthropic_messages/decode.rs::decode_request`, replace the messages-iteration block (currently approximately lines 61-81):

```rust
    // -- messages --
    let messages = req
        .messages
        .into_iter()
        .map(|m| {
            let role = role_from_anthropic(&m.role)
                .ok_or_else(|| FrontendError::InvalidBody(format!("unknown role: {}", m.role)))?;
            let content = match m.content {
                InboundMessageContent::Text(text) => vec![ContentBlock::text(text)],
                InboundMessageContent::Blocks(blocks) => blocks
                    .into_iter()
                    .map(inbound_block_to_canonical)
                    .collect::<Result<Vec<_>, _>>()?,
            };
            Ok(Message {
                role,
                content,
                name: None,
                extensions: ExtensionMap::new(),
            })
        })
        .collect::<Result<Vec<_>, FrontendError>>()?;
```

with:

```rust
    // -- messages with prelude-fold rule (2026-06-10 spec) --
    // Leading-run role:"system" entries fold into the session-level
    // `system` vec, joining any entries already produced from the
    // top-level `system` field. The first non-system message ends the
    // prelude; later role:"system" entries stay positional as
    // `Message{role:System}`.
    let mut messages: Vec<Message> = Vec::with_capacity(req.messages.len());
    let mut prelude_phase = true;
    for m in req.messages {
        let role = role_from_anthropic(&m.role)
            .ok_or_else(|| FrontendError::InvalidBody(format!("unknown role: {}", m.role)))?;
        let content = match m.content {
            InboundMessageContent::Text(text) => vec![ContentBlock::text(text)],
            InboundMessageContent::Blocks(blocks) => blocks
                .into_iter()
                .map(inbound_block_to_canonical)
                .collect::<Result<Vec<_>, _>>()?,
        };
        match role {
            MessageRole::System if prelude_phase => {
                system.push(SystemInstruction {
                    source: SystemSource::AnthropicSystem,
                    content,
                });
            }
            MessageRole::System => {
                messages.push(Message {
                    role: MessageRole::System,
                    content,
                    name: None,
                    source: Some(SystemSource::AnthropicSystem),
                    extensions: ExtensionMap::new(),
                });
            }
            other => {
                prelude_phase = false;
                messages.push(Message {
                    role: other,
                    content,
                    name: None,
                    source: None,
                    extensions: ExtensionMap::new(),
                });
            }
        }
    }
```

Note: the existing `let system = match req.system { ... }` block is currently `let`-binding (immutable). Change its binding to `let mut system: Vec<SystemInstruction> = match req.system { ... };` so the loop above can push to it.

Add `MessageRole` to the top-of-file import line if not already present:

```rust
use agent_shim_core::{
    // ... existing imports ...
    message::{Message, MessageRole, SystemInstruction, SystemSource},
    // ... existing imports ...
};
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo nextest run -p agent-shim-frontends decode::tests
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/frontends/src/anthropic_messages/decode.rs
git commit -m "feat(frontends/anthropic): fold leading messages-array system; preserve positional

Spec §3.1: leading run of role:'system' entries appends to the
session-level system vec (after any top-level system field entries).
Tail / mid-conversation role:'system' becomes Message{role:System,
source:Some(AnthropicSystem)}, preserving position semantics."
```

---

## Task 6: OpenAI Chat frontend — prelude_phase walk

**Files:**
- Modify: `crates/frontends/src/openai_chat/decode.rs`
- Test: same file's `#[cfg(test)] mod tests`

- [ ] **Step 1: Write failing tests**

Append to `crates/frontends/src/openai_chat/decode.rs::mod tests`:

```rust
#[test]
fn openai_decode_first_system_folds_to_top_level() {
    let body = br#"{
        "model":"gpt-4o",
        "messages":[
            {"role":"system","content":"You are concise."},
            {"role":"user","content":"hi"}
        ]
    }"#;
    let req = decode(body).unwrap();
    assert_eq!(req.system.len(), 1);
    assert_eq!(req.system[0].source, SystemSource::OpenAiSystem);
    assert_eq!(req.messages.len(), 1);
    assert_eq!(req.messages[0].role, MessageRole::User);
}

#[test]
fn openai_decode_developer_at_start_folds_with_source() {
    let body = br#"{
        "model":"gpt-4o",
        "messages":[
            {"role":"developer","content":"Be terse."},
            {"role":"user","content":"hi"}
        ]
    }"#;
    let req = decode(body).unwrap();
    assert_eq!(req.system.len(), 1);
    assert_eq!(req.system[0].source, SystemSource::OpenAiDeveloper);
}

#[test]
fn openai_decode_consecutive_prelude_systems_all_in_top_level() {
    let body = br#"{
        "model":"gpt-4o",
        "messages":[
            {"role":"system","content":"A"},
            {"role":"system","content":"B"},
            {"role":"user","content":"go"}
        ]
    }"#;
    let req = decode(body).unwrap();
    assert_eq!(req.system.len(), 2);
    assert!(req.messages.iter().all(|m| m.role != MessageRole::System));
}

#[test]
fn openai_decode_mid_system_preserved_in_messages() {
    let body = br#"{
        "model":"gpt-4o",
        "messages":[
            {"role":"user","content":"a"},
            {"role":"system","content":"shift"},
            {"role":"user","content":"b"}
        ]
    }"#;
    let req = decode(body).unwrap();
    assert!(req.system.is_empty());
    assert_eq!(req.messages.len(), 3);
    assert_eq!(req.messages[1].role, MessageRole::System);
    assert_eq!(req.messages[1].source, Some(SystemSource::OpenAiSystem));
}

#[test]
fn openai_decode_developer_in_middle_preserved_with_source() {
    let body = br#"{
        "model":"gpt-4o",
        "messages":[
            {"role":"user","content":"a"},
            {"role":"developer","content":"hint"},
            {"role":"user","content":"b"}
        ]
    }"#;
    let req = decode(body).unwrap();
    assert_eq!(req.messages[1].role, MessageRole::System);
    assert_eq!(req.messages[1].source, Some(SystemSource::OpenAiDeveloper));
}

#[test]
fn openai_decode_system_after_assistant_preserved() {
    let body = br#"{
        "model":"gpt-4o",
        "messages":[
            {"role":"user","content":"a"},
            {"role":"assistant","content":"b"},
            {"role":"system","content":"continue politely"}
        ]
    }"#;
    let req = decode(body).unwrap();
    assert!(req.system.is_empty());
    assert_eq!(req.messages.len(), 3);
    assert_eq!(req.messages[2].role, MessageRole::System);
}
```

If a similar existing test already asserts the old "fold ALL system to top-level" behaviour, it MUST be deleted or rewritten — find it via `grep -n "system" crates/frontends/src/openai_chat/decode.rs` in the `mod tests` section.

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo nextest run -p agent-shim-frontends openai_chat::decode
```

Expected: the mid-system / developer-in-middle / system-after-assistant tests FAIL because the current decoder folds all system to top-level.

- [ ] **Step 3: Modify the decoder to walk with `prelude_phase`**

In `crates/frontends/src/openai_chat/decode.rs::decode` (around lines 66-150), replace the inbound iteration block:

```rust
    for inbound in req.messages {
        let role_class = role_to_canonical(&inbound.role)
            .ok_or_else(|| FrontendError::InvalidBody(format!("unknown role: {}", inbound.role)))?;
        // ... existing text_content extraction ...
        match role_class {
            RoleClass::System(source) => {
                system.push(SystemInstruction {
                    source,
                    content: text_content,
                });
            }
            // ... existing arms ...
        }
    }
```

with the prelude-aware version. The full structure becomes:

```rust
    let mut prelude_phase = true;
    for inbound in req.messages {
        let role_class = role_to_canonical(&inbound.role)
            .ok_or_else(|| FrontendError::InvalidBody(format!("unknown role: {}", inbound.role)))?;

        let text_content: Vec<ContentBlock> = match inbound.content {
            None => vec![],
            Some(InboundMessageContent::Text(t)) => vec![ContentBlock::text(t)],
            Some(InboundMessageContent::Parts(parts)) => parts
                .into_iter()
                .map(|p| match p {
                    InboundContentPart::Text { text } => ContentBlock::text(text),
                    InboundContentPart::ImageUrl { image_url } => {
                        match image_url_to_binary_source(&image_url.url) {
                            Some(source) => ContentBlock::Image(ImageBlock {
                                source,
                                extensions: ExtensionMap::new(),
                            }),
                            None => ContentBlock::Unsupported(UnsupportedBlock {
                                origin: "openai_chat".into(),
                                raw: serde_json::json!({
                                    "type": "image_url",
                                    "image_url": { "url": image_url.url }
                                }),
                            }),
                        }
                    }
                })
                .collect(),
        };

        match role_class {
            RoleClass::System(source) if prelude_phase => {
                system.push(SystemInstruction {
                    source,
                    content: text_content,
                });
            }
            RoleClass::System(source) => {
                messages.push(Message {
                    role: MessageRole::System,
                    content: text_content,
                    name: inbound.name,
                    source: Some(source),
                    extensions: ExtensionMap::new(),
                });
            }

            RoleClass::Message(MessageRole::Tool) => {
                prelude_phase = false;
                // ... existing Tool branch unchanged ...
            }

            RoleClass::Message(role) => {
                prelude_phase = false;
                // ... existing Message branch unchanged ...
            }
        }
    }
```

Keep the inner bodies of `Tool` and `Message(role)` arms exactly as they currently exist — only the new `prelude_phase = false;` line at the top of each arm, and the split of `System` into `if prelude_phase` / `else`, are new.

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo nextest run -p agent-shim-frontends openai_chat::decode
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/frontends/src/openai_chat/decode.rs
git commit -m "feat(frontends/openai_chat): prelude_phase fold for system/developer

Spec §3.2: leading run of system/developer messages folds into the
session-level vec; once any non-system message appears, later
system/developer messages become positional Message{role:System}
with source preserved (OpenAiSystem vs OpenAiDeveloper)."
```

---

## Task 7: OpenAI Responses frontend — prelude_phase walk with `instructions` priming

**Files:**
- Modify: `crates/frontends/src/openai_responses/decode.rs`
- Test: same file's `#[cfg(test)] mod tests`

- [ ] **Step 1: Write failing tests**

Append to `crates/frontends/src/openai_responses/decode.rs::mod tests`:

```rust
#[test]
fn responses_decode_input_first_system_folds_when_no_instructions() {
    let body = br#"{
        "model":"gpt-5",
        "input":[
            {"type":"message","role":"system","content":[{"type":"input_text","text":"A"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]
    }"#;
    let req = decode(body).unwrap();
    assert_eq!(req.system.len(), 1);
    assert!(req.messages.iter().all(|m| m.role != MessageRole::System));
}

#[test]
fn responses_decode_instructions_and_leading_input_system_concatenate() {
    let body = br#"{
        "model":"gpt-5",
        "instructions":"Standing X",
        "input":[
            {"type":"message","role":"system","content":[{"type":"input_text","text":"Leading Y"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]
    }"#;
    let req = decode(body).unwrap();
    assert_eq!(req.system.len(), 2);
    // X first (from top-level instructions), then Y (from leading-run input).
    match &req.system[0].content[0] {
        ContentBlock::Text(t) => assert_eq!(t.text, "Standing X"),
        other => panic!("expected Text, got {other:?}"),
    }
    match &req.system[1].content[0] {
        ContentBlock::Text(t) => assert_eq!(t.text, "Leading Y"),
        other => panic!("expected Text, got {other:?}"),
    }
    assert_eq!(req.messages.len(), 1);
}

#[test]
fn responses_decode_input_mid_system_preserved() {
    let body = br#"{
        "model":"gpt-5",
        "instructions":"Standing X",
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"a"}]},
            {"type":"message","role":"system","content":[{"type":"input_text","text":"mid"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"b"}]}
        ]
    }"#;
    let req = decode(body).unwrap();
    assert_eq!(req.system.len(), 1);
    assert_eq!(req.messages.len(), 3);
    assert_eq!(req.messages[1].role, MessageRole::System);
    assert_eq!(req.messages[1].source, Some(SystemSource::OpenAiSystem));
}

#[test]
fn responses_decode_input_developer_round_trips_with_source() {
    let body = br#"{
        "model":"gpt-5",
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"a"}]},
            {"type":"message","role":"developer","content":[{"type":"input_text","text":"dev"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"b"}]}
        ]
    }"#;
    let req = decode(body).unwrap();
    assert_eq!(req.messages[1].role, MessageRole::System);
    assert_eq!(req.messages[1].source, Some(SystemSource::OpenAiDeveloper));
}
```

If `decode_instructions_become_system` (an existing test) collides with the new behaviour, update its expectation to use the new shape; do not delete it.

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo nextest run -p agent-shim-frontends openai_responses::decode
```

Expected: the mid-system / developer-in-middle / instructions+leading tests FAIL.

- [ ] **Step 3: Add `prelude_phase` to `decode_input`**

The OpenAI Responses decoder has two input-walk paths (`decode_input` and a second helper at line 201 visible from grep output). Update BOTH to track `prelude_phase` and produce positional `Message{role:System}` for non-prelude system/developer entries.

In each input-walking function (`decode_input` and the sibling at the second path), apply this pattern. Replace each existing branch like:

```rust
"system" => {
    system.push(SystemInstruction {
        source: SystemSource::OpenAiSystem,
        content: text,
    });
}
"developer" => {
    system.push(SystemInstruction {
        source: SystemSource::OpenAiDeveloper,
        content: text,
    });
}
```

with:

```rust
"system" if prelude_phase => {
    system.push(SystemInstruction {
        source: SystemSource::OpenAiSystem,
        content: text,
    });
}
"system" => {
    out.push(Message {
        role: MessageRole::System,
        content: text,
        name: None,
        source: Some(SystemSource::OpenAiSystem),
        extensions: ExtensionMap::new(),
    });
}
"developer" if prelude_phase => {
    system.push(SystemInstruction {
        source: SystemSource::OpenAiDeveloper,
        content: text,
    });
}
"developer" => {
    out.push(Message {
        role: MessageRole::System,
        content: text,
        name: None,
        source: Some(SystemSource::OpenAiDeveloper),
        extensions: ExtensionMap::new(),
    });
}
```

Add `let mut prelude_phase = true;` at the top of each input-walk function. Inside the catch-all "user"/"assistant" arms in those functions, add `prelude_phase = false;` as the first statement.

In the top-level `decode` function, after the `instructions` push, also seed `prelude_phase` correctly. Per spec §3.3, the `instructions` field does NOT change prelude_phase — entries pushed from leading-run input still join the same session-level vec. So `decode_input` continues to start with `prelude_phase = true` regardless of whether `req.instructions` was present.

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo nextest run -p agent-shim-frontends openai_responses::decode
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/frontends/src/openai_responses/decode.rs
git commit -m "feat(frontends/openai_responses): prelude_phase fold across input array

Spec §3.3: top-level instructions content enters the session-level vec
first; leading-run input system/developer entries append after it;
later system/developer entries become positional Message{role:System}."
```

---

## Task 8: Anthropic provider — tests for positional `Message{role:System}` flowing through

**Files:**
- Modify: `crates/providers/src/anthropic/request.rs` (`#[cfg(test)] mod tests` only)

No production code changes: `role_to_anthropic(MessageRole::System) -> "system"` already lands the right wire role thanks to Task 2.

- [ ] **Step 1: Write tests for the new path**

Append to `crates/providers/src/anthropic/request.rs::mod tests`:

```rust
#[test]
fn system_role_message_emits_role_system_in_messages() {
    let mut req = empty_request(false);
    req.messages.push(Message::user(vec![ContentBlock::text("hi")]));
    req.messages.push(Message::system(
        SystemSource::AnthropicSystem,
        vec![ContentBlock::text("hook")],
    ));
    let body = build(&req, &target());
    assert_eq!(body["messages"][1]["role"], "system");
    assert_eq!(body["messages"][1]["content"][0]["type"], "text");
    assert_eq!(body["messages"][1]["content"][0]["text"], "hook");
}

#[test]
fn top_level_system_and_messages_system_both_emit() {
    let mut req = empty_request(false);
    req.system.push(SystemInstruction {
        source: SystemSource::AnthropicSystem,
        content: vec![ContentBlock::text("Standing X")],
    });
    req.messages.push(Message::user(vec![ContentBlock::text("a")]));
    req.messages.push(Message::system(
        SystemSource::AnthropicSystem,
        vec![ContentBlock::text("Mid Y")],
    ));
    req.messages.push(Message::user(vec![ContentBlock::text("b")]));
    let body = build(&req, &target());

    assert_eq!(body["system"], "Standing X");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][1]["role"], "system");
    assert_eq!(body["messages"][1]["content"][0]["text"], "Mid Y");
    assert_eq!(body["messages"][2]["role"], "user");
}
```

- [ ] **Step 2: Run tests**

```bash
cargo nextest run -p agent-shim-providers anthropic::request
```

Expected: PASS. (No production code change required — Task 2 already wired it.)

- [ ] **Step 3: Commit**

```bash
git add crates/providers/src/anthropic/request.rs
git commit -m "test(providers/anthropic): positional Message{role:System} round-trips to role:'system' wire"
```

---

## Task 9: OpenAI Chat wire provider — dispatch `MessageRole::System` per source

**Files:**
- Modify: `crates/providers/src/oai_chat_wire/canonical_to_chat.rs`
- Test: same file's `#[cfg(test)] mod tests`

- [ ] **Step 1: Write failing tests**

Append to `crates/providers/src/oai_chat_wire/canonical_to_chat.rs::mod tests`:

```rust
#[test]
fn chat_system_message_emits_in_position() {
    let mut req = empty_request(false);
    req.messages.push(Message::user(vec![ContentBlock::text("a")]));
    req.messages.push(Message::system(
        SystemSource::OpenAiSystem,
        vec![ContentBlock::text("mid")],
    ));
    req.messages.push(Message::user(vec![ContentBlock::text("b")]));
    let body = build_json(&req, &target(), false);
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[1]["role"], "system");
    assert_eq!(msgs[1]["content"], "mid");
    assert_eq!(msgs[2]["role"], "user");
}

#[test]
fn chat_system_message_with_developer_source_emits_role_developer() {
    let mut req = empty_request(false);
    req.messages.push(Message::user(vec![ContentBlock::text("a")]));
    req.messages.push(Message::system(
        SystemSource::OpenAiDeveloper,
        vec![ContentBlock::text("hint")],
    ));
    let body = build_json(&req, &target(), false);
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs[1]["role"], "developer");
}

#[test]
fn chat_top_level_system_first_then_messages_system_preserves_order() {
    let mut req = empty_request(false);
    req.system.push(SystemInstruction {
        source: SystemSource::OpenAiSystem,
        content: vec![ContentBlock::text("standing")],
    });
    req.messages.push(Message::user(vec![ContentBlock::text("a")]));
    req.messages.push(Message::system(
        SystemSource::OpenAiSystem,
        vec![ContentBlock::text("mid")],
    ));
    req.messages.push(Message::user(vec![ContentBlock::text("b")]));
    let body = build_json(&req, &target(), false);
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "standing");
    assert_eq!(msgs[1]["role"], "user");
    assert_eq!(msgs[2]["role"], "system");
    assert_eq!(msgs[2]["content"], "mid");
    assert_eq!(msgs[3]["role"], "user");
}

#[test]
fn chat_system_message_with_none_source_defaults_to_system_role() {
    let mut req = empty_request(false);
    req.messages.push(Message::user(vec![ContentBlock::text("a")]));
    req.messages.push(Message {
        role: MessageRole::System,
        content: vec![ContentBlock::text("orphan")],
        name: None,
        source: None,
        extensions: ExtensionMap::new(),
    });
    let body = build_json(&req, &target(), false);
    assert_eq!(body["messages"][1]["role"], "system");
}
```

If `empty_request` / `target()` helpers don't exist in this file, search for the analogous test fixtures and add them following the pattern of the existing tests.

- [ ] **Step 2: Run tests to verify they fail (compile error)**

```bash
cargo nextest run -p agent-shim-providers oai_chat_wire::canonical_to_chat
```

Expected: compile error `non-exhaustive patterns: MessageRole::System not covered` on the `match msg.role` block (around line 100).

- [ ] **Step 3: Add the `System` arm**

In `crates/providers/src/oai_chat_wire/canonical_to_chat.rs::build` around line 99, replace:

```rust
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
```

with:

```rust
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
            MessageRole::System => match msg.source {
                Some(SystemSource::OpenAiDeveloper) => "developer",
                // Defaults to "system" for OpenAiSystem, AnthropicSystem, and the
                // documented None fallback (spec §2.6 invariant I1).
                _ => "system",
            },
        };
```

Add `SystemSource` to the top-of-file import list if not already present:

```rust
use agent_shim_core::{
    request::{ReasoningEffort, ResponseFormat},
    BackendTarget, CanonicalRequest, ContentBlock, MessageRole, SystemSource, ToolCallArguments,
    ToolChoice,
};
```

(It is already imported per the grep at plan time — verify.)

`MessageRole::System` messages MUST skip the tool_calls / tool_results branches below. The existing code shape handles this because System messages don't carry ToolCall / ToolResult content blocks; the `tool_calls.is_empty()` and `tool_results.is_empty()` checks naturally route them to the `// Normal message` branch. Verify by reading lines 184-204 — no change needed.

- [ ] **Step 4: Run tests to verify they pass (and confirm derivative providers still compile)**

```bash
cargo nextest run -p agent-shim-providers oai_chat_wire::canonical_to_chat
cargo nextest run -p agent-shim-providers deepseek glm github_copilot
```

Expected: PASS. `deepseek`, `glm`, and `github_copilot` are thin
wrappers over `canonical_to_chat::build` per spec §4.5 / §4.6 — they
get the `MessageRole::System` handling for free; their existing test
suites must remain green.

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/oai_chat_wire/canonical_to_chat.rs
git commit -m "feat(providers/oai_chat_wire): emit positional Message{role:System} per source

Spec §4.2: MessageRole::System maps to wire role 'developer' when
source==OpenAiDeveloper, else 'system'. Inherited by deepseek, glm,
github_copilot providers which wrap canonical_to_chat::build."
```

---

## Task 10: OpenAI Responses provider — collapse positional System into instructions

**Files:**
- Modify: `crates/providers/src/openai_compatible/responses_api/encode_request.rs`
- Test: a new `#[cfg(test)] mod tests` if none exists in this file, otherwise append.

- [ ] **Step 1: Write failing tests**

If there is no `#[cfg(test)] mod tests` block at the end of `encode_request.rs`, create one. Add:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        ids::RequestId,
        message::{Message, MessageRole, SystemInstruction, SystemSource},
        request::{CanonicalRequest, GenerationOptions, RequestMetadata},
        target::{BackendTarget, FrontendInfo, FrontendKind, FrontendModel},
        tool::ToolChoice,
        ContentBlock, ExtensionMap,
    };

    fn req_with_system_only_in_messages() -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: FrontendModel::from("claude-opus-4-8"),
            },
            model: FrontendModel::from("claude-opus-4-8"),
            system: vec![],
            messages: vec![
                Message::user(vec![ContentBlock::text("a")]),
                Message::system(
                    SystemSource::OpenAiSystem,
                    vec![ContentBlock::text("mid hint")],
                ),
                Message::user(vec![ContentBlock::text("b")]),
            ],
            tools: vec![],
            tool_choice: ToolChoice::default(),
            generation: GenerationOptions::default(),
            response_format: None,
            stream: false,
            metadata: RequestMetadata::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: Default::default(),
            extensions: ExtensionMap::new(),
        }
    }

    fn target() -> BackendTarget {
        BackendTarget {
            provider: "openai".into(),
            model: "gpt-5".into(),
            policy: Default::default(),
        }
    }

    #[test]
    fn responses_mid_conv_system_collapses_to_instructions() {
        let req = req_with_system_only_in_messages();
        let body = build(&req, &target());
        assert_eq!(body["instructions"], "mid hint");
        // Input array should NOT contain a system-role item.
        let input = body["input"].as_array().unwrap();
        for item in input {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
            assert!(role != "system" && role != "developer");
        }
    }

    #[test]
    fn responses_top_level_and_mid_conv_systems_concatenate_in_order() {
        let mut req = req_with_system_only_in_messages();
        req.system.push(SystemInstruction {
            source: SystemSource::OpenAiSystem,
            content: vec![ContentBlock::text("standing")],
        });
        let body = build(&req, &target());
        // Top-level system vec entries first, then positional in message order.
        assert_eq!(body["instructions"], "standing\nmid hint");
    }

    #[test]
    fn responses_empty_system_message_text_skipped_in_collapse() {
        let mut req = req_with_system_only_in_messages();
        // Replace the mid system with an empty-text one.
        req.messages[1] = Message::system(SystemSource::OpenAiSystem, vec![]);
        let body = build(&req, &target());
        // Nothing to collapse; instructions absent.
        assert!(body.get("instructions").is_none() || body["instructions"] == "");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail (compile error)**

```bash
cargo nextest run -p agent-shim-providers openai_compatible::responses_api::encode_request
```

Expected: compile error `non-exhaustive patterns: MessageRole::System not covered` on the `match msg.role` block in `build` (around line 31).

- [ ] **Step 3: Update `build` to skip System and collect their text**

In `crates/providers/src/openai_compatible/responses_api/encode_request.rs::build`, modify the messages-loop region. The existing structure is roughly:

```rust
let mut input: Vec<Value> = Vec::new();
for msg in &req.messages {
    let role = match msg.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => { /* function_call_output handling */ continue; }
    };
    // ... rest
}
```

Add a `MessageRole::System` arm that collects text into a side buffer and `continue`s. Replace the role match and add the buffer:

```rust
let mut input: Vec<Value> = Vec::new();
let mut mid_conv_systems: Vec<String> = Vec::new();
for msg in &req.messages {
    let role = match msg.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => {
            // ... existing function_call_output handling unchanged ...
            continue;
        }
        MessageRole::System => {
            let text = extract_text(&msg.content);
            if !text.is_empty() {
                tracing::debug!(
                    source = ?msg.source,
                    "openai_responses provider: mid-conversation system \
                     message collapsed to top-level instructions \
                     (position lost)"
                );
                mid_conv_systems.push(text);
            }
            continue;
        }
    };
    // ... rest unchanged ...
}
```

Then, in the instructions assembly block at the top of `build`, change:

```rust
    let instructions: Vec<String> = req
        .system
        .iter()
        .map(|s| extract_text(&s.content))
        .collect();
    if !instructions.is_empty() {
        body["instructions"] = Value::String(instructions.join("\n"));
    }
```

This block currently runs BEFORE the message loop. Move the `body["instructions"]` assignment to AFTER the message loop and merge in `mid_conv_systems`. Replace the original block with just the collection step:

```rust
    let mut instructions: Vec<String> = req
        .system
        .iter()
        .map(|s| extract_text(&s.content))
        .collect();
```

Then, after the `body["input"] = Value::Array(input);` line (around line 99), insert:

```rust
    instructions.extend(mid_conv_systems);
    if !instructions.is_empty() {
        body["instructions"] = Value::String(instructions.join("\n"));
    }
```

Verify the existing `body["instructions"] = ...` assignment is removed (only the post-message assignment remains).

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo nextest run -p agent-shim-providers openai_compatible::responses_api::encode_request
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/openai_compatible/responses_api/encode_request.rs
git commit -m "feat(providers/openai_responses): collapse positional System into instructions

Spec §4.3: positional Message{role:System} messages do not appear in
the input array; their text is appended to top-level instructions in
message order, after top-level system vec entries. tracing::debug logs
the position loss for diagnosis."
```

---

## Task 11: Gemini provider — collapse positional System into systemInstruction

**Files:**
- Modify: `crates/providers/src/gemini/request.rs`
- Test: same file's `#[cfg(test)] mod tests`

- [ ] **Step 1: Write failing tests**

Append to `crates/providers/src/gemini/request.rs::mod tests`:

```rust
#[test]
fn gemini_mid_conv_system_appended_to_system_instruction() {
    use agent_shim_core::message::{Message, SystemSource};

    let mut req = empty_request(false);
    req.messages.push(Message::user(vec![ContentBlock::text("a")]));
    req.messages.push(Message::system(
        SystemSource::OpenAiSystem,
        vec![ContentBlock::text("mid hint")],
    ));
    req.messages.push(Message::user(vec![ContentBlock::text("b")]));

    let body = build(&req, &target());
    let parts = body["systemInstruction"]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0]["text"], "mid hint");
}

#[test]
fn gemini_mid_conv_system_excluded_from_contents() {
    use agent_shim_core::message::{Message, SystemSource};

    let mut req = empty_request(false);
    req.messages.push(Message::user(vec![ContentBlock::text("a")]));
    req.messages.push(Message::system(
        SystemSource::AnthropicSystem,
        vec![ContentBlock::text("mid")],
    ));
    req.messages.push(Message::user(vec![ContentBlock::text("b")]));

    let body = build(&req, &target());
    let contents = body["contents"].as_array().unwrap();
    for item in contents {
        let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
        assert_ne!(role, "system");
        assert_ne!(role, "developer");
    }
    assert_eq!(contents.len(), 2);
}

#[test]
fn gemini_top_level_and_mid_conv_systems_concatenate_in_order() {
    use agent_shim_core::message::{Message, SystemInstruction, SystemSource};

    let mut req = empty_request(false);
    req.system.push(SystemInstruction {
        source: SystemSource::AnthropicSystem,
        content: vec![ContentBlock::text("standing")],
    });
    req.messages.push(Message::user(vec![ContentBlock::text("a")]));
    req.messages.push(Message::system(
        SystemSource::OpenAiSystem,
        vec![ContentBlock::text("mid")],
    ));
    req.messages.push(Message::user(vec![ContentBlock::text("b")]));

    let body = build(&req, &target());
    let parts = body["systemInstruction"]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["text"], "standing");
    assert_eq!(parts[1]["text"], "mid");
}
```

If `empty_request` / `target` test helpers don't exist for the gemini request tests, add them following the pattern of the other tests in this file.

- [ ] **Step 2: Run tests to verify they fail (compile error)**

```bash
cargo nextest run -p agent-shim-providers gemini::request
```

Expected: compile error `non-exhaustive patterns: MessageRole::System not covered` on `build_contents` (around line 157).

- [ ] **Step 3: Update `build_system_instruction` to also walk messages**

In `crates/providers/src/gemini/request.rs::build_system_instruction` (around line 308), replace:

```rust
fn build_system_instruction(req: &CanonicalRequest) -> Option<Content> {
    if req.system.is_empty() {
        return None;
    }
    let parts: Vec<Part> = req
        .system
        .iter()
        .flat_map(|si| si.content.iter())
        .filter_map(|b| {
            if let ContentBlock::Text(t) = b {
                Some(Part {
                    text: Some(t.text.clone()),
                    ..Default::default()
                })
            } else {
                None
            }
        })
        .collect();

    if parts.is_empty() {
        return None;
    }

    Some(Content {
        role: "user".to_string(),
        parts,
    })
}
```

with:

```rust
fn build_system_instruction(req: &CanonicalRequest) -> Option<Content> {
    let mut parts: Vec<Part> = Vec::new();

    for si in &req.system {
        for block in &si.content {
            if let ContentBlock::Text(t) = block {
                parts.push(Part {
                    text: Some(t.text.clone()),
                    ..Default::default()
                });
            }
        }
    }

    for msg in &req.messages {
        if msg.role == MessageRole::System {
            for block in &msg.content {
                if let ContentBlock::Text(t) = block {
                    tracing::debug!(
                        source = ?msg.source,
                        "gemini provider: mid-conversation system message \
                         collapsed to systemInstruction (position lost)"
                    );
                    parts.push(Part {
                        text: Some(t.text.clone()),
                        ..Default::default()
                    });
                }
            }
        }
    }

    if parts.is_empty() {
        return None;
    }

    Some(Content {
        // Gemini's `systemInstruction` field expects a `Content` shape; the
        // role on it is conventionally "user" (the API ignores it but
        // requires the field to deserialize as `Content`).
        role: "user".to_string(),
        parts,
    })
}
```

- [ ] **Step 4: Update `build_contents` to skip System**

In the same file, in `build_contents` (around line 156), replace:

```rust
    for msg in &req.messages {
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "model",
            MessageRole::Tool => "function",
        };

        let parts: Vec<Part> = msg
            .content
            .iter()
            .filter_map(|b| block_to_part(b, tool_call_name_by_id))
            .collect();

        // Skip empty messages. Gemini rejects `Content` with no parts, and
        // dropping a fully-skipped message is preferable to a 400.
        if parts.is_empty() {
            continue;
        }

        out.push(Content {
            role: role.to_string(),
            parts,
        });
    }
```

with:

```rust
    for msg in &req.messages {
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "model",
            MessageRole::Tool => "function",
            // System messages are already absorbed by
            // build_system_instruction (with debug log noting the
            // positional information loss). Skip here to avoid double
            // emission.
            MessageRole::System => continue,
        };

        let parts: Vec<Part> = msg
            .content
            .iter()
            .filter_map(|b| block_to_part(b, tool_call_name_by_id))
            .collect();

        if parts.is_empty() {
            continue;
        }

        out.push(Content {
            role: role.to_string(),
            parts,
        });
    }
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo nextest run -p agent-shim-providers gemini::request
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/providers/src/gemini/request.rs
git commit -m "feat(providers/gemini): collapse positional System into systemInstruction

Spec §4.4: positional Message{role:System} text is appended to
systemInstruction.parts after top-level system vec entries; contents
loop skips System (already absorbed). tracing::debug logs the position
loss."
```

---

## Task 12: Cross-protocol integration tests

**Files:**
- Modify: `crates/protocol-tests/` — append cross-protocol cases. Likely target:
  - `crates/protocol-tests/tests/responses_to_anthropic_tools.rs` (or a new sibling file).
  - `crates/protocol-tests/tests/responses_to_gemini_tools.rs` (sibling pattern).
  - Or a new file `crates/protocol-tests/tests/positional_system.rs` dedicated to this feature.

Spec §5.4 lists five cross-protocol cases. Create one new test file dedicated to the cross-protocol matrix for positional system, since the existing files focus on tools.

- [ ] **Step 1: Inspect the existing protocol-tests scaffolding**

```bash
ls crates/protocol-tests/tests/
```

Read one existing test (e.g. `responses_to_anthropic_tools.rs`) to learn the harness pattern (how a CanonicalRequest is constructed and how upstream body assertions are made).

- [ ] **Step 2: Create `crates/protocol-tests/tests/positional_system.rs`**

```rust
//! Cross-protocol verification for the 2026-06-10 positional system
//! messages spec.
//!
//! Matrix coverage (per spec §5.4):
//!   * Anthropic-in → Anthropic-out: positional preserved
//!   * Anthropic-in → OpenAI Chat-out: positional preserved
//!   * Anthropic-in → OpenAI Responses-out: downgrade to instructions
//!   * Anthropic-in → Gemini-out: downgrade to systemInstruction
//!   * OpenAI Chat-in → Anthropic-out: positional preserved

use agent_shim_core::{
    ids::RequestId,
    message::{Message, MessageRole, SystemSource},
    request::{CanonicalRequest, GenerationOptions, RequestMetadata},
    target::{BackendTarget, FrontendInfo, FrontendKind, FrontendModel},
    tool::ToolChoice,
    ContentBlock, ExtensionMap,
};

fn canonical_user_system_user(frontend: FrontendKind, source: SystemSource) -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: frontend,
            requested_model: FrontendModel::from("test-model"),
        },
        model: FrontendModel::from("test-model"),
        system: vec![],
        messages: vec![
            Message::user(vec![ContentBlock::text("turn one")]),
            Message::system(source, vec![ContentBlock::text("mid system")]),
            Message::user(vec![ContentBlock::text("turn two")]),
        ],
        tools: vec![],
        tool_choice: ToolChoice::default(),
        generation: GenerationOptions::default(),
        response_format: None,
        stream: false,
        metadata: RequestMetadata::default(),
        inbound_anthropic_headers: vec![],
        resolved_policy: Default::default(),
        extensions: ExtensionMap::new(),
    }
}

fn anthropic_target() -> BackendTarget {
    BackendTarget {
        provider: "anthropic".into(),
        model: "claude-opus-4-8".into(),
        policy: Default::default(),
    }
}

fn openai_target() -> BackendTarget {
    BackendTarget {
        provider: "openai".into(),
        model: "gpt-5".into(),
        policy: Default::default(),
    }
}

fn gemini_target() -> BackendTarget {
    BackendTarget {
        provider: "gemini".into(),
        model: "gemini-2.0-flash".into(),
        policy: Default::default(),
    }
}

#[test]
fn anthropic_in_anthropic_out_preserves_positional_system() {
    let req = canonical_user_system_user(
        FrontendKind::AnthropicMessages,
        SystemSource::AnthropicSystem,
    );
    let body = agent_shim_providers::anthropic::request::build(&req, &anthropic_target());
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[1]["role"], "system");
    assert_eq!(msgs[1]["content"][0]["text"], "mid system");
}

#[test]
fn anthropic_in_openai_chat_out_preserves_positional_system() {
    let req = canonical_user_system_user(
        FrontendKind::AnthropicMessages,
        SystemSource::AnthropicSystem,
    );
    let body = agent_shim_providers::oai_chat_wire::canonical_to_chat::build_json(
        &req,
        &openai_target(),
        false,
    );
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[1]["role"], "system");
    assert_eq!(msgs[1]["content"], "mid system");
}

#[test]
fn anthropic_in_openai_responses_out_downgrades_to_instructions() {
    let req = canonical_user_system_user(
        FrontendKind::AnthropicMessages,
        SystemSource::AnthropicSystem,
    );
    let body = agent_shim_providers::openai_compatible::responses_api::encode_request::build(
        &req,
        &openai_target(),
    );
    assert_eq!(body["instructions"], "mid system");
    // input array carries no system items.
    for item in body["input"].as_array().unwrap() {
        let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
        assert!(role != "system" && role != "developer");
    }
}

#[test]
fn anthropic_in_gemini_out_downgrades_to_system_instruction() {
    let req = canonical_user_system_user(
        FrontendKind::AnthropicMessages,
        SystemSource::AnthropicSystem,
    );
    let body = agent_shim_providers::gemini::request::build(&req, &gemini_target());
    let parts = body["systemInstruction"]["parts"].as_array().unwrap();
    assert!(parts.iter().any(|p| p["text"] == "mid system"));
    for item in body["contents"].as_array().unwrap() {
        let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
        assert_ne!(role, "system");
    }
}

#[test]
fn openai_chat_in_anthropic_out_preserves_positional_system() {
    let req = canonical_user_system_user(
        FrontendKind::OpenAiChat,
        SystemSource::OpenAiSystem,
    );
    let body = agent_shim_providers::anthropic::request::build(&req, &anthropic_target());
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[1]["role"], "system");
    assert_eq!(msgs[1]["content"][0]["text"], "mid system");
}
```

If `crates/protocol-tests/Cargo.toml` does not already depend on `agent-shim-providers` for its `tests/`, add the `dev-dependencies` line — check existing tests first; they almost certainly already have the dep since `responses_to_*_tools.rs` similar tests exist.

If any of the `agent_shim_providers::<crate>::request::build` paths is `pub(crate)` rather than `pub`, locate the public wrapper used by other integration tests in the same directory and use that instead (e.g. `build_json` for the chat wire).

- [ ] **Step 3: Run the cross-protocol tests**

```bash
cargo nextest run -p agent-shim-protocol-tests --test positional_system
```

Expected: PASS for all five.

- [ ] **Step 4: Commit**

```bash
git add crates/protocol-tests/tests/positional_system.rs crates/protocol-tests/Cargo.toml
git commit -m "test(protocol-tests): cross-protocol matrix for positional system messages

Five cases per spec §5.4. Anthropic-out and openai_chat-out preserve
position; openai_responses-out and gemini-out downgrade to top-level
instructions / systemInstruction respectively."
```

---

## Task 13: Reproduce the original bug end-to-end and confirm fix

**Files:**
- No file edits; verification step only.

This is the manual verification step from spec §5.6. Confirms that the original failure is resolved with the implementation in place.

- [ ] **Step 1: Build a release binary**

```bash
cargo build --release -p agent-shim
```

If the build fails with `Access is denied (os error 5)` on Windows because a running `agent-shim.exe` holds the binary, rename per CLAUDE.md:

```powershell
Rename-Item target\release\agent-shim.exe target\release\agent-shim.exe.OLD-by-fix
cargo build --release -p agent-shim
```

- [ ] **Step 2: Locate the bug-trigger dump from the original failure**

The dump that triggered this whole effort is at
`C:\ProgramData\agent-shim\logs\decode-failures\anthropic-messages-13c83365.json`.
Confirm it still exists. Its first ~500 chars contain `"model":"claude-opus-4-8"` and a `messages` array with one `role:"user"` and one `role:"system"`.

- [ ] **Step 3: Replay the dump through the decoder via a one-shot test binary or `curl`**

The cleanest verification path is to start the freshly-built `agent-shim serve` with a route that maps `claude-opus-4-8` to a valid Anthropic upstream (or a mockito stub), then POST the dump body to `/v1/messages`.

If a live Anthropic upstream is not available, write a one-shot Rust test that calls
`agent_shim_frontends::anthropic_messages::decode::decode(&bytes)` on the dump bytes
and asserts `Ok(_)`:

Create a temporary `crates/frontends/tests/regression_opus_4_8_hook_dump.rs`:

```rust
//! Regression: Claude Code → claude-opus-4-8 with SessionStart hook
//! injection in messages[1] used to fail with 400 "unknown role: system".
//! Per spec dated 2026-06-10, the canonical decoder now accepts it.

use std::fs;

#[test]
fn opus_4_8_session_start_hook_dump_decodes() {
    let path = r"C:\ProgramData\agent-shim\logs\decode-failures\anthropic-messages-13c83365.json";
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("regression dump not present at {path}; skipping");
            return;
        }
    };
    let req = agent_shim_frontends::anthropic_messages::decode::decode(&bytes)
        .expect("decoder must accept the SessionStart-hook dump");
    // The dump's messages array is [user, system(hook)]. After decode,
    // since this is a tail-positioned system, prelude_phase ends at
    // messages[0] (user) and messages[1] becomes positional.
    assert_eq!(req.messages.len(), 2);
    assert_eq!(req.messages[0].role, agent_shim_core::MessageRole::User);
    assert_eq!(req.messages[1].role, agent_shim_core::MessageRole::System);
}
```

Run:

```bash
cargo nextest run -p agent-shim-frontends --test regression_opus_4_8_hook_dump
```

Expected: PASS (or `skipped` if the dump is absent in the CI runner). On the local box where the dump exists, this MUST pass.

- [ ] **Step 4: Optionally live-test against Anthropic upstream**

If you have a working `gateway.yaml` routing `claude-opus-4-8` to Anthropic:

```bash
target\release\agent-shim serve --config gateway.yaml
```

Then from another terminal:

```bash
curl -X POST http://127.0.0.1:8080/v1/messages \
  -H "Content-Type: application/json" \
  --data-binary "@C:\ProgramData\agent-shim\logs\decode-failures\anthropic-messages-13c83365.json"
```

Expected: HTTP 200 with a streaming response from Anthropic upstream.
Expected (not): no new file in `C:\ProgramData\agent-shim\logs\decode-failures\` after this request.

- [ ] **Step 5: Commit the regression test**

```bash
git add crates/frontends/tests/regression_opus_4_8_hook_dump.rs
git commit -m "test(regression): claude-opus-4-8 SessionStart hook dump decodes

Pins the original 2026-06-10 bug fix. Skips when the local dump file
is unavailable so CI does not fail; runs on the dev workstation where
the dump lives."
```

---

## Task 14: CHANGELOG entry + final release-gate check

**Files:**
- Modify: `CHANGELOG.md`
- Modify (if exists): `Cargo.toml` `workspace.package.version` bumped to `0.10.0`.

- [ ] **Step 1: Inspect current CHANGELOG and version**

```bash
head -50 CHANGELOG.md
grep -n "^version" Cargo.toml
```

If `CHANGELOG.md` does not exist, create one with the standard "Keep a Changelog" header. If the workspace version is below `0.10.0`, prepare a bump.

- [ ] **Step 2: Bump workspace version to 0.10.0**

In `Cargo.toml` at the workspace root, update `workspace.package.version` from its current value to `"0.10.0"`. Per CLAUDE.md "Conventions" the per-crate `version.workspace = true` inherits automatically.

- [ ] **Step 3: Write the CHANGELOG entry**

Prepend (under `# Changelog`) the `[0.10.0]` section:

````markdown
## [0.10.0] - 2026-06-10

### Added
- Canonical `MessageRole::System` variant authorising positional system
  instructions inside the `messages` array. Authorised by ADR-0011 (iv).
- Canonical `Message.source: Option<SystemSource>` field tagging the
  origin of a `MessageRole::System` message so outbound encoders can
  choose between OpenAI `"system"` and `"developer"` wire roles.
  Authorised by ADR-0011 (v).
- `Message::system(source, content)` constructor.
- ADR-0011 (`docs/adr/0011-canonical-message-additive-discipline.md`)
  amending ADR-0007 with categories (iv) and (v) and clarifying that
  (iii) covers `#[cfg(test)]` item modification.

### Changed (BREAKING — semantic)
- `anthropic_messages` frontend no longer rejects `messages` array
  entries with `role:"system"`. Leading-run entries fold into the
  session-level `system` vec (after any top-level `system` field
  entries); later entries become positional
  `Message{role:System, source:Some(AnthropicSystem)}`.
- `openai_chat` and `openai_responses` frontends preserve
  mid-conversation `system` / `developer` messages instead of folding
  ALL of them into top-level system. Only the leading run folds; later
  entries are positional `Message{role:System, source:Some(...)}`.
- `anthropic` and `oai_chat_wire` providers emit positional
  `Message{role:System}` at the original index inside the outbound
  `messages` array (`role:"system"` or `role:"developer"` per
  `source`).
- `openai_compatible` Responses provider and `gemini` provider collapse
  positional system messages into top-level `instructions` /
  `systemInstruction` with a `tracing::debug` log noting the position
  loss.

### Frozen-core hunk classification (per ADR-0007 §(b) + ADR-0011)

| Hunk | File | Category |
|------|------|----------|
| `MessageRole` adds `System` variant | `crates/core/src/message.rs` | (iv) |
| `Message` adds `source: Option<SystemSource>` + `Message::system(...)` constructor | `crates/core/src/message.rs` | (v) |
| `role_to_anthropic` adds `System => "system"` arm | `crates/core/src/mapping/anthropic_wire.rs` | (iv) — atomic dispatch update per ADR-0011 (iv) invariant 3 |
| `role_from_anthropic` adds `"system" => Some(System)` arm | `crates/core/src/mapping/anthropic_wire.rs` | (iv) — atomic dispatch update per ADR-0011 (iv) invariant 3 |
| `role_unknown_returns_none` test renamed & flipped (now `role_system_string_decodes_to_system_variant`); new `role_unknown_returns_none` against `"human"` | `crates/core/src/mapping/anthropic_wire.rs` | (iii) per ADR-0011 scope clarification |
| `proptest_roundtrip.rs` extends `arb_message_role` to cover `System` | `crates/core/tests/proptest_roundtrip.rs` | (iii) |

### Rollback
Revert to v0.9.x. No data layer migration, no config changes — safe.
````

- [ ] **Step 4: Run the full release gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo deny check
```

Expected: every command exits 0. If `cargo fmt --check` fails, run `cargo fmt --all` and recommit.

- [ ] **Step 5: Commit**

```bash
git add CHANGELOG.md Cargo.toml
git commit -m "chore(release): v0.10.0 — positional system messages

ADR-0011 + canonical Message/MessageRole upgrades + three frontends +
four providers. CHANGELOG carries the full hunk classification table
per ADR-0007 (b)."
```

- [ ] **Step 6: Final verification — `git log` and summary**

```bash
git log --oneline -16
```

Expected last 14-16 commits include: ADR + spec commit, then 14 feature commits Task 1 → Task 14.

---

## Done. Now what?

Open a PR with the branch and link this plan, the spec, and ADR-0011 in the description. The PR title should be `v0.10.0: positional system messages`.

Reviewer focus areas:
- ADR-0011 rule shape (categories iv and v).
- Spec §3 prelude_phase rule uniformity across three frontends.
- Spec §4.3 / §4.4 downgrade order in tests.
- Hunk classification table accuracy.
