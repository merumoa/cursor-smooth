# End-to-end verification of the tray companion: real process, real controls,
# real engine child. Run under Windows PowerShell 5.1 (System.Drawing + WinForms):
#   powershell -Version 5 -NoProfile -File tray\verify-tray.ps1
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$probe = Join-Path $root 'ui-probe.ps1'
$exe = Join-Path $root 'bin\CursorSmoothTray.exe'
$script:failures = 0

function Step([string]$name, [scriptblock]$body) {
    try {
        $result = & $body
        if ($result) {
            Write-Output ("PASS  " + $name + "  " + $result)
        } else {
            Write-Output ("PASS  " + $name)
        }
    } catch {
        $script:failures++
        Write-Output ("FAIL  " + $name + ": " + $_.Exception.Message)
    }
}

function Invoke-Probe([hashtable]$arguments) {
    & $probe @arguments
}

function Encode([string]$text) {
    return [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($text))
}

function Engine-Processes {
    return @(Get-CimInstance Win32_Process -Filter "Name = 'cursor-smooth.exe'" | Where-Object { $_.CommandLine -like '*--stop-event*' })
}

function Wait-Until([scriptblock]$condition, [int]$timeoutMs) {
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    while ((Get-Date) -lt $deadline) {
        if (& $condition) { return $true }
        Start-Sleep -Milliseconds 150
    }
    return (& $condition)
}

# Clean slate: earlier manual launches or aborted runs would steal the single
# instance and the engine mutex.
Get-Process -Name 'CursorSmoothTray' -ErrorAction SilentlyContinue | Stop-Process -Force
foreach ($child in Engine-Processes) { Stop-Process -Id $child.ProcessId -Force }
Start-Sleep -Milliseconds 500

$tray = $null
try {
    Step 'launch opens settings' {
        $line = Invoke-Probe @{ Action = 'launch'; Arguments = '--settings' }
        $script:trayPid = [int]($line -replace 'pid=', '')
        if (-not (Wait-Until { (Invoke-Probe @{ Action = 'hidden'; TargetPid = $script:trayPid }) -eq 'visible=True' } 8000)) {
            throw 'settings window never appeared'
        }
        'pid=' + $script:trayPid
    }

    Step 'controls rendered' {
        $dump = Invoke-Probe @{ Action = 'list'; TargetPid = $script:trayPid } | Out-String
        foreach ($required in @('Путь к cursor-smooth.exe:', 'Запустить', 'Остановить', 'doctor', 'Обзор', 'Состояние: работает')) {
            if ($dump -notlike ('*' + $required + '*')) { throw ('missing control text: ' + $required) }
        }
        'layout ok'
    }

    Step 'engine started by tray' {
        if (-not (Wait-Until { @(Engine-Processes).Count -eq 1 } 8000)) {
            $raw = @(Get-CimInstance Win32_Process -Filter "Name = 'cursor-smooth.exe'")
            $tail = (Get-Content (Join-Path $root 'bin\tray.log') -Tail 3) -join ' | '
            $detail = ($raw | ForEach-Object { 'pid=' + $_.ProcessId + ' cmd=' + $_.CommandLine }) -join ' ;; '
            throw ('engine child not running; raw=' + $raw.Count + '; ' + $detail + '; log=' + $tail)
        }
        $command = (Engine-Processes)[0].CommandLine
        if ($command -notlike '*run --stop-event*') { throw ('unexpected engine command line: ' + $command) }
        'child=' + (Engine-Processes)[0].ProcessId
    }

    Step 'screenshot' {
        $shot = Join-Path $root 'bin\settings.png'
        Invoke-Probe @{ Action = 'shot'; TargetPid = $script:trayPid; Out = $shot }
    }

    Step 'stop button stops engine gracefully' {
        Invoke-Probe @{ Action = 'click'; TargetPid = $script:trayPid; Text64 = (Encode 'Остановить') } | Out-Null
        if (-not (Wait-Until { @(Engine-Processes).Count -eq 0 } 6000)) { throw 'engine still running after stop' }
        $dump = Invoke-Probe @{ Action = 'list'; TargetPid = $script:trayPid } | Out-String
        if ($dump -notlike '*Состояние: остановлен*') { throw 'status label not updated' }
        'engine stopped'
    }

    Step 'start button restarts engine' {
        Invoke-Probe @{ Action = 'click'; TargetPid = $script:trayPid; Text64 = (Encode 'Запустить') } | Out-Null
        if (-not (Wait-Until { @(Engine-Processes).Count -eq 1 } 8000)) {
            $tail = (Get-Content (Join-Path $root 'bin\tray.log') -Tail 3) -join ' | '
            throw ('engine did not restart; log=' + $tail)
        }
        $dump = Invoke-Probe @{ Action = 'list'; TargetPid = $script:trayPid } | Out-String
        if ($dump -notlike '*Состояние: работает*') { throw 'status label did not return to running' }
        'engine restarted'
    }

    Step 'doctor button appends a report' {
        Invoke-Probe @{ Action = 'click'; TargetPid = $script:trayPid; Text64 = (Encode 'doctor') } | Out-Null
        Start-Sleep -Milliseconds 1200
        $dump = Invoke-Probe @{ Action = 'list'; TargetPid = $script:trayPid } | Out-String
        if ($dump -notlike '*--- doctor ---*') { throw 'doctor output missing from the log' }
        'doctor ran'
    }

    Step 'window close hides into the tray' {
        $result = Invoke-Probe @{ Action = 'closewin'; TargetPid = $script:trayPid } | Out-String
        if ($result -notlike '*alive=True*') { throw ('tray exited on close: ' + $result) }
        if ($result -notlike '*visibleAfterClose=False*') { throw ('window stayed visible: ' + $result) }
        $visible = Invoke-Probe @{ Action = 'hidden'; TargetPid = $script:trayPid }
        if ($visible -ne 'visible=False') { throw ('window visible after hide: ' + $visible) }
        'hidden, process alive'
    }

    Step 'second launch reopens settings of the running tray' {
        $again = Invoke-Probe @{ Action = 'launch' }
        $secondPid = [int]($again -replace 'pid=', '')
        if (-not (Wait-Until { (Invoke-Probe @{ Action = 'hidden'; TargetPid = $script:trayPid }) -eq 'visible=True' } 6000)) {
            throw 'settings did not reopen'
        }
        Start-Sleep -Milliseconds 800
        if (Get-Process -Id $secondPid -ErrorAction SilentlyContinue) { throw 'second tray instance kept running' }
        'single instance + reopen ok'
    }

    Step 'exit path stops the engine' {
        Invoke-Probe @{ Action = 'closewin'; TargetPid = $script:trayPid } | Out-Null
        Stop-Process -Id $script:trayPid -Force
        Start-Sleep -Milliseconds 600
        foreach ($child in Engine-Processes) { Stop-Process -Id $child.ProcessId -Force }
        Start-Sleep -Milliseconds 400
        $line = Invoke-Probe @{ Action = 'launch'; Arguments = '--exit-after-ms 5000' }
        $script:trayPid = [int]($line -replace 'pid=', '')
        if (-not (Wait-Until { @(Engine-Processes).Count -eq 1 } 8000)) { throw 'engine not started by restarted tray' }
        if (-not (Wait-Until { -not (Get-Process -Id $script:trayPid -ErrorAction SilentlyContinue) } 9000)) {
            throw 'tray did not exit after the test hook'
        }
        if (-not (Wait-Until { @(Engine-Processes).Count -eq 0 } 5000)) { throw 'engine survived tray exit' }
        'tray exited, engine stopped'
    }
} finally {
    if ($script:trayPid -and (Get-Process -Id $script:trayPid -ErrorAction SilentlyContinue)) {
        Stop-Process -Id $script:trayPid -Force
    }
    foreach ($child in Engine-Processes) { Stop-Process -Id $child.ProcessId -Force }
}

Write-Output ('verify: ' + $script:failures + ' failure(s)')
if ($script:failures -ne 0) { exit 1 }
Write-Output 'verify: ok'
exit 0
