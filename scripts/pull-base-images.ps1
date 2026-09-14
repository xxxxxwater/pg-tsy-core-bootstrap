#!/usr/bin/env pwsh
# Pull the Docker base images this repo needs through a registry mirror and
# re-tag them under their upstream names.
#
# Why this exists: on some networks the Docker daemon cannot reach Docker Hub
# (auth.docker.io times out) even though the host and build containers can reach
# crates.io/GitHub. Dockerfile and docker-compose.yml keep using the upstream
# image names, so we only need to seed the local image store once.
[CmdletBinding()]
param(
    [string] $Mirror = "docker.m.daocloud.io/library"
)

$ErrorActionPreference = "Stop"

# Keep these in sync with Dockerfile (FROM lines) and docker-compose.yml.
$images = @(
    "rust:1.98.1-bookworm",
    "debian:bookworm-slim",
    "postgres:17"
)

foreach ($image in $images) {
    $mirrored = "$Mirror/$image"
    Write-Host "pulling $mirrored" -ForegroundColor Cyan
    docker pull $mirrored
    if ($LASTEXITCODE -ne 0) {
        throw "failed to pull $mirrored"
    }
    docker tag $mirrored $image
    if ($LASTEXITCODE -ne 0) {
        throw "failed to tag $mirrored as $image"
    }
    Write-Host "  -> tagged as $image" -ForegroundColor Green
}

Write-Host "base images ready" -ForegroundColor Green
