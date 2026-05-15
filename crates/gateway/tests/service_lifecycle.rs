//! Phase 4 SCM end-to-end smoke test. install → start → healthz →
//! verify log file → stop → uninstall. Marked `#[ignore]`; admin only.
//!
//! NOT run in CI. Use:
//!
//! ```bash
//! cargo test -p agent-shim --test service_lifecycle -- --ignored
//! ```
//!
//! from an elevated PowerShell/CMD.

#![cfg(windows)]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

struct ServiceCleanup {
    name: String,
}
impl Drop for ServiceCleanup {
    fn drop(&mut self) {
        let _ = Command::new("sc").args(["stop", &self.name]).status();
        let _ = Command::new("sc").args(["delete", &self.name]).status();
    }
}

fn cargo_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agent-shim"))
}

fn pick_port() -> u16 {
    // Bind to 0, read the assigned port, close the listener.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn write_config(dir: &std::path::Path, port: u16, log_path: &std::path::Path) -> PathBuf {
    let path = dir.join("gateway.yaml");
    let log_path_str = log_path.display().to_string().replace('\\', "\\\\");
    let yaml = format!(
        "server:\n  bind: 127.0.0.1\n  port: {port}\nlogging:\n  format: json\n  filter: info\n  \
         file:\n    path: \"{log_path_str}\"\n    format: json\n    rotation: daily\n    max_files: 7\nupstreams: {{}}\nroutes: []\n"
    );
    std::fs::write(&path, yaml).unwrap();
    path
}

fn wait_for_healthz(port: u16, timeout: Duration) {
    let start = Instant::now();
    let url = format!("http://127.0.0.1:{port}/");
    loop {
        if let Ok(resp) = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap()
            .get(&url)
            .send()
        {
            if resp.status().is_success() {
                return;
            }
        }
        assert!(
            start.elapsed() < timeout,
            "service not responding on port {port} after {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
#[ignore = "requires administrator; run with --ignored only"]
fn service_full_lifecycle() {
    let svc_name = format!("agent-shim-test-{}", uuid::Uuid::new_v4());
    let _cleanup = ServiceCleanup {
        name: svc_name.clone(),
    };

    let tmp = tempfile::tempdir().unwrap();
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log_path = log_dir.join("agent-shim.log");
    let port = pick_port();
    let cfg = write_config(tmp.path(), port, &log_path);

    let bin = cargo_bin();

    // install
    let out = Command::new(&bin)
        .args(["service", "install", "--name", &svc_name, "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "install: {:?} {:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // start
    let out = Command::new(&bin)
        .args(["service", "start", "--name", &svc_name])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start: {:?} {:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // healthz
    wait_for_healthz(port, Duration::from_secs(15));

    // Verify log file exists. Rolling appender filename is
    // "agent-shim.log.YYYY-MM-DD" — look for ANY file in log_dir starting
    // with "agent-shim.log".
    let entries: Vec<_> = std::fs::read_dir(&log_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("agent-shim.log")
        })
        .collect();
    assert!(!entries.is_empty(), "no log file produced in {log_dir:?}");

    // stop
    let out = Command::new(&bin)
        .args(["service", "stop", "--name", &svc_name])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stop: {:?} {:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // status should now report Stopped
    let out = Command::new(&bin)
        .args(["service", "status", "--name", &svc_name])
        .output()
        .unwrap();
    assert!(out.status.success(), "status post-stop");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("State:       Stopped"),
        "expected Stopped, got: {stdout}"
    );

    // uninstall
    let out = Command::new(&bin)
        .args(["service", "uninstall", "--name", &svc_name])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "uninstall: {:?} {:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
