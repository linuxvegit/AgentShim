//! Last-resort error sink for the Windows Service entry point.
//!
//! SCM-launched processes have no console, so `eprintln!` is invisible
//! to operators. `tracing` isn't initialized until `run_core` runs — so
//! config-load / validate / runtime-init failures (which all happen
//! before `run_core`) leak no diagnostic information at all. Without
//! this sink, the operator sees "the service stopped with error 1" in
//! `sc query` and nothing else.
//!
//! This module writes plain-text timestamped lines to
//! `%PROGRAMDATA%\agent-shim\logs\agent-shim-startup-error.log` in
//! append mode. The file is a SIBLING of the rolling appender's daily
//! files (managed by `tracing-appender` in `run_core`), so there is no
//! rotation or locking conflict — `tracing-appender` writes
//! `agent-shim.log.YYYY-MM-DD` and we write `agent-shim-startup-error.log`.
//!
//! All operations are best-effort. We are already on the unrecoverable
//! path; no escalation is useful, so file-write failures are silently
//! swallowed. The operator will still get the SCM `Stopped(exit_code=1)`
//! signal from `run_service`'s wrapper, and the missing log file at the
//! documented location is itself diagnostic (PROGRAMDATA missing, ACLs
//! blocking the service account, etc.).
//!
//! Both the path resolver and `record` accept an env-var lookup closure
//! so tests can inject a tempdir without mutating the process
//! environment (which is racy across parallel test threads).

use std::ffi::OsString;
use std::path::PathBuf;

/// Resolve the on-disk path for the startup-error log, using the
/// provided env-var lookup. Production code uses `std::env::var_os`;
/// tests inject a closure pointing at a tempdir.
pub fn startup_error_log_path_with_env<F>(env_get: F) -> PathBuf
where
    F: Fn(&str) -> Option<OsString>,
{
    let base = env_get("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("agent-shim")
        .join("logs")
        .join("agent-shim-startup-error.log")
}

/// Production-friendly path resolver: reads `PROGRAMDATA` from the
/// process environment.
///
/// Currently unused at runtime (everything goes through `record` →
/// `record_with_env`), kept for diagnostics tooling and to anchor the
/// production env-var lookup at one call site. `#[allow(dead_code)]`
/// is preferred over removing it because the test surface is the
/// `_with_env` variant — when an operator or test author asks "where
/// does the file actually live?" the answer is this function.
#[allow(dead_code)]
pub fn startup_error_log_path() -> PathBuf {
    startup_error_log_path_with_env(|k| std::env::var_os(k))
}

/// Append a timestamped error line to the startup-error log, resolving
/// the path through the provided env-var lookup. Best-effort: any I/O
/// failure is silently swallowed.
pub fn record_with_env<F>(env_get: F, message: &str)
where
    F: Fn(&str) -> Option<OsString>,
{
    let path = startup_error_log_path_with_env(&env_get);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("[{ts}] {message}\n");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
}

/// Production entry point: record a startup error using the process
/// environment to resolve `PROGRAMDATA`.
pub fn record(message: &str) {
    record_with_env(|k| std::env::var_os(k), message);
}
