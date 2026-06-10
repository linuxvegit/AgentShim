//! Pure helpers: split a `&[Message]` into user-led groups, and count
//! tokens of a group's text/reasoning blocks.
//!
//! Spec §6.1 (groups) + §6.2 (token counting).

#![cfg(feature = "prompt_compressor")]

use std::ops::Range;

use agent_shim_core::{ContentBlock, Message, MessageRole};

/// Split messages into groups. Each group is a contiguous [start, end)
/// range starting at a user-role message and extending up to (but not
/// including) the next user-role message.
///
/// Properties:
/// - `groups.iter().map(|r| r.len()).sum::<usize>() == messages.len()`
/// - Adjacent groups never overlap.
/// - `messages.is_empty()` -> `[]`.
/// - The first group's first message is user-role unless `messages[0]`
///   itself is non-user (defensive: preserve input, never panic).
pub fn group_boundaries(messages: &[Message]) -> Vec<Range<usize>> {
    if messages.is_empty() {
        return Vec::new();
    }
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

/// Sum cl100k tokens of text + reasoning blocks across the given message
/// range. Image/audio/file/tool_call/tool_result/redacted_reasoning/
/// unsupported blocks contribute 0 — they are not text-billed via cl100k.
pub fn count_messages_tokens(messages: &[Message]) -> u32 {
    let mut total: u32 = 0;
    for msg in messages {
        for block in &msg.content {
            match block {
                ContentBlock::Text(t) => {
                    total = total.saturating_add(agent_shim_tokens::count_text(&t.text));
                }
                ContentBlock::Reasoning(r) => {
                    total = total.saturating_add(agent_shim_tokens::count_text(&r.text));
                }
                _ => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        BinarySource, ContentBlock, ImageBlock, Message, ReasoningBlock, TextBlock,
    };

    fn user_text(s: &str) -> Message {
        Message::user(vec![ContentBlock::Text(TextBlock {
            text: s.to_string(),
            extensions: Default::default(),
        })])
    }

    fn assistant_text(s: &str) -> Message {
        Message::assistant(vec![ContentBlock::Text(TextBlock {
            text: s.to_string(),
            extensions: Default::default(),
        })])
    }

    #[test]
    fn empty_messages_no_groups() {
        let groups = group_boundaries(&[]);
        assert!(groups.is_empty());
    }

    #[test]
    fn single_user_message_one_group() {
        let messages = vec![user_text("hi")];
        let groups = group_boundaries(&messages);
        assert_eq!(groups, vec![0..1]);
    }

    #[test]
    fn user_assistant_pair_one_group() {
        let messages = vec![user_text("hi"), assistant_text("hello")];
        let groups = group_boundaries(&messages);
        assert_eq!(groups, vec![0..2]);
    }

    #[test]
    fn two_user_turns_two_groups() {
        let messages = vec![
            user_text("a"),
            assistant_text("b"),
            user_text("c"),
            assistant_text("d"),
        ];
        let groups = group_boundaries(&messages);
        assert_eq!(groups, vec![0..2, 2..4]);
    }

    #[test]
    fn tool_turn_grouped_with_assistant() {
        // A Tool-role message between two user-led groups should attach
        // to the preceding group (it is NOT a user boundary).
        // [user, assistant, tool, assistant, user]
        // Groups should be: [0..4, 4..5]
        let tool_msg = Message {
            role: MessageRole::Tool,
            content: vec![ContentBlock::Text(TextBlock {
                text: "tool result".to_string(),
                extensions: Default::default(),
            })],
            name: None,
            source: None,
            extensions: Default::default(),
        };
        let messages = vec![
            user_text("question"),
            assistant_text("calling tool..."),
            tool_msg,
            assistant_text("answer"),
            user_text("next question"),
        ];
        let groups = group_boundaries(&messages);
        assert_eq!(groups, vec![0..4, 4..5]);
    }

    #[test]
    fn count_messages_tokens_text_only() {
        let messages = vec![
            user_text("hello world"),
            assistant_text("hi there"),
            Message::user(vec![
                ContentBlock::Reasoning(ReasoningBlock {
                    text: "thinking...".to_string(),
                    extensions: Default::default(),
                }),
                ContentBlock::Image(ImageBlock {
                    source: BinarySource::Url {
                        url: "https://example.com/cat.png".to_string(),
                    },
                    extensions: Default::default(),
                }),
            ]),
        ];
        let n = count_messages_tokens(&messages);
        assert!(n > 0, "expected positive token count, got {n}");
        assert!(n < 100, "expected < 100 tokens, got {n}");
    }
}
