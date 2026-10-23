# Builds the tray companion with the in-box .NET Framework compiler.
# No SDK, no NuGet: the icon is generated at build time, the sources are plain
# WinForms so the binary stays small and portable.
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$bin = Join-Path $here 'bin'
New-Item -ItemType Directory -Force -Path $bin | Out-Null

$iconPath = Join-Path $bin 'app.ico'
# The icon needs the full .NET Framework image stack, so it is drawn by the
# in-box Windows PowerShell 5.1 rather than by pwsh 7 (System.Drawing.Common).
$ps51 = Join-Path $env:WINDIR 'System32\WindowsPowerShell\v1.0\powershell.exe'
& $ps51 -NoProfile -ExecutionPolicy Bypass -File (Join-Path $here 'make-icon.ps1') -Path $iconPath
if ($LASTEXITCODE -ne 0) { throw "icon generation failed with exit code $LASTEXITCODE" }

$csc = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
& $csc /nologo /codepage:65001 /optimize+ /target:winexe `
    "/win32icon:$iconPath" `
    "/out:$(Join-Path $bin 'CursorSmoothTray.exe')" `
    /r:System.Windows.Forms.dll /r:System.Drawing.dll /r:System.Runtime.Serialization.dll `
    (Join-Path $here 'CursorSmoothTray.cs')
if ($LASTEXITCODE -ne 0) { throw "csc failed with exit code $LASTEXITCODE" }
Write-Host "built $(Join-Path $bin 'CursorSmoothTray.exe')"
