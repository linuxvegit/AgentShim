# Frontend: Anthropic Messages API

## Supported

| Feature | Notes |
|---------|-------|
| Single-turn and multi-turn messages | `user` / `assistant` roles |
| Streaming (`stream: true`) | SSE with `text/event-stream` |
| Non-streaming | JSON response |
| Text content blocks | `type: text` |
| Tool use (request) | `tools` + `tool_choice` forwarded |
| Tool result blocks | Passed through in messages |
| `max_tokens` | Forwarded to provider |
| `temperature`, `top_p`, `top_k` | Forwarded where provider supports |
| `stop_sequences` | Forwarded |
| System prompt (string) | Forwarded |
| SSE keepalive pings | Configurable interval |

## Not Supported

- `computer_use` beta tool type
- `web_search` tool type
- PDF / document content blocks
- Streaming in batch mode
- Cache control (`cache_control` field on messages)
- Multi-modal image input (images in messages)

## Stop-Reason Mapping

| Provider value | Canonical `StopReason` | Anthropic wire value |
|---------------|----------------------|---------------------|
| `end_turn` / `stop` | `EndTurn` | `end_turn` |
| `max_tokens` / `length` | `MaxTokens` | `max_tokens` |
| `tool_use` / `tool_calls` | `ToolUse` | `tool_use` |
| `stop_sequence` | `StopSequence` | `stop_sequence` |
| `content_filter` | `ContentFilter` | `content_filter` |
| anything else | `Other(string)` | `other` |

## SSE Event Sequence

```
event: message_start
event: content_block_start
event: content_block_delta  (×N)
event: content_block_stop
event: message_delta        (contains stop_reason + usage)
event: message_stop
```
