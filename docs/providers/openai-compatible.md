# Provider: OpenAI-Compatible

Any API that speaks the OpenAI Chat Completions wire format can be used with
this provider.

## Config Example

```yaml
providers:
  my_provider:
    kind: openai_compatible
    base_url: "https://api.deepseek.com/v1"
    api_key: !secret DEEPSEEK_API_KEY
    model: deepseek-chat
    # Optional overrides:
    # timeout_secs: 120
    # max_retries: 2
```

## Behavior

### Authentication

Requests are authenticated with a `Bearer` token set to `api_key` in the
`Authorization` header. The key is never logged.

### Streaming

When the upstream request is streaming, the provider sends
`"stream_options": {"include_usage": true}` so that token counts are
included in the final SSE chunk. This is required for accurate `UsageDelta`
events in the canonical stream.

### Tool-Call Passthrough

Tool definitions and tool-choice are forwarded verbatim to the upstream
provider. Tool-call response chunks are mapped to `ToolCallStart`,
`ToolCallArgumentsDelta`, and `ToolCallStop` canonical events.

### Error Handling

HTTP 4xx/5xx responses from the upstream are surfaced as
`StreamEvent::Error` events in the canonical stream so the frontend
can emit an appropriate error to the client.

### Retries

The provider does **not** retry automatically. Retry logic belongs in the
caller or at the infrastructure layer (e.g. a load balancer).
