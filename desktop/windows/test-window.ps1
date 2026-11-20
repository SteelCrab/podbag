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

# The first launch can present a Skip button. Wait for localized web content as
# well as the native title so a blank WebView cannot pass.
do {
    $skip = Find-Text 'Skip'
    if ($skip) {
        $invoke = $null
        if ($skip.TryGetCurrentPattern([Windows.Automation.InvokePattern]::Pattern, [ref]$invoke)) {
            $invoke.Invoke()
        }
    }
    $tutorials = Find-Text 'Tutorials'
    if ($tutorials) { break }
    Start-Sleep -Milliseconds 500
} until ([DateTime]::UtcNow -gt $deadline)
if (!$tutorials) { throw 'The WebView did not expose the English Tutorials navigation item' }
$invoke = $tutorials.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern)
$invoke.Invoke()
do {
    $heading = Find-Text 'Install a tutorial module'
    if ($heading) { break }
    Start-Sleep -Milliseconds 400
} until ([DateTime]::UtcNow -gt $deadline)
if (!$heading) { throw 'The Tutorials view did not open' }

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
Write-Output 'Opened Tutorials in the installed English desktop app.'
