<#
.SYNOPSIS
    Build the portable Windows distributable for ViewMusic.

.DESCRIPTION
    Produces a self-contained release build of the viz-app binary for the
    x86_64-pc-windows-msvc target and packages it into a zip archive alongside a
    short usage note. The VC runtime is linked statically (RUSTFLAGS
    "-C target-feature=+crt-static") so viewmusic.exe runs on a clean Windows 10
    1803+ x64 machine with no Visual C++ Redistributable installed.

    The script is idempotent: the staging directory and any previous archive are
    removed and rebuilt on every run.

.OUTPUTS
    target\viewmusic-windows-x64.zip — the distributable archive
    (viewmusic.exe + README-windows.txt).
#>

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# Repository root (this script lives in <root>\packaging).
$Root   = Split-Path -Parent $PSScriptRoot
$Target = 'x86_64-pc-windows-msvc'
$ExeName = 'viewmusic.exe'

$OutDir     = Join-Path $Root 'target'
$StageDir   = Join-Path $OutDir 'windows-stage'
$ZipPath    = Join-Path $OutDir 'viewmusic-windows-x64.zip'
$ReleaseExe = Join-Path $OutDir (Join-Path $Target (Join-Path 'release' $ExeName))

Write-Host "==> cargo build --release ($Target, crt-static)"
# Statically link the VC runtime so the exe runs without VCRedist on a clean machine.
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --manifest-path (Join-Path $Root 'Cargo.toml') -p viz-app --target $Target
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed with exit code $LASTEXITCODE"
}

if (-not (Test-Path $ReleaseExe)) {
    throw "expected build output not found: $ReleaseExe"
}

Write-Host "==> staging $StageDir"
# Idempotent: wipe any previous stage + archive before re-creating them.
if (Test-Path $StageDir) { Remove-Item -Recurse -Force $StageDir }
if (Test-Path $ZipPath)  { Remove-Item -Force $ZipPath }
New-Item -ItemType Directory -Path $StageDir | Out-Null

Copy-Item $ReleaseExe (Join-Path $StageDir $ExeName)

# Short usage note shipped inside the archive.
$readme = @'
ViewMusic for Windows (x64)
===========================

A real-time system-audio visualizer. It captures whatever your PC is playing
(via WASAPI loopback) and renders beat-reactive visuals at 60 fps.

Requirements
------------
  Windows 10 version 1803 or newer, 64-bit.
  No driver, no audio routing, and no Visual C++ Redistributable required
  (the runtime is linked statically).

Run
---
  Double-click viewmusic.exe, then play audio from any app.

SmartScreen
-----------
  This build is not code-signed, so Microsoft Defender SmartScreen may show an
  "unrecognized app" warning on first run. Click "More info", then
  "Run anyway". The warning suppresses itself as the download accrues
  reputation; it is not a security gate.

State and logs
--------------
  Settings:  %APPDATA%\acrive82\viewmusic\config\
  Artifacts: %APPDATA%\acrive82\viewmusic\config\artifacts\
  Logs:      %LOCALAPPDATA%\acrive82\viewmusic\data\logs\viewmusic.log

License
-------
  MIT. See https://github.com/acrive82/viewmusic.
'@
Set-Content -Path (Join-Path $StageDir 'README-windows.txt') -Value $readme -Encoding utf8

Write-Host "==> compressing $ZipPath"
Compress-Archive -Path (Join-Path $StageDir '*') -DestinationPath $ZipPath -Force

Write-Host "==> done"
Write-Output $ZipPath
