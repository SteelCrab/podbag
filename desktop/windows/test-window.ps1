# Wait for the Podbag Desktop window of a process, check that it draws (not one plain
# color), save a screenshot, and close the window.
param([Parameter(Mandatory)][int]$AppPid, [Parameter(Mandatory)][string]$OutputFile)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
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
}
'@
[void][WindowCapture]::SetProcessDPIAware()
$title = 'Podbag Desktop'

$deadline = [DateTime]::UtcNow.AddSeconds(90)
$process = Get-Process -Id $AppPid
do {
    $process.Refresh()
    if ($process.HasExited) { throw "The app exited with code $($process.ExitCode)" }
    $window = $process.MainWindowHandle
    if ($window -ne [IntPtr]::Zero -and $process.MainWindowTitle -eq $title) { break }
    Start-Sleep -Milliseconds 400
} until ([DateTime]::UtcNow -gt $deadline)
if ($process.MainWindowTitle -ne $title) { throw "The '$title' window did not appear" }
Write-Output "Opened window '$title' in session $($process.SessionId)."

# The window as the app drew it: through DWM (PrintWindow with PW_RENDERFULLCONTENT,
# which includes Direct3D content), else from the screen.
function Get-WindowImage([bool]$FromScreen) {
    $r = New-Object WindowCapture+Rect
    if (![WindowCapture]::GetWindowRect($window, [ref]$r)) { throw 'Cannot read the window bounds' }
    $w = $r.Right - $r.Left
    $h = $r.Bottom - $r.Top
    if ($w -le 0 -or $h -le 0) { return $null }
    $image = [Drawing.Bitmap]::new($w, $h)
    $graphics = [Drawing.Graphics]::FromImage($image)
    try {
        if ($FromScreen) {
            [void][WindowCapture]::SetForegroundWindow($window)
            Start-Sleep -Milliseconds 300
            $graphics.CopyFromScreen($r.Left, $r.Top, 0, 0, $image.Size)
        } else {
            $dc = $graphics.GetHdc()
            try { [void][WindowCapture]::PrintWindow($window, $dc, 2) }
            finally { $graphics.ReleaseHdc($dc) }
        }
    } finally {
        $graphics.Dispose()
    }
    $image
}

# How many colors a grid of points in the image has, and the share of the most common one.
function Measure-Image([Drawing.Bitmap]$Image) {
    $counts = @{}
    $n = 0
    for ($y = 4; $y -lt $Image.Height; $y += [Math]::Max(1, [int]($Image.Height / 48))) {
        for ($x = 4; $x -lt $Image.Width; $x += [Math]::Max(1, [int]($Image.Width / 48))) {
            $c = $Image.GetPixel($x, $y).ToArgb()
            $counts[$c] = 1 + [int]$counts[$c]
            $n++
        }
    }
    $top = ($counts.Values | Measure-Object -Maximum).Maximum
    [pscustomobject]@{ Colors = $counts.Count; Top = $top / [Math]::Max(1, $n) }
}

# The first frames can come before the fonts and the views: wait until it has content.
$deadline = [DateTime]::UtcNow.AddSeconds(60)
$image = $null
$drawn = $false
do {
    foreach ($fromScreen in $false, $true) {
        if ($image) { $image.Dispose() }
        $image = Get-WindowImage $fromScreen
        if (!$image) { continue }
        $m = Measure-Image $image
        if ($m.Colors -ge 8 -and $m.Top -lt 0.95) {
            $drawn = $true
            $how = if ($fromScreen) { 'the screen' } else { 'PrintWindow' }
            Write-Output "The window draws: $($m.Colors) colors in a sample, from $how."
            break
        }
    }
    if (!$drawn) { Start-Sleep -Seconds 1 }
} until ($drawn -or [DateTime]::UtcNow -gt $deadline)
if ($image) {
    $image.Save([IO.Path]::GetFullPath($OutputFile), [Drawing.Imaging.ImageFormat]::Png)
    $image.Dispose()
}
if (!$drawn) { throw "The window stayed blank ($($m.Colors) colors, see $OutputFile)" }
[void]$process.CloseMainWindow()
Write-Output 'Closed the window.'
