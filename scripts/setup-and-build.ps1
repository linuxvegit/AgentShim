# scripts/setup-and-build.ps1
#
# Bootstrap local Rust toolchain and produce a release build of AgentShim
# on Windows (PowerShell 5.1+ / PowerShell 7+).
#
# What this does:
#   1. Verifies / installs rustup + the stable toolchain pinned in rust-toolchain.toml.
#   2. Ensures rustfmt / clippy / llvm-tools-preview components are present.
#   3. (Optional) installs cargo-nextest for fast tests.
#   4. Runs `cargo build --release -p agent-shim` and reports the binary path.
#
# Usage:
#   pwsh -File scripts\setup-and-build.ps1
#   pwsh -File scripts\setup-and-build.ps1 -SkipNextest
#   pwsh -File scripts\setup-and-build.ps1 -WorkspaceBuild     # build whole workspace
#   pwsh -File scripts\setup-and-build.ps1 -RunTests           # also run nextest

[CmdletBinding()]
param(
    [switch]$SkipNextest,
    [switch]$WorkspaceBuild,
    [switch]$RunTests
)

$ErrorActionPreference = 'Stop'
$ProgressPreference    = 'SilentlyContinue'   # quiet Invoke-WebRequest

# Resolve repo root from this script's location so the script works no matter
# where the user invokes it from.
$RepoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $RepoRoot

function Write-Step {
    param([string]$Message)
    Write-Host ""
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Write-Info {
    param([string]$Message)
    Write-Host "    $Message" -ForegroundColor DarkGray
}

function Test-Command {
    param([string]$Name)
    return [bool](Get-Command $Name -ErrorAction SilentlyContinue)
}

# ---------------------------------------------------------------------------
# 1. rustup
# ---------------------------------------------------------------------------
Write-Step "Checking rustup"

if (-not (Test-Command 'rustup')) {
    Write-Info "rustup not found. Installing via official installer..."

    $rustupInit = Join-Path $env:TEMP 'rustup-init.exe'
    $arch = if ([Environment]::Is64BitOperatingSystem) { 'x86_64' } else { 'i686' }
    $url  = "https://win.rustup.rs/$arch"

    Invoke-WebRequest -Uri $url -OutFile $rustupInit -UseBasicParsing
    & $rustupInit -y --default-toolchain none --no-modify-path
    if ($LASTEXITCODE -ne 0) {
        throw "rustup-init failed with exit code $LASTEXITCODE"
    }

    # Make cargo/rustup available in this session without reopening the shell.
    $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path $cargoBin) {
        $env:PATH = "$cargoBin;$env:PATH"
    }
    Remove-Item $rustupInit -ErrorAction SilentlyContinue
} else {
    Write-Info "rustup found: $(rustup --version)"
}

# ---------------------------------------------------------------------------
# 2. Toolchain + components (driven by rust-toolchain.toml)
# ---------------------------------------------------------------------------
Write-Step "Installing toolchain pinned in rust-toolchain.toml"

# `rustup show` triggers installation of the pinned toolchain and components.
rustup show
if ($LASTEXITCODE -ne 0) { throw "rustup show failed" }

Write-Info "rustc:  $(rustc --version)"
Write-Info "cargo:  $(cargo --version)"

# ---------------------------------------------------------------------------
# 3. (Optional) cargo-nextest
# ---------------------------------------------------------------------------
if (-not $SkipNextest) {
    Write-Step "Ensuring cargo-nextest is installed"
    if (Test-Command 'cargo-nextest') {
        Write-Info "cargo-nextest already installed."
    } else {
        # Prefer the prebuilt binary; fall back to `cargo install` if unavailable.
        Write-Info "Installing cargo-nextest (prebuilt binary)..."
        cargo install cargo-nextest --locked
        if ($LASTEXITCODE -ne 0) {
            Write-Warning "cargo-nextest install failed; tests will fall back to 'cargo test'."
        }
    }
}

# ---------------------------------------------------------------------------
# 4. Release build
# ---------------------------------------------------------------------------
Write-Step "Building AgentShim (release)"

if ($WorkspaceBuild) {
    Write-Info "cargo build --release --workspace"
    cargo build --release --workspace
} else {
    Write-Info "cargo build --release -p agent-shim"
    cargo build --release -p agent-shim
}
if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }

# ---------------------------------------------------------------------------
# 5. (Optional) tests
# ---------------------------------------------------------------------------
if ($RunTests) {
    Write-Step "Running tests"
    if (Test-Command 'cargo-nextest') {
        cargo nextest run --workspace --release
    } else {
        cargo test --workspace --release
    }
    if ($LASTEXITCODE -ne 0) { throw "tests failed with exit code $LASTEXITCODE" }
}

# ---------------------------------------------------------------------------
# 6. Report binary location
# ---------------------------------------------------------------------------
$binary = Join-Path $RepoRoot 'target\release\agent-shim.exe'
Write-Step "Build complete"
if (Test-Path $binary) {
    $size = '{0:N2} MB' -f ((Get-Item $binary).Length / 1MB)
    Write-Host "    Binary: $binary ($size)" -ForegroundColor Green
    Write-Host "    Run:    .\target\release\agent-shim.exe --help" -ForegroundColor Green
} else {
    Write-Warning "Expected binary not found at $binary. Inspect target\release\ manually."
}
