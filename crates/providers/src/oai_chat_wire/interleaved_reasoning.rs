//! State machine that interleaves text and reasoning deltas into properly
//! bracketed `ContentBlockStart` / `ContentBlockStop` boundaries.
//!
//! Several OpenAI-chat-compatible providers (DeepSeek-R1, Gemini's thinking
//! mode) emit reasoning content interleaved with regular text within a single
//! SSE stream. The canonical model represents these as separate
//! [`ContentBlock`]s — each emission of a `Reasoning` or `Text` block is
//! bracketed by `ContentBlockStart` / `ContentBlockStop`, and deltas are
//! tagged with the block's index.
//!
//! When upstream alternates kinds (reasoning chunk → text chunk → reasoning
//! chunk), the parser must close the current block and open a new one — never
//! two consecutive `ContentBlockStart`s without a `ContentBlockStop`, and
//! never two deltas of different kinds with the same index. This lib
//! encapsulates that bookkeeping so each provider's SSE parser composes one
//! source of truth instead of re-implementing it.
//!
//! Sibling provider modules consume this via `pub(crate)` re-export from
//! `oai_chat_wire/mod.rs`.

use agent_shim_core::{ContentBlockKind, StreamEvent};

/// Whether an incoming delta is regular assistant text or reasoning content.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    #[default]
    Text,
    Reasoning,
}

/// Tracks which (if any) content block is currently open and what its index
/// is. Private — consumers only see [`ReasoningInterleaver`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    InText {
        index: u32,
    },
    InReasoning {
        index: u32,
    },
}

/// Consumes a stream of (kind, text) deltas and emits the right combination
/// of `ContentBlockStart` / `ContentBlockStop` plus `TextDelta` /
/// `ReasoningDelta` events.
///
/// Index policy: blocks are numbered from 0. The first block of any kind gets
/// index 0; every kind switch increments the index by 1.
///
/// Empty-delta policy: pushes with `text == ""` are dropped silently. They do
/// not allocate a new block index, so an upstream that sends a leading empty
/// `content` chunk followed by a real `reasoning_content` chunk produces a
/// reasoning block at index 0 — not a text block at 0 followed by reasoning
/// at 1.
#[derive(Debug, Default)]
pub struct ReasoningInterleaver {
    state: State,
    next_index: u32,
}

impl ReasoningInterleaver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a delta. Emits the right combination of `ContentBlockStart` /
    /// `ContentBlockStop` and `TextDelta` / `ReasoningDelta` events into
    /// `out`.
    pub fn push(&mut self, kind: DeltaKind, text: &str, out: &mut Vec<StreamEvent>) {
        // Drop empty deltas without allocating a block index. See struct doc.
        if text.is_empty() {
            return;
        }

        match (self.state, kind) {
            (State::InText { index }, DeltaKind::Text) => {
                out.push(StreamEvent::TextDelta {
                    index,
                    text: text.to_string(),
                });
            }
            (State::InReasoning { index }, DeltaKind::Reasoning) => {
                out.push(StreamEvent::ReasoningDelta {
                    index,
                    text: text.to_string(),
                });
            }
            (State::Idle, _) => {
                self.open_new_block(kind, text, out);
            }
            (State::InText { index }, DeltaKind::Reasoning)
            | (State::InReasoning { index }, DeltaKind::Text) => {
                out.push(StreamEvent::ContentBlockStop { index });
                self.state = State::Idle;
                self.open_new_block(kind, text, out);
            }
        }
    }

    /// Close the current block (no-op if `Idle`). Called on stream end so the
    /// final block is properly bracketed.
    pub fn flush(&mut self, out: &mut Vec<StreamEvent>) {
        match self.state {
            State::Idle => {}
            State::InText { index } | State::InReasoning { index } => {
                out.push(StreamEvent::ContentBlockStop { index });
                self.state = State::Idle;
            }
        }
    }

    /// Allocates a fresh block index, emits `ContentBlockStart`, transitions
    /// state, and emits the first delta. Caller must guarantee `text` is
    /// non-empty and `state` is currently `Idle`.
    fn open_new_block(&mut self, kind: DeltaKind, text: &str, out: &mut Vec<StreamEvent>) {
        let index = self.next_index;
        self.next_index += 1;
        match kind {
            DeltaKind::Text => {
                out.push(StreamEvent::ContentBlockStart {
                    index,
                    kind: ContentBlockKind::Text,
                });
                out.push(StreamEvent::TextDelta {
                    index,
                    text: text.to_string(),
                });
                self.state = State::InText { index };
            }
            DeltaKind::Reasoning => {
                out.push(StreamEvent::ContentBlockStart {
                    index,
                    kind: ContentBlockKind::Reasoning,
                });
                out.push(StreamEvent::ReasoningDelta {
                    index,
                    text: text.to_string(),
                });
                self.state = State::InReasoning { index };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<F>(setup: F) -> Vec<StreamEvent>
    where
        F: FnOnce(&mut ReasoningInterleaver, &mut Vec<StreamEvent>),
    {
        let mut interleaver = ReasoningInterleaver::new();
        let mut out = Vec::new();
        setup(&mut interleaver, &mut out);
        out
    }

    #[test]
    fn new_starts_idle_emits_nothing() {
        let out = run(|i, out| i.flush(out));
        assert_eq!(out, Vec::<StreamEvent>::new());
    }

    #[test]
    fn single_text_delta_starts_block_at_index_0() {
        let out = run(|i, out| i.push(DeltaKind::Text, "hi", out));
        assert_eq!(
            out,
            vec![
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Text,
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "hi".into(),
                },
            ]
        );
    }

    #[test]
    fn single_reasoning_delta_starts_block_at_index_0() {
        let out = run(|i, out| i.push(DeltaKind::Reasoning, "think", out));
        assert_eq!(
            out,
            vec![
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Reasoning,
                },
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "think".into(),
                },
            ]
        );
    }

    #[test]
    fn multiple_text_deltas_share_index() {
        let out = run(|i, out| {
            i.push(DeltaKind::Text, "a", out);
            i.push(DeltaKind::Text, "b", out);
        });
        assert_eq!(
            out,
            vec![
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Text,
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "a".into(),
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "b".into(),
                },
            ]
        );
    }

    #[test]
    fn multiple_reasoning_deltas_share_index() {
        let out = run(|i, out| {
            i.push(DeltaKind::Reasoning, "x", out);
            i.push(DeltaKind::Reasoning, "y", out);
        });
        assert_eq!(
            out,
            vec![
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Reasoning,
                },
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "x".into(),
                },
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "y".into(),
                },
            ]
        );
    }

    #[test]
    fn text_to_reasoning_transition_closes_and_opens() {
        let out = run(|i, out| {
            i.push(DeltaKind::Text, "a", out);
            i.push(DeltaKind::Reasoning, "b", out);
        });
        assert_eq!(
            out,
            vec![
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Text,
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "a".into(),
                },
                StreamEvent::ContentBlockStop { index: 0 },
                StreamEvent::ContentBlockStart {
                    index: 1,
                    kind: ContentBlockKind::Reasoning,
                },
                StreamEvent::ReasoningDelta {
                    index: 1,
                    text: "b".into(),
                },
            ]
        );
    }

    #[test]
    fn reasoning_to_text_transition_closes_and_opens() {
        let out = run(|i, out| {
            i.push(DeltaKind::Reasoning, "a", out);
            i.push(DeltaKind::Text, "b", out);
        });
        assert_eq!(
            out,
            vec![
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Reasoning,
                },
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "a".into(),
                },
                StreamEvent::ContentBlockStop { index: 0 },
                StreamEvent::ContentBlockStart {
                    index: 1,
                    kind: ContentBlockKind::Text,
                },
                StreamEvent::TextDelta {
                    index: 1,
                    text: "b".into(),
                },
            ]
        );
    }

    #[test]
    fn three_transitions_increment_index() {
        let out = run(|i, out| {
            i.push(DeltaKind::Text, "a", out);
            i.push(DeltaKind::Reasoning, "b", out);
            i.push(DeltaKind::Text, "c", out);
        });
        assert_eq!(
            out,
            vec![
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Text,
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "a".into(),
                },
                StreamEvent::ContentBlockStop { index: 0 },
                StreamEvent::ContentBlockStart {
                    index: 1,
                    kind: ContentBlockKind::Reasoning,
                },
                StreamEvent::ReasoningDelta {
                    index: 1,
                    text: "b".into(),
                },
                StreamEvent::ContentBlockStop { index: 1 },
                StreamEvent::ContentBlockStart {
                    index: 2,
                    kind: ContentBlockKind::Text,
                },
                StreamEvent::TextDelta {
                    index: 2,
                    text: "c".into(),
                },
            ]
        );
    }

    #[test]
    fn flush_closes_open_text_block() {
        let out = run(|i, out| {
            i.push(DeltaKind::Text, "a", out);
            i.flush(out);
        });
        assert_eq!(
            out,
            vec![
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Text,
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "a".into(),
                },
                StreamEvent::ContentBlockStop { index: 0 },
            ]
        );
    }

    #[test]
    fn flush_closes_open_reasoning_block() {
        let out = run(|i, out| {
            i.push(DeltaKind::Reasoning, "a", out);
            i.flush(out);
        });
        assert_eq!(
            out,
            vec![
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Reasoning,
                },
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "a".into(),
                },
                StreamEvent::ContentBlockStop { index: 0 },
            ]
        );
    }

    #[test]
    fn flush_after_flush_is_noop() {
        let out = run(|i, out| {
            i.flush(out);
            i.flush(out);
        });
        assert_eq!(out, Vec::<StreamEvent>::new());
    }

    #[test]
    fn flush_after_close_is_noop() {
        let out = run(|i, out| {
            i.push(DeltaKind::Text, "a", out);
            i.flush(out);
            // Second flush should add nothing.
            let len_before = out.len();
            i.flush(out);
            assert_eq!(out.len(), len_before);
        });
        // Sanity: just confirm the first flush produced the expected close.
        assert_eq!(
            out.last(),
            Some(&StreamEvent::ContentBlockStop { index: 0 })
        );
    }

    #[test]
    fn empty_delta_is_dropped() {
        let mut interleaver = ReasoningInterleaver::new();
        let mut out = Vec::new();
        interleaver.push(DeltaKind::Text, "", &mut out);
        assert_eq!(out, Vec::<StreamEvent>::new());
        // State must remain Idle so the next non-empty push opens block 0.
        interleaver.push(DeltaKind::Reasoning, "real", &mut out);
        assert_eq!(
            out,
            vec![
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Reasoning,
                },
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "real".into(),
                },
            ]
        );
    }

    #[test]
    fn empty_delta_inside_open_block_does_not_emit() {
        let out = run(|i, out| {
            i.push(DeltaKind::Text, "hello", out);
            i.push(DeltaKind::Text, "", out);
            i.push(DeltaKind::Reasoning, "", out);
            i.push(DeltaKind::Text, "world", out);
        });
        // The two empty pushes are dropped; same Text block stays open.
        assert_eq!(
            out,
            vec![
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Text,
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "hello".into(),
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "world".into(),
                },
            ]
        );
    }
}
