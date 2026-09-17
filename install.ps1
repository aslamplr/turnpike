# turnpike installer - Windows (x86_64). macOS: use install.sh.
#
# Downloads the latest prebuilt turnpike binary from GitHub Releases and
# installs it to %LOCALAPPDATA%\turnpike\bin, then ensures that directory is on
# the User PATH (open a new terminal afterwards).
#
# Environment overrides:
#   TURNPIKE_VERSION=v0.1.1   pin a release instead of "latest"
#   TURNPIKE_SKIP_SHA256=1    skip checksum verification (not recommended)
$ErrorActionPreference = 'Stop'

$Repo = 'aslamplr/turnpike'
$Asset = 'turnpike-x86_64-pc-windows-msvc.zip'
$InstallDir = Join-Path $env:LOCALAPPDATA 'turnpike\bin'
$Version = if ($env:TURNPIKE_VERSION) { $env:TURNPIKE_VERSION } else { 'latest' }

if (-not [Environment]::Is64BitOperatingSystem) {
    Write-Error ('prebuilt turnpike binaries are published for Windows (x86_64) and ' +
                 'macOS (Apple Silicon) only. On this platform, build from source: cargo build --release')
    exit 1
}

if ($Version -eq 'latest') {
    $Base = "https://github.com/$Repo/releases/latest/download"
} elseif ($Version.StartsWith('v')) {
    $Base = "https://github.com/$Repo/releases/download/$Version"
} else {
    $Base = "https://github.com/$Repo/releases/download/v$Version"
}

$TmpZip = Join-Path $env:TEMP 'turnpike-download.zip'
$TmpDir = Join-Path $env:TEMP 'turnpike-extract'

Write-Host "Downloading turnpike ($Version)..."
Invoke-WebRequest -UseBasicParsing "$Base/$Asset" -OutFile $TmpZip

if ($env:TURNPIKE_SKIP_SHA256 -ne '1') {
    $Sums = Join-Path $env:TEMP 'turnpike-SHA256SUMS.txt'
    Invoke-WebRequest -UseBasicParsing "$Base/SHA256SUMS" -OutFile $Sums
    $line = Get-Content $Sums | Where-Object { $_ -match "\s$([regex]::Escape($Asset))$" }
    if (-not $line) { Write-Error "SHA256SUMS has no entry for $Asset"; exit 1 }
    $expected = (($line -split '\s+')[0]).Trim().ToLower()
    $actual = (Get-FileHash -Algorithm SHA256 $TmpZip).Hash.ToLower()
    if ($expected -ne $actual) { Write-Error "SHA256 mismatch for $Asset"; exit 1 }
}

if (Test-Path $TmpDir) { Remove-Item -Recurse -Force $TmpDir }
Expand-Archive -Path $TmpZip -DestinationPath $TmpDir -Force
New-Item -ItemType Directory -Force $InstallDir | Out-Null
Copy-Item -Force (Join-Path $TmpDir 'turnpike.exe') (Join-Path $InstallDir 'turnpike.exe')
Remove-Item -Force $TmpZip
Remove-Item -Recurse -Force $TmpDir

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $InstallDir) {
    [Environment]::SetEnvironmentVariable('Path', "$InstallDir;$userPath", 'User')
    Write-Host "Added $InstallDir to your User PATH - open a new terminal to use it."
}

Write-Host ''
Write-Host "turnpike $Version installed to $InstallDir\turnpike.exe"
Write-Host 'Start the gateway with: turnpike serve --init'
