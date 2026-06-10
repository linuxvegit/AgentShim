# Positional System Messages — Design

**Date:** 2026-06-10
**Status:** Draft (pending implementation)
**Target release:** v0.10.0

## 1. Goal & Non-Goals

### Goal

Make agent-shim's canonical model correctly express system messages that
appear at any position inside the `messages` array — not just as a top-level
prelude.

Triggering bug: Claude Code, when targeting `claude-opus-4-8`, attaches the
SessionStart hook content as a tail-positioned
`{"role":"system","content":"..."}` entry inside the `messages` array. The
current `anthropic_messages` frontend rejects that role with
`400 unknown role: system` (decode dumped to
`C:\ProgramData\agent-shim\logs\decode-failures\`).

After this change:

- All three inbound frontends (`anthropic_messages`, `openai_chat`,
  `openai_responses`) preserve the position of `system` / `developer`
  messages instead of folding them into a prelude `Vec<SystemInstruction>`.
- All four outbound providers (`anthropic`, `oai_chat_wire::canonical_to_chat`,
  `openai_compatible::responses_api::encode_request`, `gemini`) translate
  those messages into each upstream's native shape — preserving position
  when the upstream supports it, downgrading to top-level
  `instructions` / `systemInstruction` with a `tracing::debug` log otherwise.
- The top-level `CanonicalRequest::system: Vec<SystemInstruction>` field
  keeps its existing semantics: session-level standing instructions with no
  temporal anchor.

### Non-Goals

1. Do NOT implement Anthropic's `mid_conv_system` content block form.
   Anthropic upstream currently accepts both `role:"system"` messages and
   `mid_conv_system` blocks; we pick the former because Claude Code emits
   it that way and it minimises translation.
2. Do NOT expose a canonical-level toggle that lets clients opt out of the
   new semantics. The semantic upgrade ships all at once.
3. Do NOT change how providers render the existing top-level
   `system: Vec<SystemInstruction>` field. Behaviour for that path is
   identical to v0.9.x.
4. Do NOT hard-fail when an upstream cannot represent positional system
   messages. We always downgrade (already decided in brainstorming).
5. Do NOT introduce a route-level feature flag. The upgrade is uniform.
6. Do NOT treat the `count_tokens` endpoint as an independent goal. It
   shares the `anthropic_messages` decoder and is naturally covered.

## 2. Canonical Model Changes

### 2.1 `MessageRole::System`

`crates/core/src/message.rs`:

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

Wire shape: `"system"` (snake_case, consistent with `User` / `Assistant` /
`Tool`). `MessageRole` does not derive `Ord`, so adding a variant is
non-disruptive.

### 2.2 `Message::source`

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
    /// OpenAI upstreams that distinguish them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SystemSource>,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

impl Message {
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

`Message::user` / `Message::assistant` are unchanged; they implicitly set
`source: None`.

Serde compatibility:

- New version reading old wire: `source` defaults to `None`.
- Old version reading new wire: `Message` does not use
  `deny_unknown_fields`, so unknown `source` is silently ignored.

### 2.3 `SystemSource` (unchanged)

Existing enum at `message.rs:50-57` already carries the three variants we
need:

- `AnthropicSystem`
- `OpenAiSystem`
- `OpenAiDeveloper`

### 2.4 `role_from_anthropic` / `role_to_anthropic`

`crates/core/src/mapping/anthropic_wire.rs`:

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

The existing `role_unknown_returns_none` test (which asserts `"system"`
returns `None`) must flip; replace with a test against a truly unknown
role such as `"human"`.

### 2.5 `CanonicalRequest::system` semantics

Unchanged structurally. Restated contract:

> Top-level `system: Vec<SystemInstruction>` expresses **session-level
> standing instructions** — instructions that apply from the start of the
> conversation and to every assistant turn. Position-sensitive system
> instructions live as `Message{role: System, source: Some(...)}` inside
> the `messages` array at the appropriate index.

The decision of which slot a given inbound system instruction lands in is
the frontend decoder's responsibility (Section 3).

### 2.6 Invariants

- **I1**: A `Message{role: System}` MAY carry `source: None`. Outbound
  encoders treat `None` as `OpenAiSystem` (default to `"system"` wire
  role).
- **I2**: `Message{role: System}.content` may contain block kinds other
  than `Text`, but provider encoders render only `Text` blocks and drop
  the rest with a `tracing::debug` log. This matches the existing
  treatment of `Vec<SystemInstruction>::content`.
- **I3**: `Message{role: System}` does not break user/assistant alternation
  rules at the upstream — Anthropic and OpenAI Chat both treat a
  `role:"system"` entry as an independent message type.

### 2.7 Impact

- `crates/core/src/message.rs`: ~10 lines (variant + field + constructor).
- `crates/core/src/mapping/anthropic_wire.rs`: ~4 lines, plus test
  adjustment.
- `crates/core/tests/proptest_roundtrip.rs`: extend generators to cover
  `System`.
- ~30 sites in tests across `frontends` and `providers` use the explicit
  `Message { role, content, name, extensions }` literal and must add
  `source: None`. Pure mechanical fix — exact count is compiler-driven
  after the struct change lands.

## 3. Frontend Decoder Rules

Shared principle:

> For frontends whose protocol uses **only** the `messages` array to carry
> system instructions (OpenAI Chat, OpenAI Responses without
> `instructions`), a leading run of `system` / `developer` messages folds
> into `Vec<SystemInstruction>` (session-level standing). The first
> message that is NOT system/developer ends the prelude. Any later
> system/developer becomes `Message{role: System}` in place.
>
> For frontends that have a **dedicated top-level system field**
> (Anthropic `system`, OpenAI Responses `instructions`), that field
> alone fills the session-level slot. Any system message inside the
> `messages` array is positional from the start — no leading-run
> folding.

"Leading run" definition: the contiguous prefix of system/developer
messages at the start of the `messages` array, before any user / assistant
/ tool message.

### 3.1 `anthropic_messages` frontend

`crates/frontends/src/anthropic_messages/decode.rs::decode_request`.

Input shape: Anthropic Messages has a dedicated top-level `system` field
for session-level standing instructions. The `messages` array
*theoretically* should not contain `role:"system"`, but the upstream
tolerates it and Claude Code emits it that way.

New rule:

1. Top-level `system` field, if present, becomes
   `Vec<SystemInstruction>{source: AnthropicSystem}` (current behaviour).
2. Iterate `messages`:
   - `"system"` → `Message{role: System, source: Some(AnthropicSystem),
     content: decode_content(...)}`.
   - `"user"` / `"assistant"` → existing path.
   - Anything else → `FrontendError::InvalidBody(format!("unknown role:
     {}", role))`.

Note: There is no prelude_phase logic here because the top-level `system`
field already occupies the "session-level" role; a `role:"system"` in the
`messages` array is always positional. If a client puts the standing
instruction in both places, both are faithfully preserved.

`InboundMessageContent` decoding is unchanged — `Text` and `Blocks` both
work; `inbound_block_to_canonical` handles `cache_control` extensions and
block-type variants.

### 3.2 `openai_chat` frontend

`crates/frontends/src/openai_chat/decode.rs::decode`.

Input shape: OpenAI Chat puts all instructions in the `messages` array
under `role:"system"` or `role:"developer"`. No separate top-level system
field.

New rule:

```text
let mut system: Vec<SystemInstruction> = Vec::new();
let mut messages: Vec<Message> = Vec::new();
let mut prelude_phase = true;

for inbound in req.messages {
    let role_class = role_to_canonical(&inbound.role)?;
    match role_class {
        RoleClass::System(source) => {
            if prelude_phase {
                system.push(SystemInstruction { source, content: ... });
            } else {
                messages.push(Message {
                    role: MessageRole::System,
                    source: Some(source),
                    content: ...,
                    name: inbound.name,
                    extensions: ExtensionMap::new(),
                });
            }
        }
        RoleClass::Message(role) => {
            prelude_phase = false;
            messages.push(Message { role, ... });
        }
    }
}
```

Reasoning: The common OpenAI Chat usage `[system, user, assistant, user,
...]` keeps current behaviour — the first `system` lands in the prelude
vec. The `[system, system, user, ...]` "concat-system" pattern also keeps
current behaviour (both folded). The positional pattern `[..., user,
system, user, ...]` is what changes — the inner `system` is preserved as
`Message{role: System}`.

`developer` role: treated identically to `system`. Prelude-positioned
`developer` enters the vec with `source = OpenAiDeveloper`; positional
`developer` enters `messages` with the same source. Outbound chooses
between `"system"` and `"developer"` based on `source`.

### 3.3 `openai_responses` frontend

`crates/frontends/src/openai_responses/decode.rs::decode` plus
`decode_input`.

Input shape: OpenAI Responses has two system entry points:

- Top-level `instructions` string field.
- `input` array entries shaped as
  `{type:"message", role:"system"|"developer", content:[...]}`.

New rule:

1. Top-level `instructions`, if present, becomes
   `Vec<SystemInstruction>{source: OpenAiSystem}` (current behaviour).
2. Iterate `input` with prelude_phase logic identical to `openai_chat`,
   with one twist:
   - **If `req.instructions` is present, prelude_phase starts as
     `false`** — top-level instructions already filled the session-level
     slot, so any `system` in `input` is positional.
   - Otherwise prelude_phase starts as `true` (leading run folds).

### 3.4 `anthropic_messages::count_tokens`

Indirectly calls `decode_request`. No standalone change required; the
decoder upgrade carries through.

### 3.5 Edge cases

| Input | Result |
|---|---|
| Empty system content (`content: []`) | Preserved as `Message{role: System, content: vec![]}`. Provider decides wire shape. |
| Adjacent system messages `[user, system, system, user]` | Each preserved independently; no merging. |
| `[system, system, user]` | Both fold into prelude vec. |
| `[user, system]` (tail system) | `user` into messages, `system` into messages tail. |
| `[system, user, system, user]` | First into prelude vec, second into messages. |
| Anthropic top-level `system` + messages-array `system` | Both preserved; no dedup. |

### 3.6 Impact

| File | Change |
|---|---|
| `anthropic_messages/decode.rs` | Add `MessageRole::System` branch; ~3 unit tests. |
| `openai_chat/decode.rs` | Add `prelude_phase`; adjust 2–3 existing tests; add ~5 new. |
| `openai_responses/decode.rs` | Add `prelude_phase` with `instructions`-aware init; add ~4 new tests. |
| `core/src/mapping/anthropic_wire.rs` | Flip `role_unknown_returns_none` test as noted. |

Net ~150–200 lines across the three frontends (tests included).

## 4. Provider Outbound Rules

Shared principle:

> The rendering of top-level `system: Vec<SystemInstruction>` is
> unchanged. New work covers `Message{role: System}`. The upstream
> decides: native-positional support → emit in place; no support →
> downgrade to top-level instructions / systemInstruction with a
> `tracing::debug` log.

### 4.1 `anthropic` provider

`crates/providers/src/anthropic/request.rs::build`.

Current:

- `build_system(&req.system)` renders the top-level `system` field
  (string or block array).
- `message_to_outgoing` uses `role_to_anthropic(msg.role)` — after the
  Section 2.4 upgrade, `MessageRole::System` becomes `"system"`
  automatically.
- `content_block_to_outgoing` already handles Text/ToolCall/ToolResult/
  Image/Reasoning.

New work: effectively zero in production code. `MessageRole::System`
flowing through `role_to_anthropic` is enough — the message renders as
`{"role":"system","content":[...]}` in the outgoing `messages` array, and
Anthropic upstream accepts it.

New tests:

- `system_role_message_emits_role_system_in_messages` — canonical
  `Message{role: System, source: Some(AnthropicSystem), content: [text]}`
  → outgoing `messages[?]` is `{"role":"system","content":[{"type":"text",
  "text":"..."}]}`.
- `top_level_system_and_messages_system_both_emit` — both populated → top
  `system` field plus a `messages[?]` `role:"system"`.

### 4.2 `oai_chat_wire::canonical_to_chat`

`crates/providers/src/oai_chat_wire/canonical_to_chat.rs::build`.

Current:

- Lines 82–96 render all `req.system` as messages at the **front** of the
  array.
- Lines 99–104 only know `User` / `Assistant` / `Tool`. `System` must be
  added.

New work:

```rust
for msg in &req.messages {
    let role = match msg.role {
        MessageRole::User      => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool      => "tool",
        MessageRole::System    => match msg.source {
            Some(SystemSource::OpenAiDeveloper) => "developer",
            _                                   => "system",
        },
    };
    // System messages skip tool_calls / tool_results paths.
}
```

System message content uses `build_content_value` /
`build_text_content_value` like a normal message. Only Text blocks are
expected; other blocks fall through current code paths (effectively
dropped).

Fallback for `source: None`: render `"system"` (per Invariant I1).

New tests:

- `system_message_emits_in_position` — canonical `messages[1].role ==
  System` → outgoing `messages[?]` at the correct index.
- `system_message_with_developer_source_emits_role_developer`.
- `top_level_system_first_then_messages_system_preserves_order` —
  canonical `system: [SI("standing")]` + `messages: [user_1,
  Message{System, "middle"}, user_2]` → outgoing
  `[system_msg("standing"), user_1, system_msg("middle"), user_2]`.
- `system_message_with_none_source_defaults_to_system_role`.

### 4.3 `openai_compatible::responses_api::encode_request`

`crates/providers/src/openai_compatible/responses_api/encode_request.rs::build`.

Current:

- Lines 18–25 join `req.system` into the top-level `instructions` string
  (using `\n`).
- Lines 30–97 only know `User` / `Assistant` / `Tool`.

New work: Responses API has no positional system shape inside `input`.
Downgrade: append the System message's text to `instructions`.

```rust
let mut mid_conv_systems: Vec<String> = Vec::new();
for msg in &req.messages {
    if msg.role == MessageRole::System {
        let text = extract_text(&msg.content);
        if !text.is_empty() {
            mid_conv_systems.push(text);
            tracing::debug!(
                "openai_responses provider: mid-conversation system message \
                 collapsed to top-level instructions (position lost)"
            );
        }
        continue;
    }
    // ... existing per-message logic
}

let mut all_instructions: Vec<String> = req
    .system
    .iter()
    .map(|s| extract_text(&s.content))
    .collect();
all_instructions.extend(mid_conv_systems);
if !all_instructions.is_empty() {
    body["instructions"] = Value::String(all_instructions.join("\n"));
}
```

Order: top-level `system` vec entries first, then positional System
messages in their `messages`-array order. This places the "standing"
instruction ahead of any "mid-conversation update", matching typical
override semantics.

`source` is ignored — Responses API `instructions` has no
system/developer distinction. The debug log carries `source` for
diagnosis.

New tests:

- `mid_conv_system_collapses_to_instructions`.
- `top_level_and_mid_conv_systems_concatenate_in_order`.
- `mid_conv_system_emits_debug_log` (captured via a tracing fixture).

### 4.4 `gemini` provider

`crates/providers/src/gemini/request.rs::build` plus
`build_system_instruction` (line 308+).

Current:

- `build_system_instruction(req)` aggregates `req.system` into one
  `Content`, hung on top-level `systemInstruction`.
- Message iteration elsewhere does not know `MessageRole::System`.

New work: same approach as Responses provider — downgrade to top-level
`systemInstruction`.

```rust
fn build_system_instruction(req: &CanonicalRequest) -> Option<Content> {
    let mut parts: Vec<Part> = Vec::new();
    for si in &req.system {
        for block in &si.content {
            if let ContentBlock::Text(t) = block {
                parts.push(Part::text(&t.text));
            }
        }
    }
    for msg in &req.messages {
        if msg.role == MessageRole::System {
            for block in &msg.content {
                if let ContentBlock::Text(t) = block {
                    parts.push(Part::text(&t.text));
                    tracing::debug!(
                        "gemini provider: mid-conversation system message \
                         collapsed to systemInstruction (position lost)"
                    );
                }
            }
        }
    }
    if parts.is_empty() { None } else { Some(Content { role: None, parts }) }
}
```

`contents` iteration skips `MessageRole::System` — those messages are
already absorbed by `build_system_instruction`.

New tests:

- `mid_conv_system_appended_to_system_instruction`.
- `mid_conv_system_excluded_from_contents`.
- `top_level_and_mid_conv_systems_concatenate_in_order`.

### 4.5 Derivative providers (`deepseek`, `glm`)

Both are thin wrappers around `oai_chat_wire::canonical_to_chat::build`:

- `crates/providers/src/deepseek/request.rs:34` — direct call.
- `crates/providers/src/glm/request.rs:27` — calls `build_json` wrapper.

Section 4.2's upgrade covers both automatically. No additional work.

### 4.6 `github_copilot` provider

`github_copilot` also rides the `canonical_to_chat::build` path. Covered
by 4.2. Note: when Copilot internally re-routes to an Anthropic-style
backend (e.g. Vertex Anthropic), tolerance for positional `role:"system"`
in `messages` depends on Copilot itself. We do not handle that secondary
routing here — if Copilot rejects it, the 4xx surfaces to the client.

### 4.7 Impact summary

| Provider | Code | Tests |
|---|---|---|
| anthropic | ~0 | ~30 |
| oai_chat_wire | ~15 | ~50 |
| openai_compat responses | ~20 | ~40 |
| gemini | ~20 | ~30 |
| deepseek / glm / github_copilot | 0 | 0 |

Combined: ~200–250 lines across the four providers (tests included).

## 5. Tests & Verification

### 5.1 Core unit tests

`crates/core/src/message.rs`:

- `system_message_serializes_with_role_system`.
- `message_without_source_round_trips`.
- `system_role_skip_serializing_when_no_source`.

`crates/core/src/mapping/anthropic_wire.rs`:

- Flip `role_unknown_returns_none` → `role_system_returns_system_role`,
  add a new `role_unknown_returns_none` using `"human"`.
- `role_round_trip_system`.

`crates/core/tests/proptest_roundtrip.rs`:

- Extend `arb_message_role` to include `System`; when `System` is
  generated, pair with `arb_system_source`.
- The round-trip invariant `CanonicalRequest → JSON → CanonicalRequest`
  covers `System` automatically.

### 5.2 Frontend decode tests

`anthropic_messages/decode.rs`:

- `decode_system_role_in_messages_yields_system_message`.
- `decode_system_role_at_start_of_messages` — Anthropic frontend does NOT
  fold to top vec; the entry stays at `messages[0]`.
- `decode_system_with_blocks_content_yields_text_blocks`.
- `decode_top_level_system_and_messages_system_coexist`.
- Delete existing `decode_bad_role_is_rejected` (which asserts `"system"`
  is rejected); add `decode_truly_unknown_role_is_rejected` against
  `"human"`.

`openai_chat/decode.rs`:

- `decode_first_system_folds_to_top_level`.
- `decode_mid_system_preserved_in_messages`.
- `decode_consecutive_prelude_systems_all_in_top_level`.
- `decode_developer_at_start_folds`,
  `decode_developer_in_middle_preserved_with_source`.
- `decode_system_after_assistant_preserved`.

`openai_responses/decode.rs`:

- `decode_input_first_system_folds_to_top_level_when_no_instructions`.
- `decode_input_system_preserved_when_instructions_set` — verifies the
  prelude_phase = false start when `req.instructions` is present.
- `decode_input_mid_system_preserved`.
- `decode_input_developer_round_trips_with_source`.

### 5.3 Provider encode tests

`anthropic/request.rs`:

- `system_role_message_emits_role_system_in_messages`.
- `top_level_system_and_messages_system_both_emit`.

`oai_chat_wire/canonical_to_chat.rs`:

- `system_message_emits_in_position`.
- `system_message_with_developer_source_emits_role_developer`.
- `top_level_system_first_then_messages_system_preserves_order`.
- `system_message_with_none_source_defaults_to_system_role`.

`openai_compatible/responses_api/encode_request.rs`:

- `mid_conv_system_collapses_to_instructions`.
- `top_level_and_mid_conv_systems_concatenate_in_order`.
- `mid_conv_system_emits_debug_log`.

`gemini/request.rs`:

- `mid_conv_system_appended_to_system_instruction`.
- `mid_conv_system_excluded_from_contents`.
- `top_level_and_mid_conv_systems_concatenate_in_order`.

### 5.4 Integration / E2E

`crates/protocol-tests/`:

- Anthropic-in → Anthropic-out: positional system preserved.
- Anthropic-in → OpenAI Chat-out (e.g. Copilot): positional system
  preserved.
- Anthropic-in → OpenAI Responses-out: positional system downgrades into
  `instructions`.
- Anthropic-in → Gemini-out: positional system downgrades into
  `systemInstruction`.
- OpenAI Chat-in → Anthropic-out: positional system preserved.

Each case fixes a minimal three-message request (user + system + user)
and asserts the outbound wire shape.

### 5.5 Regression rewrites

- Existing `decode_bad_role_is_rejected` (asserts `system` is rejected)
  MUST be rewritten — otherwise a false-positive regression alarm.
- Existing `role_unknown_returns_none` MUST be rewritten.
- `message_round_trips` and `message_name_field_round_trips` — audit
  for `source` interaction; expected to pass unchanged.
- `grep -rn "Message {$" crates/` (approx) — all explicit struct literal
  constructions of `Message` need `source: None` added. The exact count
  is best discovered by the compile error driver after Section 2's struct
  change lands; all such sites are in test code.

### 5.6 Manual verification

After the change, reproduce the original failure scenario:

1. Launch `agent-shim serve`.
2. From Claude Code, target `claude-opus-4-8` with any prompt
   (SessionStart hook injection brings the offending message in).
3. Expect: request succeeds; upstream Anthropic returns a normal
   response; no new dump file under
   `C:\ProgramData\agent-shim\logs\decode-failures\`.
4. Inspect `tracing` output: no `mid-conversation system` debug log when
   routing to Anthropic upstream (positional preserved); debug log
   present if routing to Responses/Gemini (downgrade).

### 5.7 CI gate

`cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`,
`cargo nextest run --workspace`, `cargo deny check` — all must pass.

### 5.8 Failure modes & diagnostics

- The existing `decode-failures/` dump mechanism in
  `crates/gateway/src/pipeline.rs` continues to operate; it just records
  rarer cases now (no longer the common `unknown role: system`).
- If an upstream returns 4xx after the change, `tracing` logs identify
  which System message was rendered where.

## 6. Risks, Rollout & Open Questions

### 6.1 Risks

- **R1**: Clients relying on "mid-conversation system silently folded".
  Low likelihood — that dependency is itself an anti-pattern. CHANGELOG
  flags as semantic upgrade.
- **R2**: ~30 explicit `Message { ... }` struct literals across tests
  break compilation until `source: None` is added. Pure mechanical fix;
  best done in a dedicated first commit.
- **R3**: Future Anthropic upstream behaviour change (hard-fail on
  `role:"system"`). Not introduced by this spec — clients hit the same
  if they bypassed agent-shim. Mitigation deferred to a follow-up that
  emits `mid_conv_system` content blocks instead.
- **R4**: Responses / Gemini downgrade loses position. `tracing::debug`
  makes this observable; clients can avoid via route config.
- **R5**: `prelude_phase` rule changes behaviour for OpenAI Chat clients
  emitting `[system_A, user, system_B, user]` — previously both folded,
  now `B` stays positional. Section 3.2 explicitly covers this.
- **R6**: `count_tokens` totals shift slightly because the System
  message's text now counts within `messages` rather than the top vec.
  Text is the same; counts should be effectively unchanged.

### 6.2 Rollout

- **Version**: v0.10.0 (minor bump). CHANGELOG entry marked
  `BREAKING (semantic)`.
- **Steps**:
  1. Single PR implements canonical + three frontends + four providers +
     all tests + CLAUDE.md doc updates.
  2. Merge → release v0.10.0.
  3. CHANGELOG entries:
     - `BREAKING (semantic)`: messages array `role:"system"` no longer
       rejected by Anthropic frontend; preserved as positional
       `Message{role: System}`.
     - `BREAKING (semantic)`: OpenAI Chat / Responses frontends preserve
       mid-conversation `system` / `developer` messages instead of
       folding all of them to top-level system; only the leading run is
       folded.
     - `feat`: outbound providers route positional system messages back
       to upstream-native shape; OpenAI Responses / Gemini collapse with
       debug logging.
- **Rollback**: revert to v0.9.x. No data layer migration, no config
  changes — safe rollback.

### 6.3 Open Questions (decided inline)

- **OQ1**: When top-level system vec and `messages`-array System coexist
  in OpenAI Chat output, which comes first?
  → **Vec entries first, then positional in original order.**
- **OQ2**: How does the provider handle non-Text content blocks inside a
  System message?
  → **Render only Text; drop others with `tracing::debug`.**
- **OQ3**: Add a metric for "downgraded mid-conv system" count?
  → **No. `tracing::debug` is sufficient for now.**
- **OQ4**: Dedup when Anthropic top-level `system` field and
  `messages[0] role:"system"` carry identical text?
  → **No. Preserve both faithfully.**
- **OQ5**: Should System messages participate in prompt compression?
  → **Treat as regular messages; revisit if operations report
  mis-compression.**
