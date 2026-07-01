#Requires -Version 5.1
<#
.SYNOPSIS
    Build a local WinProdexBar folder for the square display.

.DESCRIPTION
    Creates a runnable local package with both executables needed for the
    Windows-first device workflow:

    - codexbar-desktop.exe for the tray UI
    - codexbar.exe for serve/attention commands
    - run-display-server.ps1 with the LAN-safe display feed flags

    This is intentionally lighter than the release installer pipeline so the
    device integration can be tested before publishing signed release assets.
#>

param(
    [string]$OutDir = "",
    [switch]$SkipBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if (-not $OutDir) {
    $OutDir = Join-Path $RepoRoot "target\winprodexbar-display"
}

function Invoke-Native {
    param(
        [string]$FilePath,
        [string[]]$ArgumentList
    )

    & $FilePath @ArgumentList
    if ($LASTEXITCODE -ne 0) {
        throw "$FilePath exited with code $LASTEXITCODE"
    }
}

function Require-Command {
    param([string]$Name)

    $command = Get-Command $Name -ErrorAction SilentlyContinue
    if (-not $command) {
        throw "Missing required command: $Name"
    }
    return $command.Source
}

$cargo = Require-Command "cargo"
$pnpm = Require-Command "pnpm"

Push-Location $RepoRoot
try {
    if (-not $SkipBuild) {
        Invoke-Native $cargo @(
            "build",
            "--manifest-path", "rust\Cargo.toml",
            "--release",
            "--bin", "codexbar"
        )
        Invoke-Native $pnpm @(
            "--dir", "apps\desktop-tauri",
            "exec", "tauri", "build",
            "--ci", "--no-bundle"
        )
    }

    $cliExe = Join-Path $RepoRoot "target\release\codexbar.exe"
    $desktopExe = Join-Path $RepoRoot "target\release\codexbar-desktop-tauri.exe"
    $icon = Join-Path $RepoRoot "rust\icons\icon.ico"

    foreach ($path in @($cliExe, $desktopExe, $icon)) {
        if (-not (Test-Path -LiteralPath $path)) {
            throw "Missing expected build output: $path"
        }
    }

    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    Copy-Item -LiteralPath $cliExe -Destination (Join-Path $OutDir "codexbar.exe") -Force
    Copy-Item -LiteralPath $desktopExe -Destination (Join-Path $OutDir "codexbar-desktop.exe") -Force
    Copy-Item -LiteralPath $icon -Destination (Join-Path $OutDir "icon.ico") -Force

    $serverLauncher = @'
param(
    [string]$Secret = $env:CODEXBAR_DEVICE_SECRET,
    [string]$HostName = "0.0.0.0",
    [int]$Port = 8080
)

$ErrorActionPreference = "Stop"
if (-not $Secret) {
    throw "Pass -Secret or set CODEXBAR_DEVICE_SECRET before serving the display feed."
}

$attentionFile = Join-Path $env:APPDATA "CodexBar\attention.json"
& "$PSScriptRoot\codexbar.exe" serve `
    --host $HostName `
    --port $Port `
    --device-secret $Secret `
    --attention-file $attentionFile
'@
    $serverLauncher | Set-Content -Encoding ascii (Join-Path $OutDir "run-display-server.ps1")

    $readme = @"
# WinProdexBar Display Package

Run the tray app:

````powershell
.\codexbar-desktop.exe
````

Serve the GeekMagic display feed:

````powershell
.\run-display-server.ps1 -Secret "my-screen-secret"
````

Drive the attention takeover:

````powershell
.\codexbar.exe attention set --provider codex --reason approval --action OPEN
.\codexbar.exe attention clear
````
"@
    $readme | Set-Content -Encoding ascii (Join-Path $OutDir "README.md")

    Write-Host "Display package written to $OutDir"
    Get-ChildItem -LiteralPath $OutDir | Select-Object Name, Length, LastWriteTime | Format-Table -AutoSize
} finally {
    Pop-Location
}
