#Requires -Version 5.1
<#
.SYNOPSIS
    Creates (or removes) a Start Menu shortcut for the Megu client.

.DESCRIPTION
    Beginner-safe installer: writes a .lnk into the current user's
    Start Menu programs folder. No Administrator privileges required —
    it never touches HKLM or protected directories.

.EXAMPLE
    .\install-startmenu.ps1
    Creates "Megu" in Start Menu -> Programs.

.EXAMPLE
    .\install-startmenu.ps1 -ExePath "C:\Games\megu\megu-client-gpu.exe"
    Points the shortcut at a specific exe location.

.EXAMPLE
    .\install-startmenu.ps1 -Uninstall
    Removes the shortcut.
#>
[CmdletBinding()]
param(
    # Path to megu-client-gpu.exe. Defaults to a copy sitting next to
    # this script, which is where the release zip lays it out.
    [string]$ExePath = (Join-Path $PSScriptRoot 'megu-client-gpu.exe'),

    # Remove the Start Menu shortcut instead of creating it.
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'

$shortcutName = 'Megu.lnk'
$startMenuDir = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
$shortcutPath = Join-Path $startMenuDir $shortcutName

if ($Uninstall) {
    if (Test-Path -LiteralPath $shortcutPath) {
        Remove-Item -LiteralPath $shortcutPath -Force
        Write-Host "Removed Start Menu shortcut: $shortcutPath"
    } else {
        Write-Host "Nothing to remove: $shortcutPath does not exist."
    }
    return
}

if (-not (Test-Path -LiteralPath $ExePath)) {
    Write-Warning "Executable not found: $ExePath"
    Write-Warning "The shortcut will still be created; move the exe there or re-run with -ExePath."
}

if (-not (Test-Path -LiteralPath $startMenuDir)) {
    New-Item -ItemType Directory -Path $startMenuDir -Force | Out-Null
}

$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($shortcutPath)
$shortcut.TargetPath = $ExePath
$shortcut.WorkingDirectory = Split-Path -Parent $ExePath
$shortcut.Description = 'Megu Minecraft client'
$shortcut.IconLocation = "$ExePath,0"
$shortcut.Save()

Write-Host "Created Start Menu shortcut: $shortcutPath"
Write-Host "Target: $ExePath"
