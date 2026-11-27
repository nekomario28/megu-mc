# SPDX-License-Identifier: LGPL-3.0-or-later
# Install or update Megu: irm https://raw.githubusercontent.com/nekomario28/megu-mc/main/tools/install_windows.ps1 | iex
[CmdletBinding()]
param(
    [string]$Version = 'v0.1.0-beta.1',
    [switch]$Rollback,
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'
$Repository = 'nekomario28/megu-mc'
$InstallRoot = Join-Path $env:LOCALAPPDATA 'megu'
$Current = Join-Path $InstallRoot 'current'
$Previous = Join-Path $InstallRoot 'previous'
$BinPath = $Current

function Write-Status([string]$Message) { Write-Host "[megu-install] $Message" }
function Add-UserPath([string]$Entry) {
    $parts = @([Environment]::GetEnvironmentVariable('Path', 'User') -split ';' | Where-Object { $_ })
    if ($parts -notcontains $Entry) {
        [Environment]::SetEnvironmentVariable('Path', (($parts + $Entry) -join ';'), 'User')
    }
    if (($env:Path -split ';') -notcontains $Entry) { $env:Path = "$Entry;$env:Path" }
}
function Remove-UserPath([string]$Entry) {
    $parts = @([Environment]::GetEnvironmentVariable('Path', 'User') -split ';' | Where-Object { $_ -and $_.TrimEnd('\') -ine $Entry.TrimEnd('\') })
    [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'User')
}

if ($Uninstall) {
    Remove-UserPath $BinPath
    if (Test-Path -LiteralPath $InstallRoot) { Remove-Item -LiteralPath $InstallRoot -Recurse -Force }
    Write-Status 'Uninstalled Megu and removed its user PATH entry.'
    return
}

if ($Rollback) {
    if (-not (Test-Path -LiteralPath $Previous -PathType Container)) { throw 'No previous installation is available to roll back to.' }
    $temporary = Join-Path $InstallRoot 'rollback-staging'
    if (Test-Path $temporary) { Remove-Item $temporary -Recurse -Force }
    if (Test-Path $Current) { Move-Item $Current $temporary }
    Move-Item $Previous $Current
    if (Test-Path $temporary) { Move-Item $temporary $Previous }
    Add-UserPath $BinPath
    Write-Status 'Rolled back to the previous installation.'
    return
}

if ($Version -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z][0-9A-Za-z.-]*)?$' -or $Version.Contains('..')) {
    throw "Invalid release version: $Version"
}
$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
$target = switch ($architecture) {
    'X64' { 'windows-x86_64' }
    'Arm64' { 'windows-arm64' }
    default { throw "Unsupported Windows architecture '$architecture' (supported: x64, ARM64)." }
}

$releaseUrl = "https://github.com/$Repository/releases/download/$Version"
$archiveName = "megu-$Version-$target.tar.gz"
$work = Join-Path ([IO.Path]::GetTempPath()) ("megu-install-" + [Guid]::NewGuid().ToString('N'))
$stage = Join-Path $InstallRoot 'staging'
New-Item -ItemType Directory -Path $work -Force | Out-Null
New-Item -ItemType Directory -Path $InstallRoot -Force | Out-Null
try {
    $sumsPath = Join-Path $work 'SHA256SUMS-all.txt'
    $archivePath = Join-Path $work $archiveName
    Invoke-WebRequest -Uri "$releaseUrl/SHA256SUMS-all.txt" -OutFile $sumsPath -UseBasicParsing
    Invoke-WebRequest -Uri "$releaseUrl/$archiveName" -OutFile $archivePath -UseBasicParsing

    $matching = @((Get-Content -LiteralPath $sumsPath) | Where-Object { $_ -cmatch ('^[0-9a-f]{64}  ' + [regex]::Escape($archiveName) + '$') })
    if ($matching.Count -ne 1) { throw "Expected exactly one SHA256SUMS-all.txt entry for $archiveName." }
    $expectedHash = ($matching[0] -split '  ', 2)[0]
    $actualHash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -cne $expectedHash) { throw "SHA-256 verification failed for $archiveName." }

    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
    New-Item -ItemType Directory -Path $stage | Out-Null
    tar.exe -xzf $archivePath -C $stage
    if ($LASTEXITCODE -ne 0) { throw 'Release archive extraction failed.' }
    $bundle = Join-Path $stage "megu-$Version-$target"
    if (-not (Test-Path -LiteralPath $bundle -PathType Container)) { throw "Expected archive directory is missing: $bundle" }
    $executables = @(Get-ChildItem -LiteralPath $bundle -Filter '*.exe' -File)
    if (-not ($executables | Where-Object Name -CEQ 'megu.exe')) { throw 'The verified release archive does not contain megu.exe.' }

    $ready = Join-Path $InstallRoot 'ready'
    if (Test-Path $ready) { Remove-Item $ready -Recurse -Force }
    Move-Item -LiteralPath $bundle -Destination $ready
    if (Test-Path $Previous) { Remove-Item $Previous -Recurse -Force }
    if (Test-Path $Current) { Move-Item -LiteralPath $Current -Destination $Previous }
    try {
        Move-Item -LiteralPath $ready -Destination $Current
    } catch {
        if (-not (Test-Path $Current) -and (Test-Path $Previous)) { Move-Item $Previous $Current }
        throw
    }
    Add-UserPath $BinPath
    Write-Status "Installed Megu $Version ($target) to $Current."
    Write-Status 'Open a new terminal, or run: $env:Path = [Environment]::GetEnvironmentVariable("Path", "User")'
} finally {
    if (Test-Path $work) { Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue }
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue }
}
