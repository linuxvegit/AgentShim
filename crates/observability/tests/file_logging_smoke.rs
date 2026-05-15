// crates/observability/tests/file_logging_smoke.rs
//
// Plan windows-service P02 T4: failing integration test for file-logging.
//
// Verifies that the file-logging layer wired up by
// agent_shim_observability::init actually writes JSON log records to the
// configured path. Uses a tempdir so we don't litter the filesystem.

use agent_shim_config::schema::{FileLoggingConfig, LogFormat, LoggingConfig, RotationPolicy};

#[test]
fn file_logging_writes_json_events_to_configured_path() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("agent-shim.log");

    let cfg = LoggingConfig {
        format: LogFormat::Pretty, // stdout: pretty, irrelevant for this test
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
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .collect();
    assert!(
        !entries.is_empty(),
        "no log file produced under {:?}",
        tmp.path()
    );

    // Read the first matching file and parse each line as JSON.
    let content = std::fs::read_to_string(entries[0].path()).unwrap();
    let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        !lines.is_empty(),
        "log file is empty: {:?}",
        entries[0].path()
    );

    let mut found_hello = false;
    let mut found_second = false;
    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("log line is not JSON: {line:?} ({e})"));
        let msg = v
            .pointer("/fields/message")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        if msg == "hello from the test" {
            found_hello = true;
        }
        if msg == "second event" {
            found_second = true;
        }
    }
    assert!(found_hello, "first event missing from log file: {content}");
    assert!(
        found_second,
        "second event missing from log file: {content}"
    );
}
