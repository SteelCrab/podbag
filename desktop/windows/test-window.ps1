# Inspect the installed app through the same accessibility tree as a Windows user.
param([int]$AppPid, [string]$OutputFile)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class WindowCapture {
    [StructLayout(LayoutKind.Sequential)]
    public struct Rect { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr hwnd, out Rect rect);
    [DllImport("user32.dll")]
    public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
    [DllImport("user32.dll")]
    public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")]
    public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")]
    public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
}
'@
[void][WindowCapture]::SetProcessDPIAware()

$deadline = [DateTime]::UtcNow.AddSeconds(90)
$process = Get-Process -Id $AppPid
do {
    $process.Refresh()
    if ($process.HasExited) { throw "App exited with code $($process.ExitCode)" }
    $window = $process.MainWindowHandle
    if ($window -ne [IntPtr]::Zero -and $process.MainWindowTitle -eq 'podbag') { break }
    Start-Sleep -Milliseconds 400
} until ([DateTime]::UtcNow -gt $deadline)
if ($process.MainWindowTitle -ne 'podbag') { throw 'The main Podbag window did not appear' }
Write-Output "Opened window '$($process.MainWindowTitle)' in session $($process.SessionId)."
$root = [Windows.Automation.AutomationElement]::FromHandle($window)
$scope = [Windows.Automation.TreeScope]::Descendants

function Find-Text([string]$Name) {
    $condition = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::NameProperty, $Name)
    $root.FindFirst($scope, $condition)
}

function Activate-Button([string]$Name) {
    $named = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::NameProperty, $Name)
    $button = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::ControlTypeProperty, [Windows.Automation.ControlType]::Button)
    $item = $root.FindFirst($scope, [Windows.Automation.AndCondition]::new($named, $button))
    if ($item) {
        $invoke = $null
        if ($item.TryGetCurrentPattern([Windows.Automation.InvokePattern]::Pattern, [ref]$invoke)) {
            $invoke.Invoke()
            return
        }
    } else {
        # Some WebView2 versions expose the button's label as a separate text node.
        $item = $root.FindFirst($scope, $named)
    }
    if (!$item -or $item.Current.IsOffscreen) { return }
    $rect = $item.Current.BoundingRectangle
    if ($rect.IsEmpty -or $rect.Width -le 0 -or $rect.Height -le 0) { return }
    [void][WindowCapture]::SetForegroundWindow($window)
    if ([WindowCapture]::SetCursorPos(($rect.Left + $rect.Width / 2), ($rect.Top + $rect.Height / 2))) {
        [WindowCapture]::mouse_event(2, 0, 0, 0, [UIntPtr]::Zero)
        [WindowCapture]::mouse_event(4, 0, 0, 0, [UIntPtr]::Zero)
    }
}

# The first-launch prompts (star, then image compression) may appear after navigation
# becomes visible.
$deadline = [DateTime]::UtcNow.AddSeconds(90)
do {
    Activate-Button 'Skip'
    Activate-Button 'Not now'
    Activate-Button 'Training'
    Start-Sleep -Milliseconds 500
    $heading = Find-Text 'No modules installed.'
    if ($heading) { break }
} until ([DateTime]::UtcNow -gt $deadline)
if (!$heading) { throw 'The installed WebView did not open the English Training view' }

$bounds = New-Object WindowCapture+Rect
if (![WindowCapture]::GetWindowRect($window, [ref]$bounds)) { throw 'Cannot read window bounds' }
$image = [Drawing.Bitmap]::new(($bounds.Right - $bounds.Left), ($bounds.Bottom - $bounds.Top))
$graphics = [Drawing.Graphics]::FromImage($image)
$dc = $graphics.GetHdc()
try {
    if (![WindowCapture]::PrintWindow($window, $dc, 2)) { throw 'Cannot capture the app window' }
} finally {
    $graphics.ReleaseHdc($dc)
    $graphics.Dispose()
}
try {
    $image.Save([IO.Path]::GetFullPath($OutputFile), [Drawing.Imaging.ImageFormat]::Png)
} finally {
    $image.Dispose()
}
Write-Output 'Opened Training in the installed English desktop app.'
