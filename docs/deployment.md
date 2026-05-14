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

## Windows Service

agent-shim ships with first-class Windows Service support on Windows builds. The `agent-shim service` subcommand registers, queries, controls, and removes the service entirely through `agent-shim`'s own CLI — no manual `sc.exe` invocations required.

### Prerequisites

- An **elevated** PowerShell or CMD. `install`, `uninstall`, `start`, `stop`, and `restart` operate on the Service Control Manager and refuse to run from a normal user terminal. `status` does not need admin.
- The `agent-shim.exe` binary at a stable path. The Service Control Manager records the absolute path at install time; moving the binary later requires `uninstall` + `install`.
- A validated config file at an **absolute** path. Service-launched processes have `cwd = C:\Windows\System32`, so relative paths break.

### Install

```powershell
# Run from an elevated PowerShell.
agent-shim service install --config "C:\ProgramData\agent-shim\gateway.yaml"
```

The install command:

1. Validates that the config path is absolute.
2. Loads and validates the config (same checks as `agent-shim validate-config`). Misconfigured services are never registered.
3. Resolves the current `agent-shim.exe` location.
4. Registers the service with SCM using the following defaults — override with the flags shown below:

   | Flag                  | Default                       | Description                                                 |
   |-----------------------|-------------------------------|-------------------------------------------------------------|
   | `--name`              | `agent-shim`                  | SCM service name. Use a different name to register multiple instances. |
   | `--display-name`      | `AgentShim Gateway`           | Friendly name shown in the Services MMC console.            |
   | `--account`           | `LocalSystem`                 | One of `LocalSystem`, `NetworkService`, `LocalService`, or `DOMAIN\user`. |
   | `--password`          | (unset)                       | Required when `--account` is a domain user.                 |
   | `--start-type`        | `auto`                        | One of `auto`, `manual`, `disabled`.                        |

### Status

```powershell
agent-shim service status                  # default --name agent-shim
agent-shim service status --name agent-shim-anthropic
```

The output shows the current SCM state (`Stopped` / `StartPending` / `Running` / `StopPending`), the process ID when running, the configured ImagePath, and the config path parsed out of it. **`Running` means the TCP listener is bound** — see "What 'Running' means" below.

### Start, stop, restart

```powershell
agent-shim service start          # → SCM reports Running once the port is bound
agent-shim service stop           # → graceful shutdown via axum + OTel drain
agent-shim service restart        # → stop, then start
```

`stop` triggers the same graceful-shutdown path used by Ctrl+C in foreground mode: the axum server stops accepting new connections, in-flight streams drain, OTel batches flush. The CLI polls SCM and returns once the service reaches `Stopped` (default 30-second timeout).

### Uninstall

```powershell
agent-shim service uninstall
```

`uninstall` issues a best-effort `Stop` if the service is running, then `DeleteService`. The binary on disk is untouched.

### What "Running" means

The Windows Service subcommands implement **port-bind-then-Running** semantics: SCM does not transition to `Running` until the public TCP listener is bound and accepting connections. If `bind` fails (e.g. port already in use), the service jumps straight to `Stopped` with a non-zero exit code, so `sc query` accurately reflects "the service is not serving traffic."

If you have the admin port enabled (`admin:` block in the config), it is bound after the public port; the SCM `Running` transition fires once the public port is up, so a slow admin port bind does not delay the status report.

### Logs

By default, the service writes JSON-formatted logs to:

```
C:\ProgramData\agent-shim\logs\agent-shim.log
```

with daily rotation and a 7-day retention. The directory is created automatically at first start. Override by setting `logging.file` in the config — see [Configuration → Logging](configuration.md#logging) and [Observability → File logging](observability.md#file-logging) for the full schema.

Service-launched processes have no console, so `stdout` is detached. Files are the only way to read logs unless you also configure OTel export.

### Multi-instance on a single machine

Register two services with different `--name` and config files:

```powershell
agent-shim service install --name agent-shim-anthropic --config "C:\...\anthropic.yaml"
agent-shim service install --name agent-shim-openai    --config "C:\...\openai.yaml"
```

Each service is independent; ensure their `server.port` values differ.

### Troubleshooting

| Symptom                                          | Likely cause                                                    | Fix                                                                                                  |
|--------------------------------------------------|-----------------------------------------------------------------|------------------------------------------------------------------------------------------------------|
| `install` exits with "requires administrator"    | Not running from elevated terminal                              | Right-click PowerShell → "Run as administrator"; retry.                                              |
| `install` rejects with "--config must be absolute" | Relative path passed                                          | Use a full path: `C:\path\to\gateway.yaml`.                                                          |
| `start` succeeds but service immediately stops   | Config validates but bind fails (port in use, permission, etc.) | Inspect the log file and Windows Event Viewer → Application → "agent-shim".                          |
| `status` shows `Running` but requests fail       | Listener bound but routes misconfigured                         | The service is healthy at the port level — the request itself has a routing problem; check logs.    |
| `stop` hangs                                     | In-flight streaming request preventing graceful drain           | Default timeout is 30 seconds. If stop still doesn't return, use `sc query` to confirm SCM state.    |

## Linux systemd

On Linux, agent-shim runs cleanly under systemd. A sample unit is shipped at [`deploy/agent-shim.service`](../deploy/agent-shim.service); the steps below assume you start from that file.

### Prerequisites

- A built `agent-shim` release binary. `cargo build --release -p agent-shim` produces `target/release/agent-shim`.
- A non-privileged system user named `agent-shim` for the service to run as.

### Install

```bash
# 1. System user (no login shell, no home).
sudo useradd --system --no-create-home --shell /usr/sbin/nologin agent-shim

# 2. Binary into a stable location.
sudo install -m 0755 target/release/agent-shim /usr/local/bin/agent-shim

# 3. Config directory (read-only for the service user).
sudo mkdir -p /etc/agent-shim
sudo cp config/gateway.example.yaml /etc/agent-shim/gateway.yaml   # then edit
sudo chmod 0640 /etc/agent-shim/gateway.yaml
sudo chown root:agent-shim /etc/agent-shim/gateway.yaml

# 4. Log directory (writable by the service user, if you want file logs).
sudo mkdir -p /var/log/agent-shim
sudo chown agent-shim:agent-shim /var/log/agent-shim

# 5. Install the systemd unit.
sudo cp deploy/agent-shim.service /etc/systemd/system/
sudo systemctl daemon-reload

# 6. Enable + start.
sudo systemctl enable --now agent-shim
```

### Status and logs

```bash
sudo systemctl status agent-shim
journalctl -u agent-shim -f
```

Logs go to journald by default (via stdout). If you want a parallel rolling file, add `logging.file` to your `/etc/agent-shim/gateway.yaml`:

```yaml
logging:
  file:
    path: /var/log/agent-shim/agent-shim.log
    format: json
    rotation: daily
    max_files: 7
```

The shipped unit's `ReadWritePaths=/var/log/agent-shim` allows the service to write there even with `ProtectSystem=strict`.

### Reloading config

```bash
sudo systemctl reload agent-shim
```

`ExecReload=/bin/kill -HUP $MAINPID` sends SIGHUP, which agent-shim already handles by re-reading the config file (see `crates/gateway/src/commands/serve.rs`). Mutable fields swap atomically; immutable fields (port, upstream set) require a full restart.

### Uninstall

```bash
sudo systemctl disable --now agent-shim
sudo rm /etc/systemd/system/agent-shim.service
sudo systemctl daemon-reload
# Optional cleanup:
sudo rm /usr/local/bin/agent-shim
sudo rm -rf /etc/agent-shim /var/log/agent-shim
sudo userdel agent-shim
```
