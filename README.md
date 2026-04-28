# AgentShim

A single-binary Rust gateway that lets any AI coding agent talk to any LLM backend.

Point Claude Code at DeepSeek. Point Cursor at Ollama. Point Codex at GitHub Copilot. AgentShim translates between API dialects on the fly — streaming, tool calls, and all.

## What it does

```
┌──────────────┐         ┌──────────────┐         ┌──────────────────┐
│  Claude Code │────────▶│              │────────▶│  DeepSeek API    │
│  (Anthropic) │   /v1/  │              │  OpenAI │                  │
└──────────────┘  messages│  AgentShim   │  compat ├──────────────────┤
                         │              │         │  Ollama / vLLM   │
┌──────────────┐         │  Translates  │         ├──────────────────┤
│ Cursor/Codex │────────▶│  protocols   │────────▶│  GitHub Copilot  │
│   (OpenAI)   │  /v1/   │  + streams   │  OAuth  │                  │
└──────────────┘  chat/   │              │  device ├──────────────────┤
               completions│              │  flow   │  Kimi / Qwen     │
                         └──────────────┘         └──────────────────┘
```

**Frontends** (what your agent speaks):
- Anthropic `/v1/messages` — full SSE streaming, tool use, thinking blocks
- OpenAI `/v1/chat/completions` — full SSE streaming, tool calls, `[DONE]` terminator

**Backends** (where requests go):
- **OpenAI-compatible** — any provider with a `/v1/chat/completions` endpoint (DeepSeek, Kimi, Qwen, Ollama, vLLM, llama.cpp, Azure OpenAI, etc.)
- **GitHub Copilot** — OAuth device-flow login, automatic token refresh, Copilot-specific headers

**Cross-protocol translation works.** An Anthropic-speaking agent can talk to an OpenAI-compatible backend and vice versa, including streaming tool-call argument deltas.

## Install

**From source:**

```bash
cargo build --release -p agent-shim
# Binary at target/release/agent-shim
```

**Docker:**

```bash
docker run --rm -p 8787:8787 \
  -v $(pwd)/gateway.yaml:/etc/agent-shim/gateway.yaml:ro \
  -e DEEPSEEK_API_KEY \
  ghcr.io/anthropics/agent-shim:latest
```

## Configure

Create a `gateway.yaml`:

```yaml
server:
  bind: 127.0.0.1
  port: 8787
  keepalive_secs: 15

logging:
  format: pretty                    # or "json" for production
  filter: info,agent_shim=debug

upstreams:
  deepseek:
    type: open_ai_compatible
    base_url: https://api.deepseek.com/v1
    api_key: sk-your-key-here       # or use env: AGENT_SHIM__UPSTREAMS__DEEPSEEK__API_KEY
    request_timeout_secs: 120

routes:
  # Claude Code → DeepSeek (Anthropic protocol in, OpenAI-compat out)
  - frontend: anthropic_messages
    model: deepseek-chat
    upstream: deepseek
    upstream_model: deepseek-chat

  # Cursor/Codex → DeepSeek (OpenAI protocol in, OpenAI-compat out)
  - frontend: openai_chat
    model: deepseek-chat
    upstream: deepseek
    upstream_model: deepseek-chat
```

Validate before running:

```bash
agent-shim validate-config --config gateway.yaml
# OK: 2 routes, 1 upstreams
```

## Run

```bash
export DEEPSEEK_API_KEY=sk-...
agent-shim serve --config gateway.yaml
```

Now point your agent at `http://127.0.0.1:8787`:
- Claude Code / Anthropic clients → `http://127.0.0.1:8787/v1/messages`
- Cursor / Codex / OpenAI clients → `http://127.0.0.1:8787/v1/chat/completions`

## GitHub Copilot

Use Copilot models through AgentShim with a paid Copilot subscription:

```bash
# 1. Authenticate (one-time)
agent-shim copilot login
# Opens browser for GitHub OAuth device flow
# Saves credentials to ~/.config/agent-shim/copilot.json

# 2. Add to config
```

```yaml
upstreams:
  copilot:
    type: github_copilot

copilot:
  credential_path: ~/.config/agent-shim/copilot.json  # optional, this is the default

routes:
  - frontend: anthropic_messages
    model: claude-3.5-sonnet
    upstream: copilot
    upstream_model: claude-3.5-sonnet
  - frontend: anthropic_messages
    model: gpt-4o
    upstream: copilot
    upstream_model: gpt-4o
```

The token manager handles refresh automatically. If a token expires mid-session, the next request re-authenticates transparently.

## Quick examples

**Route Claude Code through a local Ollama instance:**

```yaml
upstreams:
  local:
    type: open_ai_compatible
    base_url: http://localhost:11434/v1
    api_key: unused
    request_timeout_secs: 300

routes:
  - frontend: anthropic_messages
    model: llama3
    upstream: local
    upstream_model: llama3:70b
```

**Multiple backends with model aliasing:**

```yaml
upstreams:
  deepseek:
    type: open_ai_compatible
    base_url: https://api.deepseek.com/v1
    api_key: sk-...
    request_timeout_secs: 120
  copilot:
    type: github_copilot

routes:
  # "fast" alias → DeepSeek
  - frontend: anthropic_messages
    model: fast
    upstream: deepseek
    upstream_model: deepseek-chat

  # "smart" alias → Copilot's Claude
  - frontend: anthropic_messages
    model: smart
    upstream: copilot
    upstream_model: claude-3.5-sonnet
```

Your agent requests `model: "fast"` or `model: "smart"` and AgentShim routes to the right backend.

## Environment variable overlay

Any config field can be overridden via environment variables with the `AGENT_SHIM__` prefix (double underscore for nesting):

```bash
AGENT_SHIM__SERVER__PORT=9000
AGENT_SHIM__UPSTREAMS__DEEPSEEK__API_KEY=sk-...
AGENT_SHIM__LOGGING__FORMAT=json
```

## Config reference

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `server.bind` | string | `127.0.0.1` | Listen address |
| `server.port` | u16 | `8787` | Listen port |
| `server.keepalive_secs` | u64 | `15` | SSE keepalive interval (0 = disabled) |
| `logging.format` | `pretty` \| `json` | `pretty` | Log output format |
| `logging.filter` | string | `info,agent_shim=debug` | `RUST_LOG`-style filter |
| `upstreams.<name>.type` | `open_ai_compatible` \| `github_copilot` | — | Backend type |
| `upstreams.<name>.base_url` | string | — | API base URL (OpenAI-compat only) |
| `upstreams.<name>.api_key` | string | — | API key (OpenAI-compat only) |
| `upstreams.<name>.request_timeout_secs` | u64 | `120` | Request timeout |
| `routes[].frontend` | `anthropic_messages` \| `openai_chat` | — | Which frontend endpoint handles this |
| `routes[].model` | string | — | Model alias the agent requests |
| `routes[].upstream` | string | — | Which upstream to route to |
| `routes[].upstream_model` | string | — | Model name sent to the upstream |

Unknown fields are rejected at startup (`deny_unknown_fields`). Typos fail loudly.

## Health check

```bash
curl http://127.0.0.1:8787/healthz
# ok
```

## How it works

1. Agent sends a request to `/v1/messages` or `/v1/chat/completions`
2. The **frontend adapter** decodes it into a protocol-neutral `CanonicalRequest`
3. The **router** resolves `(frontend, model_alias)` → `BackendTarget`
4. The **provider** encodes the request for the upstream, opens a streaming connection, and parses the response back into a `CanonicalStream`
5. The **frontend encoder** translates the stream into the agent's expected SSE format

No buffering — backpressure flows end-to-end. Client disconnect cancels the upstream request.

## Project structure

```
crates/
  core/           # Canonical data model (zero I/O)
  config/         # YAML schema, validation, Secret newtype
  observability/  # Tracing, request-ID middleware, header redaction
  frontends/      # Anthropic + OpenAI protocol adapters
  providers/      # OpenAI-compatible + GitHub Copilot backends
  router/         # Model alias → backend resolution
  gateway/        # The binary: axum server, CLI, signal handling
  protocol-tests/ # Golden SSE tests, cross-protocol tests, fuzz
```

## What's NOT in v0.1

- OpenAI `/v1/responses` frontend (Phase 3)
- Native DeepSeek/Gemini/Qwen adapters with provider-specific quirk handling (Phase 2)
- Fallback chains, circuit breakers, retries (Phase 4)
- Rate limiting, per-agent API keys (Phase 4)
- Prometheus metrics, hot-reload config, OpenTelemetry (Phase 5)
- Vision / audio / file content end-to-end
- Multi-account Copilot

See the [design spec](docs/superpowers/specs/2026-04-28-agent-shim-design.md) for the full roadmap.

## Contributing

```bash
cargo fmt --all -- --check        # format check
cargo clippy --workspace -- -D warnings  # lint
cargo test --workspace            # all tests
```

See [docs/contributing.md](docs/contributing.md) for how to add frontends and providers.

## License

MIT — see [LICENSE](LICENSE).
