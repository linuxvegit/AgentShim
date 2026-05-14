//! Default service identifiers and ImagePath formatting.
//!
//! Spec section 4.3: the command line registered with SCM is
//!   "<exe>" service run --config "<absolute config path>"
//! Quoting is critical — paths may contain spaces (e.g. C:\Program Files).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Default Windows Service name when `--name` is not provided.
#[allow(dead_code)] // Used by Phase 4 lifecycle handlers.
pub const DEFAULT_SERVICE_NAME: &str = "agent-shim";

/// Construct the `OsString` arguments passed to `ServiceInfo.launch_arguments`.
/// The `windows-service` crate joins these onto the bare executable path
/// using its own quoting rules — we don't manually quote here.
pub fn launch_arguments(config: &Path) -> Vec<OsString> {
    vec![
        OsString::from("service"),
        OsString::from("run"),
        OsString::from("--config"),
        config.as_os_str().to_owned(),
    ]
}

/// Pretty-format the full ImagePath as it would appear in `sc qc <name>`
/// output. Used by the `status` command's display only.
#[allow(dead_code)] // Reserved for richer status display + Phase 4 logging.
pub fn format_image_path_for_display(exe: &Path, config: &Path) -> String {
    format!(
        r#""{}" service run --config "{}""#,
        exe.display(),
        config.display(),
    )
}

/// Parse `--config <path>` back out of a SCM-formatted ImagePath.
/// The ImagePath is a Win32 command-line string with double-quoted
/// tokens around paths that contain spaces (notably `C:\Program Files`).
///
/// Returns `None` if `--config` (or `-c`) is absent from the line.
pub fn parse_config_from_image_path(image_path: &str) -> Option<PathBuf> {
    let tokens = tokenize_image_path(image_path);
    let mut iter = tokens.into_iter();
    while let Some(t) = iter.next() {
        if t == "--config" || t == "-c" {
            return iter.next().map(PathBuf::from);
        }
    }
    None
}

/// Quote-aware tokenizer for Win32-style command lines. Splits on
/// whitespace outside double-quoted spans; strips surrounding quotes
/// from each token. Does NOT handle escaped quotes (`\"`) because
/// SCM doesn't produce them in the agent-shim ImagePath.
fn tokenize_image_path(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for ch in s.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn launch_arguments_orders_subcommand_then_flag_then_value() {
        let args = launch_arguments(Path::new(r"C:\ProgramData\agent-shim\gateway.yaml"));
        let strs: Vec<&str> = args.iter().map(|s| s.to_str().unwrap()).collect();
        assert_eq!(
            strs,
            vec![
                "service",
                "run",
                "--config",
                r"C:\ProgramData\agent-shim\gateway.yaml"
            ]
        );
    }

    #[test]
    fn parse_config_from_image_path_extracts_quoted_path() {
        let image = r#""C:\Program Files\agent-shim\agent-shim.exe" service run --config "C:\ProgramData\agent-shim\gateway.yaml""#;
        assert_eq!(
            parse_config_from_image_path(image),
            Some(PathBuf::from(r"C:\ProgramData\agent-shim\gateway.yaml")),
        );
    }

    #[test]
    fn parse_config_from_image_path_extracts_unquoted_path() {
        let image = r#"C:\bin\agent-shim.exe service run --config C:\etc\gw.yaml"#;
        assert_eq!(
            parse_config_from_image_path(image),
            Some(PathBuf::from(r"C:\etc\gw.yaml")),
        );
    }

    #[test]
    fn parse_config_from_image_path_returns_none_when_flag_absent() {
        let image = r#""C:\bin\agent-shim.exe" serve"#;
        assert_eq!(parse_config_from_image_path(image), None);
    }

    #[test]
    fn parse_config_from_image_path_handles_quoted_path_with_spaces() {
        let image = r#""C:\Program Files\agent-shim\agent-shim.exe" service run --config "C:\Program Files\agent-shim\gateway.yaml""#;
        assert_eq!(
            parse_config_from_image_path(image),
            Some(PathBuf::from(r"C:\Program Files\agent-shim\gateway.yaml")),
        );
    }
}
