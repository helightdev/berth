# Installs berth-remote for this user on Windows: the released program,
# checked against its SHA-256, into %LOCALAPPDATA%\Programs\berth-remote (or
# BERTH_INSTALL_DIR). berth-remote uses the hosts of a berth serve elsewhere.
#
#   irm https://raw.githubusercontent.com/helightdev/berth/main/install.ps1 | iex
#
# BERTH_VERSION picks a release tag such as v0.5.0; the default is the latest.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'helightdev/berth'
$asset = 'berth-remote-x86_64-windows.zip'
$dir = if ($env:BERTH_INSTALL_DIR) { $env:BERTH_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\berth-remote' }
$version = if ($env:BERTH_VERSION) { $env:BERTH_VERSION } else { 'latest' }

function Fail([string] $message) {
    Write-Host "install: $message" -ForegroundColor Red
    # Not exit: the script runs inside the caller's PowerShell, which exit would close.
    throw "install: $message"
}

if (-not [Environment]::Is64BitOperatingSystem -or $env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
    Fail "no released program for this processor: build it with cargo install --locked --git https://github.com/$repo berth-remote"
}

$url = if ($version -eq 'latest') {
    "https://github.com/$repo/releases/latest/download"
} else {
    "https://github.com/$repo/releases/download/$version"
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("berth-remote-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $work | Out-Null
try {
    $zip = Join-Path $work $asset
    try { Invoke-WebRequest -UseBasicParsing -Uri "$url/$asset" -OutFile $zip } catch { Fail "could not download $url/$asset" }
    try { Invoke-WebRequest -UseBasicParsing -Uri "$url/$asset.sha256" -OutFile "$zip.sha256" } catch { Fail 'could not download the checksum' }
    $want = ((Get-Content -Raw "$zip.sha256").Trim() -split '\s+')[0]
    if ((Get-FileHash -Algorithm SHA256 $zip).Hash -ne $want) { Fail 'the download does not match its checksum' }
    Expand-Archive -Path $zip -DestinationPath (Join-Path $work 'unpacked')
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $program = Join-Path $dir 'berth-remote.exe'
    Copy-Item -Force (Join-Path $work 'unpacked\berth-remote.exe') $program
} finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}

$said = try { & $program --version } catch { 'berth-remote' }
Write-Host "Installed $said to $program"
$path = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (($path -split ';') -contains $dir)) {
    Write-Host "Add $dir to your Path to run it as berth-remote:"
    Write-Host "  [Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path', 'User') + ';$dir', 'User')"
}
