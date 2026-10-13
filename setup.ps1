# One-time toolchain setup for Overseer, then a first build + launch.
$ErrorActionPreference = 'Continue'

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$hasMsvc = (Test-Path $vswhere) -and (& $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property displayName)
if (-not $hasMsvc) {
    Write-Host "`n== Installing MSVC C++ Build Tools (accept the UAC prompt) ==" -ForegroundColor Cyan
    winget install --id Microsoft.VisualStudio.2022.BuildTools -e --accept-source-agreements --accept-package-agreements `
        --override "--passive --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
} else { Write-Host "MSVC build tools already installed" -ForegroundColor Green }

$cargo = "$env:USERPROFILE\.cargo\bin\cargo.exe"
if (-not (Test-Path $cargo)) {
    Write-Host "`n== Installing Rust (rustup) ==" -ForegroundColor Cyan
    winget install --id Rustlang.Rustup -e --accept-source-agreements --accept-package-agreements
} else { Write-Host "Rust already installed" -ForegroundColor Green }

if (-not (Test-Path $cargo)) {
    Write-Host "`nRust isn't installed (winget failed?). Run this script from a regular PowerShell window." -ForegroundColor Red
    exit 1
}

$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
& "$env:USERPROFILE\.cargo\bin\rustup.exe" default stable
& $cargo --version

Write-Host "`n== Building and launching Overseer (first build takes a few minutes) ==" -ForegroundColor Cyan
Set-Location (Join-Path $PSScriptRoot 'src-tauri')
& $cargo run
