// Phase windows-service-fix T2: unit tests for the
// load_validate_inject_fallback helper.
//
// Pins the contract that this helper:
//   1. Returns Err (no panic) when the config file is missing.
//   2. Returns Err (no panic) when the YAML is malformed.
//   3. Returns Err (no panic) when config::validate rejects.
//   4. Returns Ok(cfg) with logging.file populated when the input
//      file omitted the logging.file section (service fallback ran).
//   5. Preserves the user-provided logging.file when present.
//
// These tests pin the "fail fast, with a real Err" behavior that
// run_service relies on so the Windows Service wrapper can report
// Stopped(Win32(1)) instead of the process disappearing while SCM
// thinks the service is still StartPending.
//
// The helper under test is gated to Windows (it depends on the
// service log_fallback module). On Linux, this whole file compiles
// to nothing.

#![cfg(windows)]

use agent_shim_gateway::commands::service::run::load_validate_inject_fallback;
use std::io::Write;

/// Write the given YAML to a tempfile and return the (tempfile,
/// path) tuple. The tempfile must outlive the path use.
fn yaml_tempfile(yaml: &str) -> (tempfile::NamedTempFile, std::path::PathBuf) {
    let mut f = tempfile::NamedTempFile::new().expect("tempfile");
    f.write_all(yaml.as_bytes()).expect("write yaml");
    let path = f.path().to_path_buf();
    (f, path)
}

#[test]
fn returns_err_when_config_file_missing() {
    let path =
        std::path::PathBuf::from(r"C:\__agent_shim_test_definitely_does_not_exist__\nope.yaml");
    let result = load_validate_inject_fallback(&path);
    assert!(
        result.is_err(),
        "missing config must return Err, got Ok: {:?}",
        result.ok().map(|_| "<cfg>")
    );
}

#[test]
fn returns_err_when_yaml_invalid() {
    // Tab-indented YAML with unbalanced braces — guaranteed parse error.
    let (_keep_alive, path) = yaml_tempfile("not: valid: yaml: at: all\n\t{][");
    let result = load_validate_inject_fallback(&path);
    assert!(
        result.is_err(),
        "invalid YAML must return Err, got Ok: {:?}",
        result.ok().map(|_| "<cfg>")
    );
}

#[test]
fn returns_err_when_validate_rejects() {
    // server.port = 0 trips ValidationError::ZeroPort.
    let yaml = "server:\n  port: 0\n";
    let (_keep_alive, path) = yaml_tempfile(yaml);
    let result = load_validate_inject_fallback(&path);
    assert!(
        result.is_err(),
        "port=0 must be rejected by validate(), got Ok"
    );
}

#[test]
fn returns_ok_with_file_fallback_when_logging_file_absent() {
    // Minimal valid config that omits logging.file entirely.
    let yaml = "server:\n  port: 9000\n";
    let (_keep_alive, path) = yaml_tempfile(yaml);
    let cfg = load_validate_inject_fallback(&path).expect("minimal config must validate");
    let file = cfg
        .logging
        .file
        .expect("service fallback must inject logging.file");
    assert!(
        file.path.is_absolute(),
        "fallback path must be absolute; got {:?}",
        file.path
    );
}

#[test]
fn returns_ok_preserving_user_logging_file() {
    let yaml = "server:\n  port: 9000\n\
                logging:\n  file:\n    path: C:\\custom\\logs\\app.log\n";
    let (_keep_alive, path) = yaml_tempfile(yaml);
    let cfg = load_validate_inject_fallback(&path).expect("user logging.file must validate");
    let file = cfg.logging.file.expect("user-provided logging.file lost");
    assert_eq!(
        file.path,
        std::path::PathBuf::from(r"C:\custom\logs\app.log"),
        "user-provided path must win over service fallback"
    );
}
