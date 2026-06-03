//! Canonical lifecycle validator — see `CONTEXT.md` "Canonical lifecycle".
//!
//! Test-only. Walks an in-memory `&[StreamEvent]` sequence and panics on the
//! first violation of the ordering contract every `CanonicalStream` must
//! satisfy. The panic message names the failing event index and the rule, so
//! a failing test points operators directly at the broken parser / encoder
//! step instead of asking them to read the whole event list by eye.
//!
//! Used by every Provider parser test and every Frontend encoder test as a
//! cheap structural check that runs *before* the test's own per-event
//! assertions. When a future cross-dialect harness lands, it will use the
//! same function.
//!
//! Rule numbers below match the table in the design doc / grilling session
//! (PR-B1). Rules 1-5 are mandatory; rules 6-8 default to enabled and can be
//! disabled via `StrictRules` for parsers that legitimately emit a partial
//! sequence (mid-stream snapshots, property tests).
//!
//! `Error` and `RawProviderEvent` get special treatment:
//! - `Error` is a terminal event — validation stops there, the rest of the
//!   sequence is whatever the parser chose to emit on the way down.
//! - `RawProviderEvent` is a band-side signal, like `UsageDelta` — it does
//!   not touch the block state machine and is exempt from rule 3.

use std::collections::{HashMap, HashSet};

use agent_shim_core::{ContentBlockKind, StreamEvent};

/// Strict-mode toggles. Each field defaults to `true`. Set one to `false`
/// when validating a partial or otherwise loosened sequence.
#[derive(Debug, Clone, Copy)]
pub struct StrictRules {
    /// Rule 6: `MessageStart` must appear before the first `ContentBlockStart`.
    pub require_message_start_before_blocks: bool,
    /// Rule 7: at most one `ResponseStart` and at most one `MessageStart`.
    pub require_single_start: bool,
    /// Rule 8: nothing may follow `ResponseStop`.
    pub require_response_stop_last: bool,
}

impl Default for StrictRules {
    fn default() -> Self {
        Self {
            require_message_start_before_blocks: true,
            require_single_start: true,
            require_response_stop_last: true,
        }
    }
}

/// Panic if `events` violates the canonical lifecycle contract under
/// default strict rules. Equivalent to
/// `assert_canonical_lifecycle_strict(events, StrictRules::default())`.
pub fn assert_canonical_lifecycle(events: &[StreamEvent]) {
    assert_canonical_lifecycle_strict(events, StrictRules::default());
}

/// Panic if `events` violates the canonical lifecycle contract under the
/// given `strict` toggles. See module doc for rule numbering.
pub fn assert_canonical_lifecycle_strict(events: &[StreamEvent], strict: StrictRules) {
    let mut state = ValidatorState::default();

    for (i, event) in events.iter().enumerate() {
        if state.terminated_by_error {
            // Once an upstream/decode `Error` is emitted, stop validating —
            // the parser is allowed to wind down however makes sense.
            return;
        }

        match event {
            StreamEvent::Error { .. } => {
                state.terminated_by_error = true;
            }

            StreamEvent::ResponseStart { .. } => {
                if state.seen_response_start && strict.require_single_start {
                    panic_at(i, event, "rule 7: duplicate ResponseStart");
                }
                state.seen_response_start = true;
            }

            StreamEvent::MessageStart { .. } => {
                check_post_message_stop(i, event, &state);
                check_post_response_stop(i, event, &state, strict);
                if state.seen_message_start && strict.require_single_start {
                    panic_at(i, event, "rule 7: duplicate MessageStart");
                }
                state.seen_message_start = true;
            }

            StreamEvent::ContentBlockStart { index, kind } => {
                check_post_message_stop(i, event, &state);
                check_post_response_stop(i, event, &state, strict);
                if strict.require_message_start_before_blocks && !state.seen_message_start {
                    panic_at(i, event, "rule 6: ContentBlockStart before MessageStart");
                }
                if state.open_blocks.contains_key(index) {
                    panic_at(
                        i,
                        event,
                        &format!(
                            "rule 1a: ContentBlockStart for index {index} which is already open"
                        ),
                    );
                }
                if state.closed_indices.contains(index) {
                    panic_at(
                        i,
                        event,
                        &format!("rule 1b: ContentBlockStart reuses already-closed index {index}"),
                    );
                }
                state.open_blocks.insert(*index, *kind);
            }

            StreamEvent::ContentBlockStop { index } => {
                check_post_message_stop(i, event, &state);
                check_post_response_stop(i, event, &state, strict);
                let Some(kind) = state.open_blocks.remove(index) else {
                    panic_at(
                        i,
                        event,
                        &format!("rule 1c: ContentBlockStop for index {index} which is not open"),
                    );
                };
                if kind == ContentBlockKind::ToolCall && !state.tool_stopped.remove(index) {
                    panic_at(
                        i,
                        event,
                        &format!(
                            "rule 4: ContentBlockStop on ToolCall block {index} without preceding ToolCallStop"
                        ),
                    );
                }
                state.closed_indices.insert(*index);
            }

            StreamEvent::TextDelta { index, .. } => {
                check_post_message_stop(i, event, &state);
                check_post_response_stop(i, event, &state, strict);
                check_delta_kind(
                    i,
                    event,
                    &state,
                    *index,
                    ContentBlockKind::Text,
                    "TextDelta",
                );
            }

            StreamEvent::ReasoningDelta { index, .. } => {
                check_post_message_stop(i, event, &state);
                check_post_response_stop(i, event, &state, strict);
                check_delta_kind(
                    i,
                    event,
                    &state,
                    *index,
                    ContentBlockKind::Reasoning,
                    "ReasoningDelta",
                );
            }

            StreamEvent::ToolCallStart { index, .. } => {
                check_post_message_stop(i, event, &state);
                check_post_response_stop(i, event, &state, strict);
                check_delta_kind(
                    i,
                    event,
                    &state,
                    *index,
                    ContentBlockKind::ToolCall,
                    "ToolCallStart",
                );
            }

            StreamEvent::ToolCallArgumentsDelta { index, .. } => {
                check_post_message_stop(i, event, &state);
                check_post_response_stop(i, event, &state, strict);
                check_delta_kind(
                    i,
                    event,
                    &state,
                    *index,
                    ContentBlockKind::ToolCall,
                    "ToolCallArgumentsDelta",
                );
            }

            StreamEvent::ToolCallStop { index } => {
                check_post_message_stop(i, event, &state);
                check_post_response_stop(i, event, &state, strict);
                // Must be on an open ToolCall block.
                check_delta_kind(
                    i,
                    event,
                    &state,
                    *index,
                    ContentBlockKind::ToolCall,
                    "ToolCallStop",
                );
                state.tool_stopped.insert(*index);
            }

            StreamEvent::MessageStop { .. } => {
                check_post_response_stop(i, event, &state, strict);
                if state.seen_message_stop && strict.require_single_start {
                    // Same toggle gates "exactly one of each start/stop" —
                    // a second MessageStop is symmetric to a second
                    // MessageStart and is covered by the same strictness.
                    panic_at(i, event, "rule 7: duplicate MessageStop");
                }
                state.seen_message_stop = true;
            }

            StreamEvent::ResponseStop { .. } => {
                if !state.seen_message_stop {
                    panic_at(i, event, "rule 2: ResponseStop before MessageStop");
                }
                if state.seen_response_stop && strict.require_single_start {
                    panic_at(i, event, "rule 7: duplicate ResponseStop");
                }
                state.seen_response_stop = true;
            }

            // Out-of-band signals — exempt from rule 3 and don't touch state.
            StreamEvent::UsageDelta { .. } | StreamEvent::RawProviderEvent(_) => {
                check_post_response_stop(i, event, &state, strict);
            }
        }
    }

    // Rule 1d: every opened block must be closed.
    if !state.open_blocks.is_empty() {
        let mut idxs: Vec<u32> = state.open_blocks.keys().copied().collect();
        idxs.sort_unstable();
        panic!(
            "canonical lifecycle: stream ended with {} unclosed block(s): {:?}",
            idxs.len(),
            idxs
        );
    }
}

#[derive(Default)]
struct ValidatorState {
    seen_response_start: bool,
    seen_message_start: bool,
    seen_message_stop: bool,
    seen_response_stop: bool,
    /// Map index → kind for currently-open blocks.
    open_blocks: HashMap<u32, ContentBlockKind>,
    /// Tool block indices that have received `ToolCallStop` and are awaiting
    /// `ContentBlockStop`. Drained on the matching `ContentBlockStop`.
    tool_stopped: HashSet<u32>,
    /// Indices that have already been closed — used to detect rule 1b
    /// (index reuse after close).
    closed_indices: HashSet<u32>,
    /// Set when an `Error` event terminates validation early.
    terminated_by_error: bool,
}

fn check_post_message_stop(i: usize, event: &StreamEvent, state: &ValidatorState) {
    if state.seen_message_stop {
        panic_at(
            i,
            event,
            &format!(
                "rule 3: {} after MessageStop is forbidden (only UsageDelta, RawProviderEvent, and ResponseStop may follow)",
                variant_name(event)
            ),
        );
    }
}

fn check_post_response_stop(
    i: usize,
    event: &StreamEvent,
    state: &ValidatorState,
    strict: StrictRules,
) {
    if strict.require_response_stop_last && state.seen_response_stop {
        panic_at(
            i,
            event,
            &format!("rule 8: {} after ResponseStop", variant_name(event)),
        );
    }
}

fn check_delta_kind(
    i: usize,
    event: &StreamEvent,
    state: &ValidatorState,
    index: u32,
    expected: ContentBlockKind,
    event_name: &str,
) {
    match state.open_blocks.get(&index) {
        Some(kind) if *kind == expected => {}
        Some(other) => panic_at(
            i,
            event,
            &format!(
                "rule 5: {event_name} on index {index} whose open block is {other:?}, expected {expected:?}"
            ),
        ),
        None => panic_at(
            i,
            event,
            &format!("rule 5: {event_name} on index {index} which is not open"),
        ),
    }
}

fn variant_name(event: &StreamEvent) -> &'static str {
    match event {
        StreamEvent::ResponseStart { .. } => "ResponseStart",
        StreamEvent::MessageStart { .. } => "MessageStart",
        StreamEvent::ContentBlockStart { .. } => "ContentBlockStart",
        StreamEvent::TextDelta { .. } => "TextDelta",
        StreamEvent::ReasoningDelta { .. } => "ReasoningDelta",
        StreamEvent::ToolCallStart { .. } => "ToolCallStart",
        StreamEvent::ToolCallArgumentsDelta { .. } => "ToolCallArgumentsDelta",
        StreamEvent::ToolCallStop { .. } => "ToolCallStop",
        StreamEvent::ContentBlockStop { .. } => "ContentBlockStop",
        StreamEvent::UsageDelta { .. } => "UsageDelta",
        StreamEvent::MessageStop { .. } => "MessageStop",
        StreamEvent::ResponseStop { .. } => "ResponseStop",
        StreamEvent::Error { .. } => "Error",
        StreamEvent::RawProviderEvent(_) => "RawProviderEvent",
    }
}

fn panic_at(index: usize, event: &StreamEvent, message: &str) -> ! {
    panic!(
        "canonical lifecycle violation at event #{index} ({}): {message}",
        variant_name(event)
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{MessageRole, ResponseId, StopReason, ToolCallId, Usage};

    // ── Helpers ─────────────────────────────────────────────────────────────

    fn response_start() -> StreamEvent {
        StreamEvent::ResponseStart {
            id: ResponseId("resp_1".into()),
            model: "test-model".into(),
            created_at_unix: 1_700_000_000,
        }
    }

    fn message_start() -> StreamEvent {
        StreamEvent::MessageStart {
            role: MessageRole::Assistant,
        }
    }

    fn block_start(index: u32, kind: ContentBlockKind) -> StreamEvent {
        StreamEvent::ContentBlockStart { index, kind }
    }

    fn block_stop(index: u32) -> StreamEvent {
        StreamEvent::ContentBlockStop { index }
    }

    fn text_delta(index: u32, text: &str) -> StreamEvent {
        StreamEvent::TextDelta {
            index,
            text: text.into(),
        }
    }

    fn reasoning_delta(index: u32, text: &str) -> StreamEvent {
        StreamEvent::ReasoningDelta {
            index,
            text: text.into(),
        }
    }

    fn tool_start(index: u32) -> StreamEvent {
        StreamEvent::ToolCallStart {
            index,
            id: ToolCallId::from_provider("call_1"),
            name: "f".into(),
        }
    }

    fn tool_args_delta(index: u32, frag: &str) -> StreamEvent {
        StreamEvent::ToolCallArgumentsDelta {
            index,
            json_fragment: frag.into(),
        }
    }

    fn tool_stop(index: u32) -> StreamEvent {
        StreamEvent::ToolCallStop { index }
    }

    fn message_stop() -> StreamEvent {
        StreamEvent::MessageStop {
            stop_reason: StopReason::EndTurn,
            stop_sequence: None,
        }
    }

    fn response_stop() -> StreamEvent {
        StreamEvent::ResponseStop { usage: None }
    }

    fn usage_delta() -> StreamEvent {
        StreamEvent::UsageDelta {
            usage: Usage::default(),
        }
    }

    // ── Happy paths ─────────────────────────────────────────────────────────

    #[test]
    fn text_only_stream_passes() {
        let events = vec![
            response_start(),
            message_start(),
            block_start(0, ContentBlockKind::Text),
            text_delta(0, "hello "),
            text_delta(0, "world"),
            block_stop(0),
            message_stop(),
            response_stop(),
        ];
        assert_canonical_lifecycle(&events);
    }

    #[test]
    fn mixed_text_reasoning_tool_passes() {
        let events = vec![
            response_start(),
            message_start(),
            block_start(0, ContentBlockKind::Reasoning),
            reasoning_delta(0, "thinking..."),
            block_stop(0),
            block_start(1, ContentBlockKind::Text),
            text_delta(1, "answer"),
            block_stop(1),
            block_start(2, ContentBlockKind::ToolCall),
            tool_start(2),
            tool_args_delta(2, r#"{"x":1}"#),
            tool_stop(2),
            block_stop(2),
            message_stop(),
            usage_delta(), // out-of-band, allowed after MessageStop
            response_stop(),
        ];
        assert_canonical_lifecycle(&events);
    }

    #[test]
    fn error_event_terminates_validation_early() {
        let events = vec![
            response_start(),
            message_start(),
            block_start(0, ContentBlockKind::Text),
            text_delta(0, "partial"),
            StreamEvent::Error {
                message: "upstream connection reset".into(),
            },
            // The following deliberately-malformed events should be ignored
            // because validation stopped at the Error.
            block_stop(99),
            text_delta(99, "ghost"),
        ];
        assert_canonical_lifecycle(&events);
    }

    // ── Rule 1: pairing & no reuse ──────────────────────────────────────────

    #[test]
    #[should_panic(expected = "rule 1a")]
    fn rule_1a_double_open_same_index() {
        let events = vec![
            response_start(),
            message_start(),
            block_start(0, ContentBlockKind::Text),
            block_start(0, ContentBlockKind::Text),
        ];
        assert_canonical_lifecycle(&events);
    }

    #[test]
    #[should_panic(expected = "rule 1b")]
    fn rule_1b_index_reuse_after_close() {
        let events = vec![
            response_start(),
            message_start(),
            block_start(0, ContentBlockKind::Text),
            block_stop(0),
            block_start(0, ContentBlockKind::Reasoning),
        ];
        assert_canonical_lifecycle(&events);
    }

    #[test]
    #[should_panic(expected = "rule 1c")]
    fn rule_1c_stop_unopened_block() {
        let events = vec![response_start(), message_start(), block_stop(0)];
        assert_canonical_lifecycle(&events);
    }

    #[test]
    #[should_panic(expected = "stream ended with 1 unclosed block(s)")]
    fn rule_1d_unclosed_block_at_eof() {
        let events = vec![
            response_start(),
            message_start(),
            block_start(0, ContentBlockKind::Text),
            text_delta(0, "no stop"),
            message_stop(),
            response_stop(),
        ];
        assert_canonical_lifecycle(&events);
    }

    // ── Rule 2: ResponseStop ordering ───────────────────────────────────────

    #[test]
    #[should_panic(expected = "rule 2: ResponseStop before MessageStop")]
    fn rule_2_response_stop_before_message_stop() {
        let events = vec![response_start(), message_start(), response_stop()];
        assert_canonical_lifecycle(&events);
    }

    // ── Rule 3: nothing after MessageStop except UsageDelta/RawProviderEvent/ResponseStop ──

    #[test]
    #[should_panic(expected = "rule 3: TextDelta after MessageStop")]
    fn rule_3_text_delta_after_message_stop() {
        let events = vec![
            response_start(),
            message_start(),
            block_start(0, ContentBlockKind::Text),
            block_stop(0),
            message_stop(),
            text_delta(0, "too late"),
        ];
        assert_canonical_lifecycle(&events);
    }

    #[test]
    fn rule_3_usage_delta_after_message_stop_is_allowed() {
        let events = vec![
            response_start(),
            message_start(),
            message_stop(),
            usage_delta(),
            response_stop(),
        ];
        assert_canonical_lifecycle(&events);
    }

    // ── Rule 4: ToolCallStop precedes ContentBlockStop on tool blocks ───────

    #[test]
    #[should_panic(expected = "rule 4")]
    fn rule_4_tool_block_close_without_tool_call_stop() {
        let events = vec![
            response_start(),
            message_start(),
            block_start(0, ContentBlockKind::ToolCall),
            tool_start(0),
            tool_args_delta(0, "{}"),
            // Missing: tool_stop(0)
            block_stop(0),
        ];
        assert_canonical_lifecycle(&events);
    }

    // ── Rule 5: delta kind matches open block kind ──────────────────────────

    #[test]
    #[should_panic(expected = "rule 5: TextDelta on index 0 whose open block is Reasoning")]
    fn rule_5_text_delta_on_reasoning_block() {
        let events = vec![
            response_start(),
            message_start(),
            block_start(0, ContentBlockKind::Reasoning),
            text_delta(0, "wrong kind"),
        ];
        assert_canonical_lifecycle(&events);
    }

    #[test]
    #[should_panic(expected = "rule 5: TextDelta on index 7 which is not open")]
    fn rule_5_text_delta_on_unknown_index() {
        let events = vec![response_start(), message_start(), text_delta(7, "nowhere")];
        assert_canonical_lifecycle(&events);
    }

    // ── Rule 6/7/8: toggleable strictness ───────────────────────────────────

    #[test]
    #[should_panic(expected = "rule 6: ContentBlockStart before MessageStart")]
    fn rule_6_block_start_before_message_start_strict() {
        let events = vec![response_start(), block_start(0, ContentBlockKind::Text)];
        assert_canonical_lifecycle(&events);
    }

    #[test]
    fn rule_6_block_start_before_message_start_relaxed() {
        let events = vec![
            response_start(),
            block_start(0, ContentBlockKind::Text),
            text_delta(0, "ok"),
            block_stop(0),
            message_stop(),
            response_stop(),
        ];
        assert_canonical_lifecycle_strict(
            &events,
            StrictRules {
                require_message_start_before_blocks: false,
                ..Default::default()
            },
        );
    }

    #[test]
    #[should_panic(expected = "rule 7: duplicate ResponseStart")]
    fn rule_7_duplicate_response_start_strict() {
        let events = vec![response_start(), response_start()];
        assert_canonical_lifecycle(&events);
    }

    #[test]
    fn rule_7_duplicate_response_start_relaxed() {
        let events = vec![
            response_start(),
            response_start(),
            message_start(),
            message_stop(),
            response_stop(),
        ];
        assert_canonical_lifecycle_strict(
            &events,
            StrictRules {
                require_single_start: false,
                ..Default::default()
            },
        );
    }

    #[test]
    #[should_panic(expected = "rule 8: UsageDelta after ResponseStop")]
    fn rule_8_event_after_response_stop_strict() {
        let events = vec![
            response_start(),
            message_start(),
            message_stop(),
            response_stop(),
            usage_delta(),
        ];
        assert_canonical_lifecycle(&events);
    }

    #[test]
    fn rule_8_event_after_response_stop_relaxed() {
        let events = vec![
            response_start(),
            message_start(),
            message_stop(),
            response_stop(),
            usage_delta(),
        ];
        assert_canonical_lifecycle_strict(
            &events,
            StrictRules {
                require_response_stop_last: false,
                ..Default::default()
            },
        );
    }
}
