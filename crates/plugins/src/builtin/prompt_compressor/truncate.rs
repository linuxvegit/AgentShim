//! `truncate_to_tokens` strategy — drop oldest groups until total token
//! count <= target_tokens, subject to the `keep_last_n` floor.
//!
//! Spec §6.4.

#![cfg(feature = "prompt_compressor")]

use agent_shim_core::Message;

use super::groups::{count_messages_tokens, group_boundaries};
use super::CompressionResult;

pub(super) fn apply(
    messages: &[Message],
    target_tokens: u32,
    keep_last_n: usize,
) -> CompressionResult {
    let groups = group_boundaries(messages);
    if groups.len() <= keep_last_n {
        return CompressionResult {
            new_messages: messages.to_vec(),
            dropped: 0,
            action: "skipped",
        };
    }

    // Per-group token counts (compute once).
    let group_tokens: Vec<u32> = groups
        .iter()
        .map(|r| count_messages_tokens(&messages[r.clone()]))
        .collect();
    let mut total: u32 = group_tokens.iter().copied().sum();
    if total <= target_tokens {
        return CompressionResult {
            new_messages: messages.to_vec(),
            dropped: 0,
            action: "skipped",
        };
    }

    let max_drop = groups.len() - keep_last_n;
    let mut drop_idx = 0usize;
    while total > target_tokens && drop_idx < max_drop {
        total = total.saturating_sub(group_tokens[drop_idx]);
        drop_idx += 1;
    }

    let cut_idx = groups[drop_idx].start;
    let action = if total > target_tokens {
        "compressed_but_over_budget"
    } else {
        "compressed"
    };
    CompressionResult {
        new_messages: messages[cut_idx..].to_vec(),
        dropped: cut_idx,
        action,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{ContentBlock, Message, TextBlock};

    /// Build a user message of approximately N tokens by repeating a known
    /// 4-char ASCII string. cl100k typically encodes 4 chars ≈ 1 token,
    /// so `N` chars ≈ N/4 tokens. Tests below use coarse bounds, not exact
    /// equality, to remain stable across tiktoken updates.
    fn user_of_chars(n: usize) -> Message {
        Message::user(vec![ContentBlock::Text(TextBlock {
            text: "abcd".repeat(n / 4),
            extensions: Default::default(),
        })])
    }

    fn assistant_short() -> Message {
        Message::assistant(vec![ContentBlock::Text(TextBlock {
            text: "ok".to_string(),
            extensions: Default::default(),
        })])
    }

    #[test]
    fn truncate_skipped_when_already_under_budget() {
        let messages = vec![user_of_chars(40), assistant_short()];
        // ~10 input tokens + a couple output tokens, target 10000 -> skipped.
        let result = apply(&messages, 10_000, 1);
        assert_eq!(result.action, "skipped");
    }

    #[test]
    fn truncate_skipped_when_groups_le_keep_last_n() {
        let messages = vec![
            user_of_chars(4000),
            assistant_short(),
            user_of_chars(4000),
            assistant_short(),
        ];
        // 2 groups, keep_last_n=2 -> hard floor wins, skipped regardless of budget.
        let result = apply(&messages, 10, 2);
        assert_eq!(result.action, "skipped");
        assert_eq!(result.dropped, 0);
    }

    #[test]
    fn truncate_drops_oldest_until_under_budget() {
        // 4 groups, ~100 tokens each (400 chars / ~25 tok). target=80, keep=1.
        let messages = vec![
            user_of_chars(400),
            assistant_short(),
            user_of_chars(400),
            assistant_short(),
            user_of_chars(400),
            assistant_short(),
            user_of_chars(400),
            assistant_short(),
        ];
        let result = apply(&messages, 80, 1);
        assert!(
            result.action == "compressed" || result.action == "compressed_but_over_budget",
            "expected compression, got {}",
            result.action
        );
        assert!(result.dropped > 0, "expected at least one dropped message");
    }

    #[test]
    fn truncate_compressed_but_over_budget() {
        // 3 groups all heavy; keep_last_n=1; target very small.
        let messages = vec![
            user_of_chars(400),
            assistant_short(),
            user_of_chars(400),
            assistant_short(),
            user_of_chars(400),
            assistant_short(),
        ];
        let result = apply(&messages, 1, 1);
        // Drop down to 1 group (2 messages); that group is still ~100 tokens.
        assert_eq!(result.action, "compressed_but_over_budget");
        assert_eq!(result.new_messages.len(), 2);
    }

    #[test]
    fn truncate_dropped_count_matches_message_count() {
        let messages = vec![
            user_of_chars(400),
            assistant_short(),
            user_of_chars(400),
            assistant_short(),
            user_of_chars(400),
            assistant_short(),
        ];
        let result = apply(&messages, 50, 1);
        // Dropped should be the number of messages cut off from the front.
        assert_eq!(result.dropped + result.new_messages.len(), messages.len());
    }
}
