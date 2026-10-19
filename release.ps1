# Builds a signed Overseer installer and the latest.json the in-app updater reads.
#   .\release.ps1                      build only (installer copied to the folder above this repo)
#   .\release.ps1 -Publish -Notes "…"  also create the GitHub release (the version must be committed and pushed)
param(
    [switch]$Publish,
    [string]$Notes = ""
)
$ErrorActionPreference = 'Stop'
$repo = 'Cboss/My_Task_Manager'
$root = $PSScriptRoot
$tauri = Join-Path $root 'src-tauri'
$key = Join-Path $env:USERPROFILE '.tauri\overseer.key'
$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
$gh = 'C:\Program Files\GitHub CLI\gh.exe'
$git = 'C:\Program Files\Git\cmd\git.exe'

if (-not (Test-Path $key)) { throw "Signing key not found at $key. Without it, installed copies can't verify updates." }
$version = (Get-Content (Join-Path $tauri 'tauri.conf.json') -Raw | ConvertFrom-Json).version
$tag = "v$version"
Write-Host "== Overseer $version ==" -ForegroundColor Cyan

# Windows PowerShell turns a native tool's stderr into errors, which 'Stop' would treat as
# fatal even when the tool succeeded. Run natives leniently and check $LASTEXITCODE instead.
function Invoke-Native([scriptblock]$Block) {
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $Block } finally { $ErrorActionPreference = $saved }
}

if ($Publish) {
    Invoke-Native { & $gh release view $tag --repo $repo 2>&1 | Out-Null }
    if ($LASTEXITCODE -eq 0) { throw "Release $tag already exists. Bump the version in tauri.conf.json and Cargo.toml first." }
    $ahead = Invoke-Native { & $git -C $root rev-list --count origin/main..HEAD }
    if ("$ahead".Trim() -ne '0') { throw "There are $ahead unpushed commit(s). Push first so the release tag points at this code." }
}

# Sign with the key file. The key has no password.
$env:TAURI_SIGNING_PRIVATE_KEY = $key
[Environment]::SetEnvironmentVariable('TAURI_SIGNING_PRIVATE_KEY_PASSWORD', '', 'Process')
Push-Location $tauri
try {
    Invoke-Native { & $cargo tauri build 2>&1 | ForEach-Object { "$_" } | Where-Object { $_ -notmatch '^\s+Compiling ' } }
    if ($LASTEXITCODE -ne 0) { throw "Build failed" }
} finally { Pop-Location }

$nsis = Join-Path $tauri 'target\release\bundle\nsis'
$setupName = "Overseer_${version}_x64-setup.exe"
$setup = Join-Path $nsis $setupName
$sig = "$setup.sig"
if (-not (Test-Path $sig)) { throw "No signature next to the installer; was the build signed?" }

$latest = [ordered]@{
    version   = $version
    notes     = $Notes
    pub_date  = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    platforms = [ordered]@{
        'windows-x86_64' = [ordered]@{
            signature = (Get-Content $sig -Raw).Trim()
            url       = "https://github.com/$repo/releases/download/$tag/$setupName"
        }
    }
}
$latestPath = Join-Path $nsis 'latest.json'
[IO.File]::WriteAllText($latestPath, ($latest | ConvertTo-Json -Depth 5), (New-Object Text.UTF8Encoding $false))

# Handy copy next to the repo (Desktop\Onix), replacing older installers.
$drop = Split-Path $root -Parent
Get-ChildItem $drop -Filter 'Overseer_*_x64-setup.exe' | Remove-Item
Copy-Item $setup $drop
Write-Host "Installer: $(Join-Path $drop $setupName)" -ForegroundColor Green

if ($Publish) {
    $title = "Overseer $version"
    $body = if ($Notes) { $Notes } else { $title }
    Invoke-Native { & $gh release create $tag $setup $latestPath --repo $repo --target main --title $title --notes $body 2>&1 | ForEach-Object { "$_" } }
    if ($LASTEXITCODE -ne 0) { throw "gh release create failed" }
    Write-Host "Published $tag. Installed copies will offer the update within 6 hours (or on next start)." -ForegroundColor Green
}
