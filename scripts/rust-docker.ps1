#!/usr/bin/env pwsh
# Run cargo against rust/ inside the pinned Linux toolchain container.
#
# Why: this machine has no local Rust toolchain, and running cargo in the same
# image the Dockerfile uses keeps local results identical to CI and to the
# release build. The cargo target directory lives in a named volume so
# incremental builds survive between runs.
#
# The cargo command line is a single quoted string so PowerShell never tries to
# interpret flags such as `-D` or `--all-targets`.
#
# Examples:
#   ./scripts/rust-docker.ps1 "fmt --check"
#   ./scripts/rust-docker.ps1 "clippy --workspace --all-targets -- -D warnings"
#   ./scripts/rust-docker.ps1 "test --workspace"
#   ./scripts/rust-docker.ps1 "test -p pg-ibkr --features sdk"
[CmdletBinding()]
param(
    [Parameter(Position = 0, Mandatory = $true)]
    [string] $CargoCommandLine,
    [string] $Image = "rust:1.98.1-bookworm",
    [string] $TargetVolume = "pgtsy-cargo-target"
)

$repoRoot = Split-Path -Parent $PSScriptRoot
$rustDir = Join-Path $repoRoot "rust"
if (-not (Test-Path $rustDir)) {
    throw "rust workspace not found at $rustDir"
}

docker image inspect $Image 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) {
    Write-Host "image '$Image' is not present locally." -ForegroundColor Red
    Write-Host "run ./scripts/pull-base-images.ps1 first." -ForegroundColor Red
    exit 1
}

# The whole repo is mounted, not just rust/, so relative paths used by the
# Makefile and the replay targets (../strategies, ../data/replay) resolve the
# same way inside the container as they do on the host.
& docker run --rm `
    -v "${repoRoot}:/src" `
    -v "${TargetVolume}:/cargo-target" `
    -e CARGO_TARGET_DIR=/cargo-target `
    -w /src/rust `
    $Image `
    sh -c "cargo $CargoCommandLine"

exit $LASTEXITCODE
