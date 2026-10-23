param(
    [Parameter(Mandatory = $true)][ValidateSet('launch', 'list', 'shot', 'click', 'closewin', 'hidden', 'engine', 'text')]
    [string]$Action,
    [int]$TargetPid = 0,
    [string]$Text = '',
    [string]$Text64 = '',
    [string]$Out = '',
    [string]$Arguments = ''
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

if (-not ('TrayProbe' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public static class TrayProbe
{
    private delegate bool EnumProc(IntPtr handle, IntPtr param);

    [DllImport("user32.dll")]
    private static extern bool EnumWindows(EnumProc callback, IntPtr param);
    [DllImport("user32.dll")]
    private static extern bool EnumChildWindows(IntPtr parent, EnumProc callback, IntPtr param);
    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr handle, out uint pid);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetWindowTextW(IntPtr handle, StringBuilder text, int capacity);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "SendMessageW")]
    private static extern IntPtr SendMessageText(IntPtr handle, uint message, IntPtr wParam, StringBuilder lParam);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "SendMessageW")]
    private static extern IntPtr SendMessageRaw(IntPtr handle, uint message, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr handle);
    [DllImport("user32.dll")]
    private static extern bool IsWindow(IntPtr handle);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetClassNameW(IntPtr handle, StringBuilder text, int capacity);
    [DllImport("user32.dll")]
    private static extern bool GetWindowRect(IntPtr handle, out RECT rect);

    [StructLayout(LayoutKind.Sequential)]
    private struct RECT { public int Left, Top, Right, Bottom; }

    private static List<IntPtr> WindowsOf(int pid)
    {
        List<IntPtr> found = new List<IntPtr>();
        EnumWindows(delegate(IntPtr handle, IntPtr param)
        {
            uint owner;
            GetWindowThreadProcessId(handle, out owner);
            if (owner == (uint)pid)
            {
                found.Add(handle);
            }
            return true;
        }, IntPtr.Zero);
        return found;
    }

    public static IntPtr VisibleTopLevel(int pid)
    {
        foreach (IntPtr handle in WindowsOf(pid))
        {
            if (IsWindowVisible(handle))
            {
                return handle;
            }
        }
        return IntPtr.Zero;
    }

    public static string Dump(int pid)
    {
        StringBuilder report = new StringBuilder();
        List<IntPtr> tops = WindowsOf(pid);
        report.AppendLine("top-level windows: " + tops.Count);
        foreach (IntPtr top in tops)
        {
            report.AppendLine("window " + top.ToString("X") + " visible=" + IsWindowVisible(top) + " title='" + Caption(top) + "'");
            EnumChildWindows(top, delegate(IntPtr child, IntPtr param)
            {
                report.AppendLine("  " + ClassName(child) + "  '" + Caption(child) + "'");
                return true;
            }, IntPtr.Zero);
        }
        return report.ToString();
    }

    public static string Caption(IntPtr handle)
    {
        StringBuilder buffer = new StringBuilder(65536);
        SendMessageText(handle, 0x000D, (IntPtr)buffer.Capacity, buffer);
        return buffer.ToString();
    }

    public static string ClassName(IntPtr handle)
    {
        StringBuilder buffer = new StringBuilder(256);
        GetClassNameW(handle, buffer, buffer.Capacity);
        return buffer.ToString();
    }

    public static string ClickByText(int pid, string label)
    {
        string clicked = null;
        foreach (IntPtr top in WindowsOf(pid))
        {
            EnumChildWindows(top, delegate(IntPtr child, IntPtr param)
            {
                if (clicked == null && Caption(child) == label)
                {
                    SendMessageRaw(child, 0x00F5, IntPtr.Zero, IntPtr.Zero);
                    clicked = ClassName(child);
                }
                return true;
            }, IntPtr.Zero);
        }
        return clicked;
    }

    public static bool Close(IntPtr handle)
    {
        return SendMessageRaw(handle, 0x0112, (IntPtr)0xF060, IntPtr.Zero) == IntPtr.Zero || true;
    }

    public static int[] RectOf(IntPtr handle)
    {
        RECT rect;
        if (!GetWindowRect(handle, out rect))
        {
            return new int[] { 0, 0, 0, 0 };
        }
        return new int[] { rect.Left, rect.Top, rect.Right - rect.Left, rect.Bottom - rect.Top };
    }

    public static bool Alive(IntPtr handle)
    {
        return handle != IntPtr.Zero && IsWindow(handle);
    }
}
'@
}

if ($Text64) {
    # Labels are Russian; UTF-8 base64 survives any console codepage.
    $Text = [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String($Text64))
}

switch ($Action) {
    'launch' {
        $startArgs = @()
        if ($Arguments) { $startArgs = $Arguments.Split(' ', [StringSplitOptions]::RemoveEmptyEntries) }
        if ($startArgs.Count -gt 0) {
            $process = Start-Process -FilePath 'C:\projects\cursor-smooth\tray\bin\CursorSmoothTray.exe' -ArgumentList $startArgs -PassThru
        } else {
            $process = Start-Process -FilePath 'C:\projects\cursor-smooth\tray\bin\CursorSmoothTray.exe' -PassThru
        }
        Write-Output ("pid=" + $process.Id)
    }
    'list' {
        Write-Output ([TrayProbe]::Dump($TargetPid))
    }
    'shot' {
        $window = [TrayProbe]::VisibleTopLevel($TargetPid)
        if ($window -eq [IntPtr]::Zero) { Write-Output 'no visible window'; exit 1 }
        $rect = [TrayProbe]::RectOf($window)
        if ($rect[2] -le 0 -or $rect[3] -le 0) { Write-Output 'bad window rect'; exit 1 }
        $bitmap = New-Object System.Drawing.Bitmap $rect[2], $rect[3]
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        $graphics.CopyFromScreen($rect[0], $rect[1], 0, 0, (New-Object System.Drawing.Size $rect[2], $rect[3]))
        $bitmap.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
        $graphics.Dispose()
        $bitmap.Dispose()
        Write-Output ('saved ' + $rect[2] + 'x' + $rect[3] + ' -> ' + $Out)
    }
    'click' {
        $class = [TrayProbe]::ClickByText($TargetPid, $Text)
        if (-not $class) { Write-Output ('button not found: ' + $Text); exit 1 }
        Write-Output ('clicked ' + $class)
    }
    'text' {
        $window = [TrayProbe]::VisibleTopLevel($TargetPid)
        if ($window -eq [IntPtr]::Zero) { Write-Output 'no visible window'; exit 1 }
        Write-Output ([TrayProbe]::Dump($TargetPid))
    }
    'closewin' {
        $window = [TrayProbe]::VisibleTopLevel($TargetPid)
        if ($window -eq [IntPtr]::Zero) { Write-Output 'no visible window'; exit 1 }
        [TrayProbe]::Close($window) | Out-Null
        Start-Sleep -Milliseconds 400
        $stillVisible = [TrayProbe]::VisibleTopLevel($TargetPid)
        $alive = [TrayProbe]::Alive($window)
        Write-Output ('alive=' + $alive + ' visibleAfterClose=' + ($stillVisible -ne [IntPtr]::Zero))
    }
    'hidden' {
        $window = [TrayProbe]::VisibleTopLevel($TargetPid)
        Write-Output ('visible=' + ($window -ne [IntPtr]::Zero))
    }
    'engine' {
        $rows = Get-CimInstance Win32_Process -Filter "Name = 'cursor-smooth.exe'" |
            Select-Object ProcessId, @{n = 'CommandLine'; e = { $_.CommandLine } }
        if (-not $rows) { Write-Output 'engine processes: 0' } else {
            Write-Output ('engine processes: ' + @($rows).Count)
            $rows | ForEach-Object { Write-Output ('  pid=' + $_.ProcessId + ' ' + $_.CommandLine) }
        }
    }
}
