// Phase windows-service-fix T1: unit tests for the startup-error sink.
//
// These tests pin the contract that `startup_error::record` writes a
// timestamped, append-mode plain-text line under the configured
// %PROGRAMDATA%\agent-shim\logs\ directory, even when called multiple
// times. The path resolver accepts an env-var lookup closure so we can
// inject a tempdir without mutating the process environment (which is
// race-prone in a multi-test binary).
//
// The functions exercised by these tests do not yet exist; this is the
// TDD red commit.

#![cfg(windows)]

use agent_shim_gateway::commands::service::startup_error::{
    record_with_env, startup_error_log_path_with_env,
};
use std::ffi::OsString;
use std::path::PathBuf;

#[test]
fn startup_error_log_path_uses_programdata_when_set() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_str = tmp.path().to_string_lossy().into_owned();
    let env = |key: &str| -> Option<OsString> {
        if key == "PROGRAMDATA" {
            Some(OsString::from(&tmp_str))
        } else {
            None
        }
    };

    let path = startup_error_log_path_with_env(env);
    let expected = tmp
        .path()
        .join("agent-shim")
        .join("logs")
        .join("agent-shim-startup-error.log");
    assert_eq!(path, expected);
}

#[test]
fn startup_error_log_path_falls_back_when_programdata_unset() {
    let env = |_key: &str| -> Option<OsString> { None };
    let path = startup_error_log_path_with_env(env);
    assert_eq!(
        path,
        PathBuf::from(r"C:\ProgramData\agent-shim\logs\agent-shim-startup-error.log")
    );
}

#[test]
fn record_creates_file_under_programdata() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_str = tmp.path().to_string_lossy().into_owned();
    let env = move |key: &str| -> Option<OsString> {
        if key == "PROGRAMDATA" {
            Some(OsString::from(&tmp_str))
        } else {
            None
        }
    };

    record_with_env(&env, "hello from the test");

    let expected = tmp
        .path()
        .join("agent-shim")
        .join("logs")
        .join("agent-shim-startup-error.log");
    let contents = std::fs::read_to_string(&expected).expect("startup-error log was not written");
    assert!(
        contents.contains("hello from the test"),
        "log contents did not include the recorded message: {contents:?}"
    );
}

#[test]
fn record_appends_multiple_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_str = tmp.path().to_string_lossy().into_owned();
    let env = move |key: &str| -> Option<OsString> {
        if key == "PROGRAMDATA" {
            Some(OsString::from(&tmp_str))
        } else {
            None
        }
    };

    record_with_env(&env, "first");
    record_with_env(&env, "second");
    record_with_env(&env, "third");

    let path = tmp
        .path()
        .join("agent-shim")
        .join("logs")
        .join("agent-shim-startup-error.log");
    let contents = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 3, "expected 3 lines, got {lines:?}");
    assert!(lines[0].contains("first"), "lines[0]: {:?}", lines[0]);
    assert!(lines[1].contains("second"), "lines[1]: {:?}", lines[1]);
    assert!(lines[2].contains("third"), "lines[2]: {:?}", lines[2]);
}

#[test]
fn record_includes_timestamp_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_str = tmp.path().to_string_lossy().into_owned();
    let env = move |key: &str| -> Option<OsString> {
        if key == "PROGRAMDATA" {
            Some(OsString::from(&tmp_str))
        } else {
            None
        }
    };

    record_with_env(&env, "boom");

    let path = tmp
        .path()
        .join("agent-shim")
        .join("logs")
        .join("agent-shim-startup-error.log");
    let contents = std::fs::read_to_string(&path).unwrap();
    // Format we commit to: leading "[<digits>]" — Unix-epoch seconds.
    // Concrete contract pinned here so the format change requires an
    // explicit test update.
    assert!(
        contents.starts_with('['),
        "expected timestamp prefix, got: {contents:?}"
    );
    let after_bracket = contents.trim_start_matches('[');
    let close_pos = after_bracket
        .find(']')
        .unwrap_or_else(|| panic!("no closing bracket in {contents:?}"));
    let ts_str = &after_bracket[..close_pos];
    let ts_parsed: u64 = ts_str
        .parse()
        .unwrap_or_else(|_| panic!("timestamp not integer: {ts_str:?}"));
    assert!(ts_parsed > 0, "timestamp should be > 0: {ts_parsed}");
}
