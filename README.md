# AgentShim

![CI](https://github.com/anthropics/agent-shim/actions/workflows/ci.yaml/badge.svg)
![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)

A Rust gateway that translates between AI agent API dialects (Anthropic Messages,
OpenAI Chat Completions) and upstream LLM providers — with streaming, tool calls,
and cancellation propagation built in.

## Why?

Coding agents (Copilot, Cursor, Continue, …) typically speak only one API dialect.
AgentShim lets you point any such agent at any compatible backend — DeepSeek,
a local Ollama instance, Azure OpenAI, or your own model — without patching the agent.

## Quickstart

### Install

```bash
# From pre-built release
curl -Lo agent-shim \
  https://github.com/anthropics/agent-shim/releases/latest/download/agent-shim-linux-x86_64
chmod +x agent-shim

# Or build from source
cargo build --release -p agent-shim
```

### Configure

```yaml
# gateway.yaml
server:
  host: "127.0.0.1"
  port: 8787

routes:
  - frontend: anthropic_messages
    path_prefix: /v1/messages
    provider: deepseek

providers:
  deepseek:
    kind: openai_compatible
    base_url: "https://api.deepseek.com/v1"
    api_key: !secret DEEPSEEK_API_KEY
    model: deepseek-chat
```

### Run

```bash
DEEPSEEK_API_KEY=sk-... ./agent-shim serve --config gateway.yaml
```

## GitHub Copilot

To route GitHub Copilot traffic through AgentShim, configure the Copilot provider
in your gateway and point the Copilot extension at `http://127.0.0.1:8787`.

See [docs/providers/openai-compatible.md](docs/providers/openai-compatible.md)
and [docs/frontends/openai-chat-completions.md](docs/frontends/openai-chat-completions.md).

## Docker

```bash
DEEPSEEK_API_KEY=sk-... docker compose -f deploy/docker-compose.yaml up
```

See [docs/deployment.md](docs/deployment.md) for full options.

## Documentation

| Document | Contents |
|----------|---------|
| [docs/architecture.md](docs/architecture.md) | Crate graph, request lifecycle, canonical model |
| [docs/configuration.md](docs/configuration.md) | YAML schema, env overlay, secrets |
| [docs/deployment.md](docs/deployment.md) | Binary, Docker, logging, health check |
| [docs/contributing.md](docs/contributing.md) | Toolchain, adding frontends/providers, tests |
| [docs/frontends/anthropic-messages.md](docs/frontends/anthropic-messages.md) | Anthropic Messages support matrix |
| [docs/frontends/openai-chat-completions.md](docs/frontends/openai-chat-completions.md) | OpenAI Chat support matrix |
| [docs/providers/openai-compatible.md](docs/providers/openai-compatible.md) | OpenAI-compatible provider config |

## License

MIT — see [LICENSE](LICENSE).
