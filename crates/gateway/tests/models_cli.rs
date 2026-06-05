//! End-to-end coverage for `commands::models::run`. Spins up a mockito
//! server, writes a config file pointing at it, then calls `run()`
//! directly. The unit tests in `commands::models` already cover error
//! wording; these tests cover the full path through config-load,
//! AppState construction, provider lookup, and rendering.

use std::io::Write;

use agent_shim_gateway::commands::models;
use tempfile::NamedTempFile;

fn write_config(yaml: &str) -> NamedTempFile {
    let mut file = NamedTempFile::new().expect("tempfile creation");
    file.write_all(yaml.as_bytes()).expect("config write");
    file
}

fn base_config(upstream_name: &str, base_url: &str) -> String {
    // Minimum-viable config that passes validation. The mockito server
    // url has no `/v1` suffix, and openai_compatible's list_models()
    // appends `/v1/models` itself.
    //
    // Note: `port: 0` is rejected by `agent_shim_config::validate` even
    // though we never actually bind a listener here (commands::models::run
    // does not start the server). A high static port keeps validation
    // happy without affecting test isolation.
    format!(
        r#"
server: {{bind: 127.0.0.1, port: 18999}}
upstreams:
  {upstream_name}:
    type: open_ai_compatible
    base_url: "{base_url}"
    api_key: "test-key"
    tier: standard
routes:
  - frontend: openai_chat
    model: x
    upstream: {upstream_name}
    upstream_model: x
"#
    )
}

#[tokio::test]
async fn unknown_upstream_errors_with_available_list() {
    let mut server = mockito::Server::new_async().await;
    // AppState::new runs discovery on every upstream during construction
    // — give it a 404 so discovery completes cleanly without hanging.
    let _mock = server
        .mock("GET", "/v1/models")
        .with_status(404)
        .create_async()
        .await;

    let cfg = base_config("my-upstream", &server.url());
    let cfg_file = write_config(&cfg);

    let err = models::run("nonexistent", cfg_file.path(), "table")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no upstream named 'nonexistent'"), "{err}");
    assert!(err.contains("available: my-upstream"), "{err}");
}

#[tokio::test]
async fn missing_copilot_includes_device_flow_hint() {
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock("GET", "/v1/models")
        .with_status(404)
        .create_async()
        .await;

    let cfg = base_config("my-upstream", &server.url());
    let cfg_file = write_config(&cfg);

    let err = models::run("copilot", cfg_file.path(), "table")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("`agent-shim copilot models`"), "{err}");
    assert!(err.contains("device-flow"), "{err}");
}

#[tokio::test]
async fn happy_path_table_format_renders() {
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock("GET", "/v1/models")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "data": [
                    {"id": "model-a", "object": "model"},
                    {"id": "model-b", "object": "model"}
                ]
            }"#,
        )
        .expect_at_least(1)
        .create_async()
        .await;

    let cfg = base_config("my-upstream", &server.url());
    let cfg_file = write_config(&cfg);

    // Smoke: the call returns Ok. Stdout capture under tokio::test is
    // awkward; render_models unit tests already cover formatting, so we
    // only need to assert that the CLI-to-renderer path doesn't error.
    models::run("my-upstream", cfg_file.path(), "table")
        .await
        .expect("run should succeed against mocked 200");
}

#[tokio::test]
async fn happy_path_json_format_renders() {
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock("GET", "/v1/models")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"data":[{"id":"model-a"}]}"#)
        .expect_at_least(1)
        .create_async()
        .await;

    let cfg = base_config("my-upstream", &server.url());
    let cfg_file = write_config(&cfg);

    models::run("my-upstream", cfg_file.path(), "json")
        .await
        .expect("run should succeed");
}

#[tokio::test]
async fn unknown_format_errors() {
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock("GET", "/v1/models")
        .with_status(200)
        .with_body(r#"{"data":[{"id":"x"}]}"#)
        .expect_at_least(1)
        .create_async()
        .await;

    let cfg = base_config("my-upstream", &server.url());
    let cfg_file = write_config(&cfg);

    let err = models::run("my-upstream", cfg_file.path(), "yaml")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown --format"), "{err}");
    assert!(err.contains("`table` or `json`"), "{err}");
}
