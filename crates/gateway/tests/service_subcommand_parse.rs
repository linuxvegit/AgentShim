// crates/gateway/tests/service_subcommand_parse.rs
//
// Cross-platform negative test ensuring `service` subcommand is gated to
// Windows. On non-Windows, parsing "agent-shim service install ..." must
// fail with a "no such subcommand" error.
//
// On Windows, parsing succeeds. We don't run the command — just exercise
// clap's parser to confirm the variant is reachable.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "agent-shim")]
struct Cli {
    #[command(subcommand)]
    command: agent_shim_gateway::cli::Commands,
}

#[cfg(not(windows))]
#[test]
fn service_subcommand_absent_on_non_windows() {
    let result = Cli::try_parse_from([
        "agent-shim",
        "service",
        "install",
        "--config",
        "/tmp/x.yaml",
    ]);
    assert!(
        result.is_err(),
        "service subcommand must NOT be available on non-Windows; got: {result:?}"
    );
}

#[cfg(windows)]
#[test]
fn service_subcommand_present_on_windows() {
    // The full install command parses; we don't execute it.
    let result = Cli::try_parse_from([
        "agent-shim",
        "service",
        "install",
        "--config",
        "C:\\path\\gateway.yaml",
    ]);
    assert!(
        result.is_ok(),
        "service install should parse on Windows; got: {result:?}"
    );
}
