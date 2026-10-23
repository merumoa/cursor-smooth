# Puts (or refreshes) the desktop shortcut for the tray companion.
# The shortcut passes --settings, so a click always lands on the settings window;
# if a tray instance already runs, it just reopens that window.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$exe = Join-Path $root 'bin\CursorSmoothTray.exe'
if (-not (Test-Path $exe)) { throw "build the tray first: pwsh -File tray\build.ps1" }

$link = Join-Path ([Environment]::GetFolderPath('Desktop')) 'CursorSmooth.lnk'
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($link)
$shortcut.TargetPath = $exe
$shortcut.Arguments = '--settings'
$shortcut.WorkingDirectory = Join-Path $root 'bin'
$shortcut.IconLocation = "$exe,0"
$shortcut.Description = 'CursorSmooth: cursor smoothing engine, settings and start/stop'
$shortcut.Save()
Write-Output ("shortcut: " + $link)
