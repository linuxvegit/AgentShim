# Frontend: OpenAI Chat Completions API

## Supported

| Feature | Notes |
|---------|-------|
| Single-turn and multi-turn messages | `user` / `assistant` / `system` / `tool` roles |
| Streaming (`stream: true`) | SSE with `text/event-stream` and `[DONE]` sentinel |
| Non-streaming | JSON response |
| Text content (string or array) | Both forms accepted |
| Function / tool calling | `tools` + `tool_choice` forwarded |
| Tool result messages (`role: tool`) | Mapped to canonical tool-result content |
| `max_tokens` / `max_completion_tokens` | Both accepted |
| `temperature`, `top_p` | Forwarded |
| `stop` (string or array) | Forwarded as stop sequences |
| `stream_options: {include_usage: true}` | Usage emitted in final chunk |

## Not Supported

- `logprobs` / `top_logprobs`
- `response_format` structured output (JSON schema enforcement)
- `parallel_tool_calls: false` enforcement
- Predicted outputs (`prediction` field)
- Audio input/output modalities
- Legacy `functions` / `function_call` fields

## Stop-Reason Mapping

| OpenAI `finish_reason` | Canonical `StopReason` | Sent back |
|-----------------------|----------------------|-----------|
| `stop` | `EndTurn` | `stop` |
| `length` | `MaxTokens` | `length` |
| `tool_calls` | `ToolUse` | `tool_calls` |
| `content_filter` | `ContentFilter` | `content_filter` |
| anything else | `Other(string)` | original string |

## SSE Event Sequence

```
data: {"id":"…","object":"chat.completion.chunk","choices":[{"delta":{"role":"assistant"},"index":0}]}
data: {"choices":[{"delta":{"content":"…"},"index":0}]}  (×N)
data: {"choices":[{"delta":{},"finish_reason":"stop","index":0}],"usage":{…}}
data: [DONE]
```
