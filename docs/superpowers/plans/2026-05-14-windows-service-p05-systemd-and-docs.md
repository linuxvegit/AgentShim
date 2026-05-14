# Phase 5: Linux systemd Example + Documentation Closeout Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a sample Linux systemd unit, complete deployment documentation, and update user-facing docs (README, CHANGELOG, configuration reference, observability guide). After this phase, an operator can deploy agent-shim as a service on either Windows or Linux by following the docs without reading the source.

**Architecture:** Pure documentation and configuration changes. No Rust modifications. The systemd unit reuses agent-shim's existing SIGHUP reload handler (no new code path).

**Tech Stack:** Markdown, systemd unit file syntax.

**Spec reference:** sections 6, 7, 11 (phase 5) of `docs/superpowers/specs/2026-05-14-windows-service-and-file-logging-design.md`.

**Depends on:** Phase 1–4 (the feature must be implemented before we describe how to use it).

---

## File Structure

| File | Responsibility | Status |
|------|----------------|--------|
| `deploy/agent-shim.service` | Sample systemd unit | Create |
| `docs/deployment.md` | Add "Windows Service" and "Linux systemd" sections | Modify |
| `docs/observability.md` | Add "File logging" subsection | Modify |
| `docs/configuration.md` | Document `logging.file` schema | Modify |
| `README.md` | Cross-reference deployment docs | Modify |
| `CHANGELOG.md` | Unreleased entries | Modify |
| `docs/superpowers/plans/2026-05-14-windows-service-acceptance.md` | Acceptance checklist (copy from spec section 9) | Create |

---

### Task 1: Add the systemd unit file

**Files:**
- Create: `deploy/agent-shim.service`

- [ ] **Step 1: Inspect existing deploy directory contents**

```bash
ls deploy/
```

Expected: `Dockerfile`, `docker-compose.yaml`.

- [ ] **Step 2: Create the systemd unit**

```ini
# deploy/agent-shim.service
#
# Sample systemd unit for running agent-shim as a long-lived service on
# Linux. See docs/deployment.md for installation instructions.

[Unit]
Description=AgentShim Gateway - Protocol-translating API gateway for AI agents
Documentation=https://github.com/anthropics/agent-shim
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=agent-shim
Group=agent-shim
ExecStart=/usr/local/bin/agent-shim serve --config /etc/agent-shim/gateway.yaml
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=5s

# Hardening: minimal sane defaults that don't interfere with agent-shim's
# work (outbound HTTPS, optional file logging under /var/log/agent-shim).
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/log/agent-shim
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
LockPersonality=true
RestrictSUIDSGID=true

# File descriptor limit: gateway holds at least 2 FDs per upstream
# connection plus inbound clients. 65536 covers most workloads.
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

- [ ] **Step 3: Lint-check the unit file**

If `systemd-analyze` is available locally:

```bash
systemd-analyze verify deploy/agent-shim.service
```

Expected: clean (or only warnings about absolute paths not existing on the developer machine — those are deployment-time concerns).

If the tool is unavailable, skip this check.

- [ ] **Step 4: Commit**

```bash
git add deploy/agent-shim.service
git commit -m "feat(deploy): sample systemd unit for Linux deployments"
```

---

### Task 2: Add Windows + Linux deployment sections to `docs/deployment.md`

**Files:**
- Modify: `docs/deployment.md`

- [ ] **Step 1: Read the current deployment doc**

```bash
cat docs/deployment.md
```

Note the existing structure — the new sections should match the heading style and prose tone.

- [ ] **Step 2: Append two new top-level sections**

Add at the end of the file (or in whatever logical position fits the existing structure):

````markdown
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

````

- [ ] **Step 3: Verify markdown renders sanely**

```bash
# Spot-check: head and tail of the file should still show the original
# content; the new sections are appended.
head -30 docs/deployment.md
tail -100 docs/deployment.md
```

Expected: original content intact, new sections at the bottom.

- [ ] **Step 4: Commit**

```bash
git add docs/deployment.md
git commit -m "docs(deployment): Windows Service and Linux systemd sections"
```

---

### Task 3: Update `docs/observability.md` with file logging documentation

**Files:**
- Modify: `docs/observability.md`

- [ ] **Step 1: Inspect the current observability doc**

```bash
cat docs/observability.md
```

Identify where the "Logging" section lives.

- [ ] **Step 2: Add a "File logging" subsection**

Insert under the existing logging section (or as a new top-level section if logging is currently rolled into a broader heading):

````markdown
### File logging

Set `logging.file` in the gateway config to write log events to a rolling file in addition to stdout:

```yaml
logging:
  format: pretty           # stdout format; unchanged
  filter: info
  file:                    # NEW: optional
    path: /var/log/agent-shim/agent-shim.log
    format: json           # file format; independent of stdout
    rotation: daily        # daily | hourly | never
    max_files: 7           # 0 = unlimited
```

#### Fields

| Field          | Type     | Default | Notes                                                                            |
|----------------|----------|---------|----------------------------------------------------------------------------------|
| `path`         | path     | —       | Required when `file:` is set. Use an **absolute path** on Windows (under a service the cwd is `C:\\Windows\\System32`). |
| `format`       | enum     | `json`  | `json` for structured ingestion, `pretty` for human-readable.                    |
| `rotation`     | enum     | `daily` | `daily` produces `agent-shim.log.YYYY-MM-DD`; `hourly` adds the hour; `never` keeps a single file. |
| `max_files`    | integer  | `7`     | Retention: oldest rolled files are deleted once this many exist. `0` disables retention. |

#### Behavior

- **Async writes.** File writes happen on a background thread via `tracing_appender::non_blocking`. HTTP handlers never block on disk I/O — important for streaming responses.
- **Buffering.** The non-blocking writer buffers events in a bounded channel. On a graceful shutdown (Ctrl+C, SIGTERM, or SCM Stop), the worker guard drops and flushes pending events.
- **SIGKILL caveat.** If the process is `SIGKILL`-ed (or the OS terminates it ungracefully), buffered events are lost. This is the trade-off for async writes. Critical state should be persisted elsewhere.
- **Stdout still writes.** The file layer is additive. To suppress stdout in service mode, simply ignore it — service-spawned processes have no console attached.

#### Service-mode default

When agent-shim is started by the Windows Service Control Manager (via `agent-shim service run`, invoked by SCM), it injects a default `logging.file` block if the loaded config does not have one:

```yaml
file:
  path: C:\ProgramData\agent-shim\logs\agent-shim.log
  format: json
  rotation: daily
  max_files: 7
```

The user's explicit `logging.file` always takes precedence. The fallback is purely a "give the operator somewhere to look" safety net.
````

- [ ] **Step 3: Verify the rendered doc looks right**

```bash
head -40 docs/observability.md
grep -A 30 "File logging" docs/observability.md
```

- [ ] **Step 4: Commit**

```bash
git add docs/observability.md
git commit -m "docs(observability): file logging subsection (logging.file schema)"
```

---

### Task 4: Update `docs/configuration.md` with the `logging.file` schema

**Files:**
- Modify: `docs/configuration.md`

- [ ] **Step 1: Read current config doc**

```bash
cat docs/configuration.md
```

Identify the "Logging" section.

- [ ] **Step 2: Add `logging.file` to the schema reference**

In the "Logging" table or YAML reference block, add the new field. Match the style of nearby entries — if the existing doc uses tables, add a row; if it uses a YAML walkthrough, extend that. Example, if a table is in use:

```markdown
### `logging`

| Field    | Type           | Default                        | Description                                            |
|----------|----------------|--------------------------------|--------------------------------------------------------|
| `format` | enum           | `pretty`                       | `pretty` or `json`. Stdout format.                     |
| `filter` | string         | `info`                         | `EnvFilter` directive (e.g. `info,agent_shim=debug`).  |
| `file`   | object or null | `null`                         | Optional rolling file output. See [File logging](observability.md#file-logging) for the full schema and behavior. |

#### `logging.file` (optional)

| Field         | Type    | Default | Description                                                                  |
|---------------|---------|---------|------------------------------------------------------------------------------|
| `path`        | path    | —       | Required. Absolute path strongly recommended.                                |
| `format`      | enum    | `json`  | `json` or `pretty`. Independent of `logging.format`.                         |
| `rotation`    | enum    | `daily` | `daily`, `hourly`, or `never`.                                               |
| `max_files`   | integer | `7`     | Retention count; `0` disables retention.                                     |
```

If the existing config doc uses prose + inline YAML rather than tables, write the equivalent prose paragraph.

- [ ] **Step 3: Commit**

```bash
git add docs/configuration.md
git commit -m "docs(configuration): document logging.file schema"
```

---

### Task 5: Update `README.md` with a service deployment cross-reference

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Identify the Quick Start section**

```bash
head -80 README.md
```

- [ ] **Step 2: Add a cross-reference line below the existing run command**

Find the line that shows `agent-shim serve --config config/gateway.yaml` (the README's quick-start). Below it, add:

```markdown
Running as a long-lived service? See [docs/deployment.md](docs/deployment.md) for Windows Service and Linux systemd setup.
```

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs(readme): cross-reference deployment guide"
```

---

### Task 6: Update `CHANGELOG.md`

**Files:**
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Inspect the current changelog**

```bash
head -30 CHANGELOG.md
```

Identify whether there's an `[Unreleased]` section already; if not, add one above the most recent versioned entry.

- [ ] **Step 2: Add entries**

Under `## [Unreleased]`:

```markdown
### Added

- **Windows Service support.** New `agent-shim service install|uninstall|start|stop|restart|status` subcommands on Windows. Run agent-shim as a long-lived background service with SCM state visibility (port-bind-then-Running semantics) and graceful shutdown via the existing axum drain path. See `docs/deployment.md`.
- **Cross-platform file logging.** New optional `logging.file` config block (path, format, rotation, max_files). Backed by `tracing-appender` with daily/hourly rotation and async writes. See `docs/observability.md#file-logging`.
- **Linux systemd example** at `deploy/agent-shim.service`. Reuses the existing SIGHUP reload handler for `systemctl reload`.
```

- [ ] **Step 3: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs(changelog): windows service + file logging entries [Unreleased]"
```

---

### Task 7: Materialize the manual acceptance checklist

**Files:**
- Create: `docs/superpowers/plans/2026-05-14-windows-service-acceptance.md`

The spec has a manual acceptance checklist (section 9). We copy it into a standalone runnable doc so the person doing the final verification can tick boxes.

- [ ] **Step 1: Create the file**

```markdown
# Windows Service + File Logging — Manual Acceptance Checklist

Run through this checklist on every supported platform before merging the feature into `master`. Each box represents an observable, falsifiable outcome.

## Windows (elevated PowerShell)

- [ ] `agent-shim service install --config C:\full\path\gateway.yaml` succeeds; `sc query agent-shim` reports `STOPPED`.
- [ ] `agent-shim service install --config C:\does\not\exist.yaml` fails; `sc query agent-shim` shows the service does NOT exist.
- [ ] `agent-shim service install --config C:\path\to\gateway-with-typo.yaml` (file has a YAML schema error) fails; `sc query agent-shim` shows the service does NOT exist.
- [ ] `agent-shim service install --config gateway.yaml` (relative path) fails with a clear "must be absolute" message.
- [ ] Non-elevated terminal: `agent-shim service install ...` exits non-zero with "requires administrator privileges".
- [ ] `agent-shim service start` → `sc query agent-shim` reports `RUNNING`; `netstat -an | findstr :8787` shows the bound port.
- [ ] `agent-shim service status` shows: State=Running, PID matches `tasklist`, ImagePath includes `service run`, and the Config field resolves to the absolute path you passed at install.
- [ ] Default log file appears at `C:\ProgramData\agent-shim\logs\agent-shim.log` (when no `logging.file` was set), with JSON entries.
- [ ] `agent-shim service stop` returns within 5 seconds; `sc query` reports `STOPPED`; `tasklist /FI "IMAGENAME eq agent-shim.exe"` shows no process.
- [ ] Send a request through the gateway and verify a streaming response (e.g. `/v1/messages`) works end-to-end under service mode.
- [ ] Cross-midnight test: leave the service running across midnight; `agent-shim.log.YYYY-MM-DD` for the previous day exists alongside the new `agent-shim.log`.
- [ ] `agent-shim service restart` performs stop+start with no orphan process.
- [ ] `agent-shim service uninstall` removes the registration; `sc query agent-shim` reports the service does not exist.
- [ ] Multi-instance: install two services with `--name agent-shim-a` / `--name agent-shim-b`, different ports; start both; both reach Running; both serve traffic independently; stop and uninstall both.

## Linux

- [ ] `cargo build --workspace` succeeds.
- [ ] `cargo tree -p agent-shim | grep -i "windows[-_]"` returns nothing.
- [ ] `agent-shim --help` does not show a `service` subcommand.
- [ ] Following `docs/deployment.md` Linux instructions: `systemctl status agent-shim` reports active; `journalctl -u agent-shim` shows logs.
- [ ] `systemctl reload agent-shim` triggers config reload (visible via tracing target `agent_shim::reload`).
- [ ] With `logging.file` configured to `/var/log/agent-shim/agent-shim.log`, the file appears and rotates daily.

## macOS

- [ ] `cargo build --workspace` succeeds.
- [ ] `agent-shim --help` does not show `service`.
- [ ] `logging.file` configured: file appears at the path; daily rotation effective when forced (use `rotation: hourly` to test within a single run).
```

- [ ] **Step 2: Commit**

```bash
git add docs/superpowers/plans/2026-05-14-windows-service-acceptance.md
git commit -m "docs(acceptance): manual checklist for windows service + file logging"
```

---

### Task 8: Final verification

- [ ] **Step 1: Render the full plan + spec + acceptance list and skim**

```bash
ls docs/superpowers/specs/2026-05-14-*
ls docs/superpowers/plans/2026-05-14-*
```

Expected: one spec file, five plan files (p01–p05), one acceptance checklist file.

- [ ] **Step 2: Run the full CI pipeline**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo deny check
```

Expected: all green. No code changed in Phase 5 — this is a sanity check that the docs-only commits didn't accidentally include code.

- [ ] **Step 3: Run the manual acceptance checklist on Windows (admin) and Linux**

Tick boxes in `docs/superpowers/plans/2026-05-14-windows-service-acceptance.md` as you go. File any failures as separate follow-up issues; do not merge the feature with open boxes unticked.

- [ ] **Step 4: Confirm git log**

```bash
git log --oneline -15
```

Expected: a clean sequence of Phase 5 commits.

---

## Done Criteria

- `deploy/agent-shim.service` exists and is a valid systemd unit.
- `docs/deployment.md` has complete Windows Service and Linux systemd sections.
- `docs/observability.md` documents `logging.file` schema and async-write caveats.
- `docs/configuration.md` includes the `logging.file` field in its schema reference.
- `README.md` points users at the deployment doc.
- `CHANGELOG.md`'s `[Unreleased]` section lists the new features.
- Manual acceptance checklist file exists for someone to run through.
- All acceptance boxes are ticked on Windows + Linux before merging.

Phase 5 closes out the windows-service-and-file-logging feature. After Phase 5 the feature is shippable: install, file logging, status, start/stop, documentation, and manual verification are all complete.
