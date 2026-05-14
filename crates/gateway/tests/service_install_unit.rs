// crates/gateway/tests/service_install_unit.rs
//
// Phase 3 SCM smoke test. Marked #[ignore]; runs only when invoked with
// --run-ignored. Requires an elevated terminal. Cleans up the registered
// service even on panic via the Drop guard.

#![cfg(windows)]

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

struct ServiceCleanup {
    name: String,
}
impl ServiceCleanup {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
        }
    }
}
impl Drop for ServiceCleanup {
    fn drop(&mut self) {
        // Best-effort cleanup. Ignore errors; the test may have already
        // uninstalled, or sc may complain that the service is unknown.
        let _ = Command::new("sc").args(["stop", &self.name]).status();
        let _ = Command::new("sc").args(["delete", &self.name]).status();
    }
}

fn write_minimal_config(dir: &std::path::Path) -> PathBuf {
    let yaml = "\
server:
  bind: 127.0.0.1
  port: 0
logging:
  format: pretty
  filter: info
upstreams: {}
routes: []
";
    let path = dir.join("gateway.yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

fn cargo_bin() -> PathBuf {
    // env::var("CARGO_BIN_EXE_<name>") is set by cargo for binaries
    // referenced as test fixtures. But since this test lives in the
    // `agent-shim` crate's `tests/` dir, cargo sets CARGO_BIN_EXE_agent-shim.
    PathBuf::from(env!("CARGO_BIN_EXE_agent-shim"))
}

#[test]
#[ignore = "requires administrator; run with cargo nextest run --run-ignored only"]
fn install_status_uninstall_cycle_smoke() {
    let svc_name = format!("agent-shim-test-{}", uuid::Uuid::new_v4());
    let _cleanup = ServiceCleanup::new(&svc_name);

    let tmp = tempfile::tempdir().unwrap();
    let cfg = write_minimal_config(tmp.path());

    let bin = cargo_bin();

    // install
    let out = Command::new(&bin)
        .args(["service", "install", "--name", &svc_name, "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "install failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // status — must report Stopped
    let out = Command::new(&bin)
        .args(["service", "status", "--name", &svc_name])
        .output()
        .unwrap();
    assert!(out.status.success(), "status failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("State:       Stopped"),
        "status output: {stdout}"
    );
    assert!(
        stdout.contains(&cfg.display().to_string()),
        "status did not echo config path: {stdout}"
    );

    // uninstall
    let out = Command::new(&bin)
        .args(["service", "uninstall", "--name", &svc_name])
        .output()
        .unwrap();
    assert!(out.status.success(), "uninstall failed");

    // status again — must report not installed (exit code 1).
    let out = Command::new(&bin)
        .args(["service", "status", "--name", &svc_name])
        .output()
        .unwrap();
    assert!(!out.status.success(), "status should fail post-uninstall");

    // Give SCM a beat to settle, just in case.
    std::thread::sleep(Duration::from_millis(200));
}

#[test]
fn install_requires_admin_when_unelevated() {
    // This test runs as part of the regular suite (no #[ignore]) — it
    // only exercises the admin gate, no SCM call. We assume the test
    // runner is NOT elevated; on CI Windows runners (which are elevated)
    // we accept either the elevation gate firing OR the install going
    // further. The point is: the binary must not panic.

    let tmp = tempfile::tempdir().unwrap();
    let cfg = write_minimal_config(tmp.path());

    let bin = cargo_bin();
    let out = Command::new(&bin)
        .args([
            "service",
            "install",
            "--name",
            "agent-shim-elev-test",
            "--config",
        ])
        .arg(&cfg)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        // Acceptable: elevation gate fired, or SCM connect failed because
        // we're elevated but the test runner doesn't have CREATE_SERVICE
        // for some reason. Either way the message should be human-readable
        // (not a Rust panic).
        assert!(
            stderr.contains("administrator")
                || stderr.contains("Access is denied")
                || stderr.contains("error"),
            "expected friendly error, got: {stderr}",
        );
    } else {
        // CI runner is elevated and SCM is happy — clean up.
        let _ = Command::new(&bin)
            .args(["service", "uninstall", "--name", "agent-shim-elev-test"])
            .status();
    }
}
