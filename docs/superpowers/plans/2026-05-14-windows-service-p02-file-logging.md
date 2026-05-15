# Phase 2: Cross-Platform File Logging Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an optional `logging.file` configuration that lets operators on any platform (Linux, macOS, Windows) emit logs to a rolling file in parallel to stdout, using `tracing-appender` for daily/hourly rotation and asynchronous writes.

**Architecture:** Extend `LoggingConfig` with an optional `FileLoggingConfig` struct (path, format, rotation, max_files). In `agent_shim_observability::init`, build a `tracing-subscriber` layer wrapping a `RollingFileAppender` behind `tracing_appender::non_blocking`. Return the `WorkerGuard` from `init` so the caller (`commands::serve::run_core`) keeps it alive for the process lifetime. No platform gates — same code path on Linux, macOS, and Windows.

**Tech Stack:** Rust, `tracing-subscriber`, new dependency `tracing-appender = "0.2"`.

**Spec reference:** `docs/superpowers/specs/2026-05-14-windows-service-and-file-logging-design.md` section 5, plus risk notes in section 10.

**Depends on:** Phase 1 (`run_core` exists and is the wire-in point for the `WorkerGuard`).

---

## File Structure

| File | Responsibility | Status |
|------|----------------|--------|
| `Cargo.toml` (workspace) | Add `tracing-appender = "0.2"` to `[workspace.dependencies]` | Modify |
| `crates/observability/Cargo.toml` | Reference `tracing-appender = { workspace = true }` | Modify |
| `crates/config/src/schema.rs` | Add `FileLoggingConfig`, `RotationPolicy`; wire `LoggingConfig::file: Option<FileLoggingConfig>` | Modify |
| `crates/observability/src/tracing_setup.rs` | Build file layer, manage `WorkerGuard`, expose it on `TracingHandles` | Modify |
| `crates/gateway/src/commands/serve.rs` | Keep `TracingHandles` alive (already does — verify the guard field survives drop) | Modify (minor) |
| `crates/observability/tests/file_logging_smoke.rs` | New integration test: writes a log, asserts file appears with expected JSON content | Create |
| `config/gateway.example.yaml` | Add commented-out `logging.file:` example | Modify |

---

### Task 1: Add `tracing-appender` to the workspace

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/observability/Cargo.toml`

- [ ] **Step 1: Edit workspace `Cargo.toml`**

Open `Cargo.toml` at the repo root. Find the `[workspace.dependencies]` table (currently ends around line 49). Add a new line, alphabetically sensible — place it near other tracing deps:

```toml
tracing-appender = "0.2"
```

The final ordering does not matter for cargo, but keep adjacent tracing entries together:

```toml
tracing = "0.1"
tracing-appender = "0.2"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json", "fmt"] }
```

- [ ] **Step 2: Edit `crates/observability/Cargo.toml`**

Add the dependency under the existing `[dependencies]` block, after `tracing-subscriber.workspace = true`:

```toml
tracing-appender = { workspace = true }
```

- [ ] **Step 3: Build to verify the dependency resolves**

Run: `cargo build -p agent-shim-observability`
Expected: pulls `tracing-appender` from crates.io, builds cleanly.

- [ ] **Step 4: Run `cargo deny check`**

Run: `cargo deny check`
Expected: PASS. `tracing-appender` is MIT-licensed (same as `tracing`), matches existing allowlist.

If it FAILS with a license advisory, the only adjustment is to add `tracing-appender` to the existing `tracing*` license exceptions. **Do not** disable advisories.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/observability/Cargo.toml Cargo.lock
git commit -m "deps(observability): add tracing-appender 0.2 for rolling file logs"
```

---

### Task 2: Add `FileLoggingConfig` and `RotationPolicy` types to the config schema (TDD red)

**Files:**
- Modify: `crates/config/src/schema.rs`

The schema lives in a shared crate; the new types must roundtrip through serde correctly. We write the failing roundtrip test first, then add the types.

- [ ] **Step 1: Add the failing test**

Open `crates/config/src/schema.rs`. Find the existing test module at the bottom (the test currently around line 679 named `*_logging_config_*` or similar). Add a new test inside `#[cfg(test)] mod tests`:

```rust
#[test]
fn file_logging_config_roundtrips_defaults() {
    // Default for FileLoggingConfig is "no file logging" — the parent
    // LoggingConfig.file field is Option<FileLoggingConfig>, default None.
    let yaml = r#"
format: json
filter: info
file:
  path: /var/log/agent-shim/agent-shim.log
"#;
    let cfg: LoggingConfig = serde_yaml::from_str(yaml).unwrap();
    let file = cfg.file.expect("file: section should parse");
    assert_eq!(file.path, std::path::PathBuf::from("/var/log/agent-shim/agent-shim.log"));
    assert_eq!(file.format, LogFormat::Json);
    assert_eq!(file.rotation, RotationPolicy::Daily);
    assert_eq!(file.max_files, 7);
}

#[test]
fn file_logging_config_explicit_all_fields() {
    let yaml = r#"
format: pretty
filter: debug
file:
  path: /tmp/agent.log
  format: pretty
  rotation: hourly
  max_files: 24
"#;
    let cfg: LoggingConfig = serde_yaml::from_str(yaml).unwrap();
    let file = cfg.file.unwrap();
    assert_eq!(file.format, LogFormat::Pretty);
    assert_eq!(file.rotation, RotationPolicy::Hourly);
    assert_eq!(file.max_files, 24);
}

#[test]
fn rotation_policy_serde_snake_case() {
    assert_eq!(
        serde_yaml::from_str::<RotationPolicy>("daily").unwrap(),
        RotationPolicy::Daily,
    );
    assert_eq!(
        serde_yaml::from_str::<RotationPolicy>("hourly").unwrap(),
        RotationPolicy::Hourly,
    );
    assert_eq!(
        serde_yaml::from_str::<RotationPolicy>("never").unwrap(),
        RotationPolicy::Never,
    );
}

#[test]
fn file_logging_unknown_field_rejected() {
    // deny_unknown_fields contract: typos must fail at startup.
    let yaml = r#"
format: json
filter: info
file:
  path: /tmp/x.log
  weekly: true
"#;
    let result: Result<LoggingConfig, _> = serde_yaml::from_str(yaml);
    assert!(result.is_err(), "unknown field should be rejected");
}
```

Add `use serde_yaml;` to the test module imports if not already present. The crate may already use serde_json for tests — check the existing test imports. If `serde_yaml` is not in `[dev-dependencies]`, add it:

```bash
# Inspect first:
grep -n "serde_yaml\|serde_json" crates/config/Cargo.toml
```

If `serde_yaml` is missing, add to `crates/config/Cargo.toml`:

```toml
[dev-dependencies]
serde_yaml = "0.9"
```

- [ ] **Step 2: Run the failing tests**

Run: `cargo nextest run -p agent-shim-config`
Expected: COMPILE ERROR — `FileLoggingConfig` and `RotationPolicy` do not exist yet.

- [ ] **Step 3: Commit the failing test**

```bash
git add crates/config/src/schema.rs crates/config/Cargo.toml
git commit -m "test(config): failing roundtrip tests for FileLoggingConfig (TDD red)"
```

---

### Task 3: Implement `FileLoggingConfig` and `RotationPolicy` (TDD green)

**Files:**
- Modify: `crates/config/src/schema.rs`

- [ ] **Step 1: Add `use std::path::PathBuf;` near the top of the file** (if not already present)

- [ ] **Step 2: Modify the existing `LoggingConfig` struct**

Find the existing definition (around line 164):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    #[serde(default)]
    pub format: LogFormat,
    #[serde(default = "default_filter")]
    pub filter: String,
}
```

Add the new `file` field:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    #[serde(default)]
    pub format: LogFormat,
    #[serde(default = "default_filter")]
    pub filter: String,
    /// Optional file logging configuration. When set, log events are
    /// written to a rolling file in addition to stdout. See
    /// `FileLoggingConfig` for fields. Phase 2 of windows-service spec.
    #[serde(default)]
    pub file: Option<FileLoggingConfig>,
}
```

Also update `impl Default for LoggingConfig` immediately below to include `file: None`:

```rust
impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            format: LogFormat::default(),
            filter: default_filter(),
            file: None,
        }
    }
}
```

- [ ] **Step 3: Add `FileLoggingConfig` and `RotationPolicy` immediately after `LoggingConfig`**

```rust
/// File-logging configuration. When present on `LoggingConfig::file`,
/// a `tracing-appender` rolling file writer is installed in addition
/// to the stdout layer. Async writes via `tracing_appender::non_blocking`;
/// the `WorkerGuard` lives on `TracingHandles` and is dropped at process
/// exit to flush. SIGKILL bypasses the flush — documented in
/// `docs/observability.md`.
///
/// All fields except `path` have defaults; `path` is required.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileLoggingConfig {
    /// Absolute log file path. The parent directory is created at
    /// startup if it does not exist; permission failure is fatal.
    pub path: PathBuf,
    /// File log format. Independent of `LoggingConfig::format`
    /// (which controls stdout). Defaults to `json` because files
    /// are usually consumed by tooling, not humans.
    #[serde(default = "default_file_format")]
    pub format: LogFormat,
    /// Rotation cadence: `daily`, `hourly`, or `never`.
    #[serde(default)]
    pub rotation: RotationPolicy,
    /// Maximum number of log files retained (current file + history).
    /// `0` means unlimited; otherwise the oldest rolled file is deleted
    /// after rotation. Default: 7 — one week of daily logs.
    #[serde(default = "default_max_files")]
    pub max_files: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RotationPolicy {
    #[default]
    Daily,
    Hourly,
    Never,
}

fn default_file_format() -> LogFormat {
    LogFormat::Json
}

fn default_max_files() -> usize {
    7
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo nextest run -p agent-shim-config`
Expected: all four new tests PASS plus all existing config tests pass.

- [ ] **Step 5: Run clippy on the config crate**

Run: `cargo clippy -p agent-shim-config --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/config/src/schema.rs
git commit -m "feat(config): add logging.file schema (FileLoggingConfig, RotationPolicy)"
```

---

### Task 4: Write the failing file-logging integration test

**Files:**
- Create: `crates/observability/tests/file_logging_smoke.rs`

- [ ] **Step 1: Add `tempfile` and `serde_json` to observability dev-deps if missing**

Inspect `crates/observability/Cargo.toml`. The `[dev-dependencies]` block currently has `tokio`, `prometheus-parse`, `axum`, `opentelemetry_sdk`, and `serde_json`. Add:

```toml
tempfile = "3"
```

If `tempfile` is already in the workspace `Cargo.toml`'s `[workspace.dependencies]`, prefer `tempfile = { workspace = true }`.

- [ ] **Step 2: Write the failing integration test**

```rust
// crates/observability/tests/file_logging_smoke.rs
//
// Verifies that the file-logging layer wired up by
// agent_shim_observability::init actually writes JSON log records to the
// configured path. Uses a tempdir so we don't litter the filesystem.

use agent_shim_config::schema::{
    FileLoggingConfig, LogFormat, LoggingConfig, RotationPolicy,
};

#[test]
fn file_logging_writes_json_events_to_configured_path() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("agent-shim.log");

    let cfg = LoggingConfig {
        format: LogFormat::Pretty,           // stdout: pretty, irrelevant for this test
        filter: "info".to_string(),
        file: Some(FileLoggingConfig {
            path: log_path.clone(),
            format: LogFormat::Json,
            rotation: RotationPolicy::Daily,
            max_files: 7,
        }),
    };

    let handles = agent_shim_observability::init(&cfg, None);
    // file_guard must be Some — that's the invariant the file layer
    // relies on for flush at drop.
    assert!(
        handles.file_guard.is_some(),
        "file_guard missing — non_blocking writer will drop events"
    );

    // Emit a few events under a target that the filter matches.
    tracing::info!(test = "file_logging_smoke", "hello from the test");
    tracing::warn!(scenario = "rotation", "second event");

    // Drop the guard to force a flush. Without dropping, the
    // non_blocking writer's background thread may still be holding
    // events in its channel.
    drop(handles);

    // tracing-appender filenames are `<prefix>.YYYY-MM-DD` (daily) — find
    // any file in the tempdir starting with our prefix.
    let prefix = log_path.file_name().unwrap().to_string_lossy().into_owned();
    let entries: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&prefix)
        })
        .collect();
    assert!(!entries.is_empty(), "no log file produced under {:?}", tmp.path());

    // Read the first matching file and parse each line as JSON.
    let content = std::fs::read_to_string(entries[0].path()).unwrap();
    let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    assert!(!lines.is_empty(), "log file is empty: {:?}", entries[0].path());

    let mut found_hello = false;
    let mut found_second = false;
    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("log line is not JSON: {line:?} ({e})"));
        let msg = v
            .pointer("/fields/message")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        if msg == "hello from the test" { found_hello = true; }
        if msg == "second event"        { found_second = true; }
    }
    assert!(found_hello, "first event missing from log file: {content}");
    assert!(found_second, "second event missing from log file: {content}");
}
```

- [ ] **Step 3: Run the failing test**

Run: `cargo nextest run -p agent-shim-observability --test file_logging_smoke`
Expected: COMPILE ERROR — `TracingHandles` does not have a `file_guard` field yet, and `init` does not build a file layer.

- [ ] **Step 4: Commit the failing test**

```bash
git add crates/observability/tests/file_logging_smoke.rs crates/observability/Cargo.toml
git commit -m "test(observability): failing integration test for file logging (TDD red)"
```

---

### Task 5: Implement the file-logging layer in `tracing_setup.rs` (TDD green)

**Files:**
- Modify: `crates/observability/src/tracing_setup.rs`

- [ ] **Step 1: Update the imports at the top of the file**

```rust
use agent_shim_config::schema::{
    FileLoggingConfig, LogFormat, LoggingConfig, OtelConfig, RotationPolicy,
};
use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{fmt, prelude::*, EnvFilter, Layer, Registry};

use crate::otel::{build_layer, OtelHandle};
```

- [ ] **Step 2: Add `file_guard` to `TracingHandles`**

Replace the existing `TracingHandles` struct definition:

```rust
/// Tracing handles whose lifetimes must extend to process shutdown.
/// Hold this in `main.rs` (or `commands::serve::run_core`) and drop /
/// `shutdown()` it before exit so the OTLP batch exporter drains and
/// the non-blocking file writer flushes buffered events.
pub struct TracingHandles {
    pub otel: Option<OtelHandle>,
    pub file_guard: Option<WorkerGuard>,
}
```

- [ ] **Step 3: Update `init` to wire the file layer**

Replace the body of `pub fn init(log: &LoggingConfig, otel_cfg: Option<&OtelConfig>) -> TracingHandles`:

```rust
pub fn init(log: &LoggingConfig, otel_cfg: Option<&OtelConfig>) -> TracingHandles {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::try_new(&log.filter).unwrap_or_else(|_| EnvFilter::new("info"))
    });

    // Optional OTel layer (existing behavior).
    let (otel_layer, otel_handle) = if let Some(cfg) = otel_cfg {
        match build_layer::<Registry>(cfg) {
            Ok(Some((layer, handle))) => (Some(layer), Some(handle)),
            Ok(None) => (None, None),
            Err(e) => {
                eprintln!("FATAL: OTel exporter init failed: {e}");
                std::process::exit(2);
            }
        }
    } else {
        (None, None)
    };

    // Optional file layer (new in Phase 2).
    let (file_layer, file_guard) = build_file_layer(log.file.as_ref());

    let registry = tracing_subscriber::registry()
        .with(otel_layer)
        .with(file_layer)
        .with(filter);

    match log.format {
        LogFormat::Json => {
            let _ = registry.with(fmt::layer().json()).try_init();
        }
        LogFormat::Pretty => {
            let _ = registry.with(fmt::layer().pretty()).try_init();
        }
    }

    TracingHandles {
        otel: otel_handle,
        file_guard,
    }
}
```

- [ ] **Step 4: Add the `build_file_layer` helper**

Append below `init`:

```rust
/// Build the optional file-logging layer.
///
/// Behavior:
/// - `cfg = None`         → returns `(None, None)`; subscriber has no file layer.
/// - `cfg = Some(...)`    → ensures the parent directory exists (fatal on failure,
///                          matching OTel init's behavior), constructs a
///                          `RollingFileAppender` with the configured rotation
///                          and `max_log_files` retention, and wraps it in
///                          `tracing_appender::non_blocking` for async writes.
///
/// The returned `WorkerGuard` must outlive every emitted event; the caller
/// stores it on `TracingHandles.file_guard` so it lives until process
/// shutdown drops the handles.
fn build_file_layer(
    cfg: Option<&FileLoggingConfig>,
) -> (
    Option<Box<dyn Layer<Registry> + Send + Sync>>,
    Option<WorkerGuard>,
) {
    let Some(cfg) = cfg else {
        return (None, None);
    };

    // 1. Resolve directory and filename prefix.
    let dir = cfg.path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| {
        // `path` was a bare filename (no parent). Use cwd.
        std::path::PathBuf::from(".")
    });
    let prefix = match cfg.path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => {
            eprintln!(
                "FATAL: logging.file.path has no filename component: {:?}",
                cfg.path
            );
            std::process::exit(2);
        }
    };

    // 2. Create directory if needed. Fatal on failure (matches OTel behavior).
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "FATAL: cannot create log directory {:?}: {}",
            dir, e
        );
        std::process::exit(2);
    }

    // 3. Optional warning for relative paths — they resolve against cwd,
    // which is C:\\Windows\\System32 under Windows Service. Documented in
    // the spec; we surface it loudly here.
    if cfg.path.is_relative() {
        eprintln!(
            "WARNING: logging.file.path {:?} is relative — under Windows \
             Service the cwd is C:\\Windows\\System32. Use an absolute path.",
            cfg.path
        );
    }

    // 4. Build the rolling appender.
    let rotation = match cfg.rotation {
        RotationPolicy::Daily => Rotation::DAILY,
        RotationPolicy::Hourly => Rotation::HOURLY,
        RotationPolicy::Never => Rotation::NEVER,
    };
    let mut builder = RollingFileAppender::builder()
        .rotation(rotation)
        .filename_prefix(prefix);
    if cfg.max_files > 0 {
        builder = builder.max_log_files(cfg.max_files);
    }
    let appender = match builder.build(&dir) {
        Ok(a) => a,
        Err(e) => {
            eprintln!(
                "FATAL: cannot initialize rolling file appender at {:?}: {}",
                dir, e
            );
            std::process::exit(2);
        }
    };

    // 5. Wrap in non_blocking and build the layer.
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
    let layer: Box<dyn Layer<Registry> + Send + Sync> = match cfg.format {
        LogFormat::Json => Box::new(fmt::layer().json().with_writer(non_blocking)),
        LogFormat::Pretty => Box::new(fmt::layer().with_writer(non_blocking)),
    };

    (Some(layer), Some(guard))
}
```

- [ ] **Step 5: Update the in-file unit tests**

The existing tests (`init_pretty_no_otel_does_not_panic`, etc.) assert `handles.otel.is_none()`. They will now need to also handle the new `file_guard` field, but the field is `None` when `log.file` is `None`, so they should keep passing without changes. Verify the existing tests still compile:

```rust
#[test]
fn init_with_disabled_otel_does_not_panic() {
    let cfg = LoggingConfig {
        format: LogFormat::Pretty,
        filter: "info".to_string(),
        file: None,
    };
    let otel = OtelConfig::default();
    let handles = init(&cfg, Some(&otel));
    assert!(handles.otel.is_none(), "no endpoint → no OtelHandle");
    assert!(handles.file_guard.is_none(), "no file cfg → no guard");
}
```

Adjust the other two existing tests (`init_pretty_no_otel_does_not_panic`, `init_json_no_otel_does_not_panic`) to construct `LoggingConfig` with the new `file: None` field explicitly (or use `..Default::default()` — but the struct doesn't derive Default in the existing code, so explicit is safer):

```rust
let cfg = LoggingConfig {
    format: LogFormat::Pretty,
    filter: "info".to_string(),
    file: None,
};
```

- [ ] **Step 6: Run the failing integration test, expecting GREEN**

Run: `cargo nextest run -p agent-shim-observability`
Expected: `file_logging_writes_json_events_to_configured_path` PASS plus all existing unit tests PASS.

If the JSON assertion fails because the log field path is `/fields/message` vs something else, inspect a printed line manually:

```bash
cargo nextest run -p agent-shim-observability --test file_logging_smoke --no-capture
```

and adjust the JSON pointer in the test (not in the implementation) to match the actual schema emitted by `tracing-subscriber`'s JSON formatter. The most common alternatives are `/fields/message` and `/message`. Pick the one that matches.

- [ ] **Step 7: Run the workspace test suite**

Run: `cargo nextest run --workspace`
Expected: all green. Phase 1's `serve_core_smoke.rs` continues to pass; no behavior change for callers that pass `file: None`.

- [ ] **Step 8: Run clippy and fmt**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 9: Commit**

```bash
git add crates/observability/src/tracing_setup.rs
git commit -m "feat(observability): rolling file log layer behind logging.file config"
```

---

### Task 6: Verify `run_core` keeps the `WorkerGuard` alive

**Files:**
- Modify (if needed): `crates/gateway/src/commands/serve.rs`

Phase 1's `run_core` already stores the return value of `agent_shim_observability::init` in a local `tracing_handles` and drops it at the bottom (`if let Some(otel) = tracing_handles.otel { otel.shutdown(); }` leaves the rest of the struct, including the new `file_guard`, in scope until function return). Verify:

- [ ] **Step 1: Read `run_core` and confirm `tracing_handles` is held across the await of the serve loop**

Run: `grep -n "tracing_handles" crates/gateway/src/commands/serve.rs`
Expected: `let tracing_handles = agent_shim_observability::init(...)` near the top, and the variable is referenced at the bottom (`if let Some(otel) = tracing_handles.otel`).

The current pattern destructures `otel` out but leaves the rest of the handle (including `file_guard`) on the stack. That's exactly what we want — `file_guard` drops at the end of `run_core`, flushing the writer.

- [ ] **Step 2: Write an integration test that proves end-to-end file logging works through `run_core`**

Append to `crates/gateway/tests/serve_core_smoke.rs`:

```rust
#[tokio::test]
async fn run_core_writes_log_file_when_configured() {
    use agent_shim_config::schema::{FileLoggingConfig, LogFormat, RotationPolicy};

    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("agent-shim.log");

    let mut cfg = ephemeral_config();
    cfg.logging.file = Some(FileLoggingConfig {
        path: log_path.clone(),
        format: LogFormat::Json,
        rotation: RotationPolicy::Daily,
        max_files: 7,
    });

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        run_core(cfg, None, async move { let _ = shutdown_rx.await; }, |_| {}).await
    });

    // Wait for the gateway to bind and emit at least one tracing event.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let _ = shutdown_tx.send(());
    let _ = server_task.await.unwrap();

    // File should exist under the configured directory.
    let prefix = log_path.file_name().unwrap().to_string_lossy().into_owned();
    let entries: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .collect();
    assert!(!entries.is_empty(), "no log file produced in {:?}", tmp.path());
}
```

If `tempfile` is not already a dev-dep of `crates/gateway`, add it:

```bash
grep tempfile crates/gateway/Cargo.toml
```

It's already there (line 67 in the current Cargo.toml).

- [ ] **Step 3: Run the new test**

Run: `cargo nextest run -p agent-shim --test serve_core_smoke run_core_writes_log_file_when_configured`
Expected: PASS.

- [ ] **Step 4: Run the full workspace test suite**

Run: `cargo nextest run --workspace`
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add crates/gateway/tests/serve_core_smoke.rs
git commit -m "test(gateway): run_core writes file log when logging.file configured"
```

---

### Task 7: Update `config/gateway.example.yaml` with a commented-out file logging block

**Files:**
- Modify: `config/gateway.example.yaml`

- [ ] **Step 1: Inspect the current example config**

Run: `cat config/gateway.example.yaml`
Expected: shows the logging block somewhere in the YAML.

- [ ] **Step 2: Add a commented-out `file:` block under `logging:`**

Locate the existing `logging:` section. Add immediately below the existing keys (preserve any existing indentation style — spaces, 2-space):

```yaml
logging:
  format: pretty
  filter: info
  # Optional file logging. When set, log events are written to a rolling
  # file in addition to stdout. Use an ABSOLUTE path on Windows (under
  # a service the cwd is C:\Windows\System32). Daily rotation with
  # 7-day retention is a reasonable default; tune for your platform.
  #
  # file:
  #   path: "/var/log/agent-shim/agent-shim.log"   # Linux
  #   # path: "C:\\ProgramData\\agent-shim\\logs\\agent-shim.log"   # Windows
  #   format: json
  #   rotation: daily     # daily | hourly | never
  #   max_files: 7
```

- [ ] **Step 3: Verify the example still parses**

Run: `cargo run -p agent-shim -- validate-config --config config/gateway.example.yaml`
Expected: validation passes. (The commented block is YAML comments, not parsed.)

- [ ] **Step 4: Commit**

```bash
git add config/gateway.example.yaml
git commit -m "docs(config): example logging.file block in gateway.example.yaml"
```

---

### Task 8: Verify deny check and final clean build

- [ ] **Step 1: Run `cargo deny check`**

Run: `cargo deny check`
Expected: PASS. `tracing-appender` should be on the existing license allowlist (MIT). If FAIL, add to the allowlist in `deny.toml`.

- [ ] **Step 2: Run the full CI-equivalent pipeline**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

Expected: all green.

- [ ] **Step 3: Confirm git log**

Run: `git log --oneline -10`
Expected: clean sequence of commits for tasks 1 through 7.

---

## Done Criteria

- `tracing-appender` is in the workspace and observability crate.
- `LoggingConfig::file: Option<FileLoggingConfig>` exists; `FileLoggingConfig` and `RotationPolicy` deserialize correctly with sane defaults; `deny_unknown_fields` rejects typos.
- `agent_shim_observability::init` installs a rolling file layer when `log.file.is_some()`. Layer is **async** (`non_blocking`) and the `WorkerGuard` rides on `TracingHandles`.
- `run_core` (from Phase 1) keeps the guard alive for the process lifetime — verified by the new gateway integration test.
- `config/gateway.example.yaml` documents the feature with a commented-out block.
- `cargo deny check`, `cargo fmt`, `cargo clippy -- -D warnings`, and `cargo nextest run --workspace` are all clean.

Linux, macOS, and Windows users can now opt into file logging via `gateway.yaml`. Phase 3 (Windows Service install/uninstall/status) and Phase 4 (Windows Service run/start/stop) build on this foundation.
