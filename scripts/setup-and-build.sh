#!/usr/bin/bash
#
# scripts/setup-and-build.sh
#
# Bootstrap local Rust toolchain and produce a release build of AgentShim
# on Linux / macOS (and WSL).
#
# What this does:
#   1. Ensures system build prerequisites such as `cc` are present.
#   2. Verifies / installs rustup + the stable toolchain pinned in rust-toolchain.toml.
#   3. Ensures rustfmt / clippy / llvm-tools-preview components are present.
#   4. (Optional) installs cargo-nextest for fast tests.
#   5. Runs `cargo build --release -p agent-shim` and reports the binary path.
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

run_as_root() {
    if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
        "$@"
    elif have sudo; then
        sudo "$@"
    else
        return 127
    fi
}

run_package_command() {
    if ! run_as_root "$@"; then
        die "failed to run '$*'. Install the missing packages manually and re-run this script."
    fi
}

install_linux_packages() {
    local packages=("$@")

    if [[ "${#packages[@]}" -eq 0 ]]; then
        return 0
    fi

    if have apt-get; then
        info "Installing OS packages with apt-get: ${packages[*]}"
        run_package_command apt-get update
        run_package_command env DEBIAN_FRONTEND=noninteractive apt-get install -y "${packages[@]}"
    elif have dnf; then
        info "Installing OS packages with dnf: ${packages[*]}"
        run_package_command dnf install -y "${packages[@]}"
    elif have yum; then
        info "Installing OS packages with yum: ${packages[*]}"
        run_package_command yum install -y "${packages[@]}"
    elif have pacman; then
        info "Installing OS packages with pacman: ${packages[*]}"
        run_package_command pacman -S --needed --noconfirm "${packages[@]}"
    elif have apk; then
        info "Installing OS packages with apk: ${packages[*]}"
        run_package_command apk add --no-cache "${packages[@]}"
    else
        return 1
    fi
}

ensure_system_deps() {
    step "Checking system build dependencies"

    local need_cc=0
    local need_curl=0

    have cc || need_cc=1
    if ! have rustup && ! have curl; then
        need_curl=1
    fi

    if [[ "$need_cc" -eq 0 && "$need_curl" -eq 0 ]]; then
        info "system build dependencies found."
        return 0
    fi

    case "$(uname -s)" in
        Linux)
            if have apt-get; then
                local packages=()
                [[ "$need_cc" -eq 1 ]] && packages+=(build-essential)
                [[ "$need_curl" -eq 1 ]] && packages+=(curl)
                install_linux_packages "${packages[@]}"
            elif have dnf || have yum; then
                local packages=()
                [[ "$need_cc" -eq 1 ]] && packages+=(gcc gcc-c++ make)
                [[ "$need_curl" -eq 1 ]] && packages+=(curl)
                install_linux_packages "${packages[@]}"
            elif have pacman; then
                local packages=()
                [[ "$need_cc" -eq 1 ]] && packages+=(base-devel)
                [[ "$need_curl" -eq 1 ]] && packages+=(curl)
                install_linux_packages "${packages[@]}"
            elif have apk; then
                local packages=()
                [[ "$need_cc" -eq 1 ]] && packages+=(build-base)
                [[ "$need_curl" -eq 1 ]] && packages+=(curl)
                install_linux_packages "${packages[@]}"
            else
                die "missing system dependencies. Install a C toolchain, e.g. 'sudo apt-get install build-essential'."
            fi
            ;;
        Darwin)
            if [[ "$need_cc" -eq 1 ]]; then
                die "missing C toolchain. Install Xcode Command Line Tools with: xcode-select --install"
            fi
            if [[ "$need_curl" -eq 1 ]]; then
                die "curl is required to install rustup. Install curl and re-run."
            fi
            ;;
        *)
            die "missing system dependencies. Install a C toolchain that provides 'cc' and re-run."
            ;;
    esac

    have cc || die "C linker 'cc' is still missing after dependency installation."
    if ! have rustup && ! have curl; then
        die "curl is still missing after dependency installation."
    fi

    info "cc: $(cc --version | head -n 1)"
}

# Resolve repo root from this script's location so the script works no matter
# where the user invokes it from.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

if [[ "${EUID:-$(id -u)}" -eq 0 && -n "${SUDO_USER:-}" ]]; then
    warn "running the whole script with sudo is not recommended; run it as your normal user instead."
    warn "the script will ask for sudo only when OS packages need to be installed."
fi

# ---------------------------------------------------------------------------
# 1. System build dependencies
# ---------------------------------------------------------------------------
ensure_system_deps

# ---------------------------------------------------------------------------
# 2. rustup
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
# 3. Toolchain + components (driven by rust-toolchain.toml)
# ---------------------------------------------------------------------------
step "Installing toolchain pinned in rust-toolchain.toml"

# `rustup show` triggers installation of the pinned toolchain and components.
rustup show

info "rustc:  $(rustc --version)"
info "cargo:  $(cargo --version)"

# ---------------------------------------------------------------------------
# 4. (Optional) cargo-nextest
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
# 5. Release build
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
# 6. (Optional) tests
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
# 7. Report binary location
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
