//! `drop_old_turns` strategy — delete oldest user-led groups until only
//! `keep_last_n` groups remain at the tail.
//!
//! Spec §6.3.

#![cfg(feature = "prompt_compressor")]

use agent_shim_core::Message;

use super::groups::group_boundaries;
use super::CompressionResult;

pub(super) fn apply(messages: &[Message], keep_last_n: usize) -> CompressionResult {
    let groups = group_boundaries(messages);
    if groups.len() <= keep_last_n {
        return CompressionResult {
            new_messages: messages.to_vec(),
            dropped: 0,
            action: "skipped",
        };
    }
    let drop_until = groups.len() - keep_last_n;
    let cut_idx = groups[drop_until].start;
    CompressionResult {
        new_messages: messages[cut_idx..].to_vec(),
        dropped: cut_idx,
        action: "compressed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{ContentBlock, Message, TextBlock};

    fn user(s: &str) -> Message {
        Message::user(vec![ContentBlock::Text(TextBlock {
            text: s.to_string(),
            extensions: Default::default(),
        })])
    }

    fn assistant(s: &str) -> Message {
        Message::assistant(vec![ContentBlock::Text(TextBlock {
            text: s.to_string(),
            extensions: Default::default(),
        })])
    }

    #[test]
    fn drop_skipped_when_groups_le_keep_last_n() {
        let messages = vec![user("a"), assistant("b"), user("c"), assistant("d")];
        let result = apply(&messages, 2);
        assert_eq!(result.action, "skipped");
        assert_eq!(result.dropped, 0);
        assert_eq!(result.new_messages.len(), 4);
    }

    #[test]
    fn drop_keeps_exactly_n_groups() {
        // 5 groups, keep_last_n=2 -> drop oldest 3 groups (6 messages).
        let messages = vec![
            user("g1u"), assistant("g1a"),
            user("g2u"), assistant("g2a"),
            user("g3u"), assistant("g3a"),
            user("g4u"), assistant("g4a"),
            user("g5u"), assistant("g5a"),
        ];
        let result = apply(&messages, 2);
        assert_eq!(result.action, "compressed");
        assert_eq!(result.dropped, 6, "3 groups × 2 messages = 6 dropped");
        assert_eq!(result.new_messages.len(), 4, "2 groups × 2 messages = 4 kept");
    }

    #[test]
    fn drop_preserves_kept_messages_verbatim() {
        let messages = vec![user("old"), assistant("old"), user("kept"), assistant("kept")];
        let result = apply(&messages, 1);
        assert_eq!(result.action, "compressed");
        match &result.new_messages[0].content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "kept"),
            _ => panic!("unexpected block"),
        }
    }

    #[test]
    fn drop_with_only_one_group_when_keep_is_one() {
        let messages = vec![user("solo"), assistant("reply")];
        let result = apply(&messages, 1);
        assert_eq!(result.action, "skipped");
        assert_eq!(result.dropped, 0);
    }
}
