# Run on a disposable Windows runner, with the same CPU as the package.
param(
    [Parameter(Mandatory)][string]$Installer,
    [Parameter(Mandatory)][ValidateSet('x86_64')][string]$Arch
)
$ErrorActionPreference = 'Stop'
$msi = (Resolve-Path $Installer).Path
$installDir = Join-Path $env:ProgramFiles 'Podbag Desktop'
$engine = Join-Path $installDir 'podbag.exe'
$gui = Join-Path $installDir 'podbag-desktop.exe'
if (Test-Path $installDir) { throw "An installation already exists at $installDir" }

function Invoke-Msi([string]$Operation) {
    $log = Join-Path $env:TEMP "podbag-msi-$Arch-$Operation.log"
    $p = Start-Process msiexec.exe -Wait -PassThru -ArgumentList "/$Operation `"$msi`" /qn /norestart /lv `"$log`""
    if ($p.ExitCode -ne 0) {
        Get-Content $log -Tail 100
        throw "Windows Installer returned $($p.ExitCode)"
    }
}

function Assert-PeMachine([string]$Path, [int]$Expected) {
    $stream = [IO.File]::OpenRead($Path)
    try {
        $reader = [IO.BinaryReader]::new($stream)
        $stream.Position = 0x3c
        $stream.Position = $reader.ReadUInt32()
        if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne $Expected) {
            throw "Unexpected PE architecture in $Path"
        }
    } finally {
        $stream.Dispose()
    }
}

Invoke-Msi i
try {
    $machine = @{ x86_64 = 0x8664 }[$Arch]
    Assert-PeMachine $gui $machine
    Assert-PeMachine $engine $machine
    & $engine --version
    if ($LASTEXITCODE -ne 0) { throw 'The installed engine launcher failed' }

    # Use a fresh WebView2 profile and enable browser automation only for this process.
    $profile = Join-Path $env:TEMP ("podbag-webview-" + [guid]::NewGuid())
    $env:WEBVIEW2_USER_DATA_FOLDER = $profile
    $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = '--remote-debugging-port=9222 --lang=en-US'
    $app = Start-Process $gui -PassThru
    try {
        node "$PSScriptRoot/test-window.cjs" "target/podbag-desktop-$Arch.png"
        if ($LASTEXITCODE -ne 0) { throw 'The installed desktop window failed its smoke test' }
    } finally {
        if (!$app.HasExited) {
            Stop-Process -Id $app.Id -Force
            $app.WaitForExit()
        }
    }
} finally {
    Invoke-Msi x
}
if ((Test-Path $engine) -or (Test-Path $gui)) { throw 'Uninstall left application binaries behind' }
Write-Output "Verified install, launch, and uninstall on Windows $Arch."
