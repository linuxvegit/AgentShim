# Deployment

## Single Binary

Download the pre-built binary from the GitHub Releases page and run it directly:

```bash
curl -Lo agent-shim https://github.com/anthropics/agent-shim/releases/latest/download/agent-shim-linux-x86_64
chmod +x agent-shim
./agent-shim serve --config gateway.yaml
```

## Docker

```bash
# Build locally
docker build -f deploy/Dockerfile -t agent-shim .
docker run -p 8787:8787 \
  -v $(pwd)/config/gateway.example.yaml:/etc/agent-shim/gateway.yaml:ro \
  -e DEEPSEEK_API_KEY=sk-... \
  agent-shim

# Or use docker compose from the repo root
DEEPSEEK_API_KEY=sk-... docker compose -f deploy/docker-compose.yaml up
```

## Operational Stance

- **Single process, no clustering.** Run multiple instances behind a load balancer if horizontal scale is needed.
- **Stateless.** No database, no persistent state between requests. Safe to restart at any time.
- **Ports.** Only one port (`8787` by default) is needed. No admin or metrics port is exposed by default.

## Logging

Structured JSON logs are emitted to stdout. Control verbosity with:

```bash
RUST_LOG=info agent-shim serve --config gateway.yaml
RUST_LOG=agent_shim=debug,tower_http=info agent-shim serve --config gateway.yaml
```

## Health Check

```
GET /health  →  200 OK  {"status":"ok"}
```

Use this endpoint for container liveness/readiness probes.
