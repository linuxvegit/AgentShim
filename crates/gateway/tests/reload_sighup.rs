//! Plan 04 P04 T6: SIGHUP triggers a reload. Unix-only.

#![cfg(unix)]

use std::process::Stdio;
use std::time::Duration;

#[tokio::test]
async fn sighup_increments_reload_counter() {
    // Build the gateway binary under test.
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--bin", "agent-shim", "--quiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");

    let public_port = pick_port_blocking();
    let admin_port = pick_port_blocking();

    // Write a temp config.
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("gateway.yaml");
    let yaml = format!(
        r#"
server: {{bind: 127.0.0.1, port: {public_port}}}
admin: {{bind: 127.0.0.1, port: {admin_port}}}
upstreams:
  m: {{type: open_ai_compatible, base_url: http://x/v1, api_key: a}}
routes:
  - {{frontend: openai_chat, model: x, upstream: m, upstream_model: x}}
"#
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    // Spawn the binary.
    let bin = std::path::PathBuf::from(env!("CARGO_TARGET_DIR")).join("debug/agent-shim");
    let bin = if bin.exists() {
        bin
    } else {
        // Fallback: assume target/debug under the workspace root.
        std::path::PathBuf::from("target/debug/agent-shim")
    };
    let mut child = std::process::Command::new(bin)
        .args(["serve", "--config", cfg_path.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent-shim");

    // Wait for /readyz to come up.
    let admin_url = format!("http://127.0.0.1:{}", admin_port);
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if reqwest::get(format!("{}/readyz", admin_url)).await.is_ok() {
            break;
        }
    }

    // Send SIGHUP.
    let pid = child.id();
    let kill_status = std::process::Command::new("kill")
        .args(["-HUP", &pid.to_string()])
        .status()
        .expect("kill -HUP");
    assert!(kill_status.success());

    // Give the reload task a moment.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Scrape /metrics. Counter must show ≥ 1 reload.
    let body = reqwest::get(format!("{}/metrics", admin_url))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let scrape = prometheus_parse::Scrape::parse(body.lines().map(|l| Ok(l.to_string()))).unwrap();
    let total: f64 = scrape
        .samples
        .iter()
        .filter(|s| s.metric == "agent_shim_config_reloads_total")
        .filter_map(|s| match s.value {
            prometheus_parse::Value::Counter(v) | prometheus_parse::Value::Untyped(v) => Some(v),
            _ => None,
        })
        .sum();

    let _ = child.kill();
    let _ = child.wait();

    assert!(total >= 1.0, "expected ≥1 reload, got {total}");
}

fn pick_port_blocking() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}
