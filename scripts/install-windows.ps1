[CmdletBinding()]
param(
    [switch]$Startup,
    [switch]$NoBuild
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$sourceExe = Join-Path $repoRoot 'target\release\orpheus.exe'
$installDir = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'Programs\Orpheus'
$installedExe = Join-Path $installDir 'orpheus.exe'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$legacyStartup = Join-Path ([Environment]::GetFolderPath('Startup')) 'Orpheus.lnk'
$existingStartup = Get-ItemProperty -LiteralPath $runKey -Name Orpheus -ErrorAction SilentlyContinue
$enableStartup = $Startup -or ($null -ne $existingStartup) -or (Test-Path -LiteralPath $legacyStartup)

if (-not $NoBuild) {
    Push-Location $repoRoot
    try {
        & cargo build --release --locked
        if ($LASTEXITCODE -ne 0) {
            throw 'The release build failed.'
        }
    }
    finally {
        Pop-Location
    }
}

if (-not (Test-Path -LiteralPath $sourceExe -PathType Leaf)) {
    throw "Release executable not found: $sourceExe"
}

$running = Get-Process -Name orpheus -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $installedExe }
if ($running) {
    throw 'Quit the installed Orpheus app before updating it.'
}

New-Item -ItemType Directory -Path $installDir -Force | Out-Null
Copy-Item -LiteralPath $sourceExe -Destination $installedExe -Force
Copy-Item -LiteralPath (Join-Path $repoRoot 'LICENSE') -Destination $installDir -Force
Copy-Item -LiteralPath (Join-Path $repoRoot 'assets\icon-LICENSE.txt') -Destination $installDir -Force
Copy-Item -LiteralPath (Join-Path $repoRoot 'assets\fonts\OFL.txt') -Destination (Join-Path $installDir 'Geist-OFL.txt') -Force

$shell = New-Object -ComObject WScript.Shell
$shortcutFolders = @([Environment]::GetFolderPath('Programs'))
foreach ($folder in $shortcutFolders) {
    $shortcut = $shell.CreateShortcut((Join-Path $folder 'Orpheus.lnk'))
    $shortcut.TargetPath = $installedExe
    $shortcut.Arguments = 'gui'
    $shortcut.WorkingDirectory = $installDir
    $shortcut.Description = 'Orpheus mouse polling control'
    $shortcut.IconLocation = "$installedExe,0"
    $shortcut.WindowStyle = 1
    $shortcut.Save()
}

if ($enableStartup) {
    New-Item -Path $runKey -Force | Out-Null
    New-ItemProperty -LiteralPath $runKey -Name Orpheus -PropertyType String -Value "`"$installedExe`" gui" -Force | Out-Null
    if (Test-Path -LiteralPath $legacyStartup) {
        Remove-Item -LiteralPath $legacyStartup
    }
}

Write-Output "Installed Orpheus to $installedExe"
if ($enableStartup) {
    Write-Output 'Orpheus will open at Windows sign-in for the current user.'
}
