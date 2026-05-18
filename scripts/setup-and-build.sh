#!/usr/bin/env bash
#
# scripts/setup-and-build.sh
#
# Bootstrap local Rust toolchain and produce a release build of AgentShim
# on Linux / macOS (and WSL).
#
# What this does:
#   1. Verifies / installs rustup + the stable toolchain pinned in rust-toolchain.toml.
#   2. Ensures rustfmt / clippy / llvm-tools-preview components are present.
#   3. (Optional) installs cargo-nextest for fast tests.
#   4. Runs `cargo build --release -p agent-shim` and reports the binary path.
#
# Usage:
#   ./scripts/setup-and-build.sh
#   ./scripts/setup-and-build.sh --skip-nextest
#   ./scripts/setup-and-build.sh --workspace          # build whole workspace
#   ./scripts/setup-and-build.sh --run-tests          # also run nextest

set -euo pipefail

# ---- option parsing --------------------------------------------------------
SKIP_NEXTEST=0
WORKSPACE_BUILD=0
RUN_TESTS=0

for arg in "$@"; do
    case "$arg" in
        --skip-nextest)   SKIP_NEXTEST=1 ;;
        --workspace)      WORKSPACE_BUILD=1 ;;
        --run-tests)      RUN_TESTS=1 ;;
        -h|--help)
            sed -n '2,20p' "$0"
            exit 0
            ;;
        *)
            echo "unknown option: $arg" >&2
            echo "usage: $0 [--skip-nextest] [--workspace] [--run-tests]" >&2
            exit 1
            ;;
    esac
done

# ---- helpers ---------------------------------------------------------------
step() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
info() { printf '    \033[2m%s\033[0m\n'    "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n'   "$*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

# Resolve repo root from this script's location so the script works no matter
# where the user invokes it from.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

# ---------------------------------------------------------------------------
# 1. rustup
# ---------------------------------------------------------------------------
step "Checking rustup"

if ! have rustup; then
    info "rustup not found. Installing via official installer..."
    if ! have curl; then
        die "curl is required to install rustup. Install curl and re-run."
    fi
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain none --no-modify-path

    # Make cargo/rustup available in this shell session.
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
else
    info "rustup found: $(rustup --version)"
fi

# ---------------------------------------------------------------------------
# 2. Toolchain + components (driven by rust-toolchain.toml)
# ---------------------------------------------------------------------------
step "Installing toolchain pinned in rust-toolchain.toml"

# `rustup show` triggers installation of the pinned toolchain and components.
rustup show

info "rustc:  $(rustc --version)"
info "cargo:  $(cargo --version)"

# ---------------------------------------------------------------------------
# 3. (Optional) cargo-nextest
# ---------------------------------------------------------------------------
if [[ "$SKIP_NEXTEST" -eq 0 ]]; then
    step "Ensuring cargo-nextest is installed"
    if have cargo-nextest; then
        info "cargo-nextest already installed."
    else
        info "Installing cargo-nextest..."
        if ! cargo install cargo-nextest --locked; then
            warn "cargo-nextest install failed; tests will fall back to 'cargo test'."
        fi
    fi
fi

# ---------------------------------------------------------------------------
# 4. Release build
# ---------------------------------------------------------------------------
step "Building AgentShim (release)"

if [[ "$WORKSPACE_BUILD" -eq 1 ]]; then
    info "cargo build --release --workspace"
    cargo build --release --workspace
else
    info "cargo build --release -p agent-shim"
    cargo build --release -p agent-shim
fi

# ---------------------------------------------------------------------------
# 5. (Optional) tests
# ---------------------------------------------------------------------------
if [[ "$RUN_TESTS" -eq 1 ]]; then
    step "Running tests"
    if have cargo-nextest; then
        cargo nextest run --workspace --release
    else
        cargo test --workspace --release
    fi
fi

# ---------------------------------------------------------------------------
# 6. Report binary location
# ---------------------------------------------------------------------------
binary="$REPO_ROOT/target/release/agent-shim"
step "Build complete"
if [[ -x "$binary" ]]; then
    # Cross-platform size (BSD stat on macOS vs GNU stat on Linux).
    if size=$(stat -c%s "$binary" 2>/dev/null); then :;
    else size=$(stat -f%z "$binary"); fi
    printf '    \033[1;32mBinary:\033[0m %s (%.2f MB)\n' "$binary" "$(awk "BEGIN { printf \"%.2f\", $size/1048576 }")"
    printf '    \033[1;32mRun:\033[0m    %s --help\n' "./target/release/agent-shim"
else
    warn "Expected binary not found at $binary. Inspect target/release/ manually."
fi
