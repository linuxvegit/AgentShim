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
