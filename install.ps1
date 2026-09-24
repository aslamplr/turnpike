# turnpike installer - Windows (x86_64). macOS: use install.sh.
#
# Downloads the latest prebuilt turnpike binary from GitHub Releases and
# installs it to %LOCALAPPDATA%\turnpike\bin, then ensures that directory is on
# the User PATH (open a new terminal afterwards). Then offers to install the
# Desktop app.
#
# Environment overrides:
#   TURNPIKE_VERSION=v0.1.2   pin a release instead of "latest"
#   TURNPIKE_SKIP_SHA256=1    skip checksum verification (not recommended)
#   TURNPIKE_DESKTOP=0|1      skip / accept the Desktop prompt without asking
$ErrorActionPreference = 'Stop'

$Repo = 'aslamplr/turnpike'
$Asset = 'turnpike-x86_64-pc-windows-msvc.zip'
$DesktopAsset = 'turnpike_desktop-x86_64-pc-windows-msvc-setup.exe'
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

# --- Desktop app (optional) -------------------------------------------------

# `irm … | iex` is a non-interactive host by default, so PromptForChoice is not
# always available. Anything short of a positive answer - no prompt support, or
# a dismissed/failed one - means skip: the CLI half has already succeeded, and
# hanging the install for an optional extra would be the worse failure.
function Confirm-DesktopApp {
    switch ($env:TURNPIKE_DESKTOP) {
        { $_ -in '1', 'yes', 'true' }  { return $true }
        { $_ -in '0', 'no', 'false' }  { return $false }
        { $_ -ne '' -and $_ -ne $null } {
            Write-Warning "TURNPIKE_DESKTOP='$env:TURNPIKE_DESKTOP' not understood; ignoring"
        }
    }
    $Prompt = 'Install the turnpike Desktop app (menu-bar item + config window)?'
    $Choices = [System.Collections.ObjectModel.Collection[System.Management.Automation.Host.ChoiceDescription]]@(
        (New-Object System.Management.Automation.Host.ChoiceDescription '&Yes', 'Download and run the installer.'),
        (New-Object System.Management.Automation.Host.ChoiceDescription '&No',  'Install the CLI only.')
    )
    try {
        $answer = $Host.UI.PromptForChoice('turnpike', $Prompt, $Choices, 1)
        return $answer -eq 0
    } catch {
        Write-Host 'No console is available to ask about the Desktop app; skipping it.'
        return $false
    }
}

if (Confirm-DesktopApp) {
    $DesktopExe = Join-Path $env:TEMP $DesktopAsset
    Write-Host ''
    Write-Host "Downloading the Desktop app ($Version)..."
    Invoke-WebRequest -UseBasicParsing "$Base/$DesktopAsset" -OutFile $DesktopExe

    if ($env:TURNPIKE_SKIP_SHA256 -ne '1') {
        # A separate manifest, because the desktop and CLI halves publish on
        # their own jobs and one can fail without the other. Missing it is a
        # warning, not an error - hashing the wrong file is worse than not
        # hashing this one.
        try {
            $DesktopSums = Join-Path $env:TEMP 'turnpike-SHA256SUMS-desktop.txt'
            Invoke-WebRequest -UseBasicParsing "$Base/SHA256SUMS-desktop" -OutFile $DesktopSums
            $dline = Get-Content $DesktopSums | Where-Object { $_ -match "\s$([regex]::Escape($DesktopAsset))$" }
            if (-not $dline) { throw "SHA256SUMS-desktop has no entry for $DesktopAsset" }
            $dexpected = (($dline -split '\s+')[0]).Trim().ToLower()
            $dactual = (Get-FileHash -Algorithm SHA256 $DesktopExe).Hash.ToLower()
            if ($dexpected -ne $dactual) { throw "SHA256 mismatch for $DesktopAsset" }
        } catch {
            Write-Warning "Could not verify $DesktopAsset ($($_.Exception.Message)); continuing."
        }
    }

    # The NSIS installer is self-contained and takes over from here: it shows its
    # own progress, installs per-user, and registers the uninstaller. Waiting
    # keeps this script's exit code honest about whether it worked.
    Write-Host 'Launching the Desktop installer...'
    $proc = Start-Process -FilePath $DesktopExe -PassThru -Wait
    Remove-Item -Force $DesktopExe -ErrorAction SilentlyContinue
    if ($proc.ExitCode -eq 0) {
        Write-Host 'turnpike Desktop installed.'
    } else {
        Write-Warning "The Desktop installer exited with code $($proc.ExitCode)."
    }
    # Unsigned, so SmartScreen shows "Windows protected your PC" on first run.
    Write-Host 'The app is unsigned: if SmartScreen blocks setup, choose More info -> Run anyway.'
}
