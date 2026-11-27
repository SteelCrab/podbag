# Install Podbag Desktop from its .msi, check the install, open the app, and uninstall it.
# With -Previous, install that older .msi first (the last one the Tauri bundler made) and
# check that the new one replaces it in place.
# Run on a disposable Windows runner, with the same CPU as the package.
param(
    [Parameter(Mandatory)][string]$Installer,
    [Parameter(Mandatory)][ValidateSet('x86_64', 'aarch64')][string]$Arch,
    [string]$Previous,
    # The podbag-desktop.exe that went into the .msi, to compare with the installed one.
    [string]$App = 'target/release/podbag-desktop.exe',
    [string]$Screenshot = "target/podbag-desktop-$Arch.png"
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/msi.ps1"
$nativeArch = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
$expectedHost = @{ x86_64 = 'X64'; aarch64 = 'Arm64' }[$Arch]
if ($nativeArch -ne $expectedHost) { throw "Expected a native $Arch runner, found $nativeArch" }

$msi = (Resolve-Path $Installer).Path
$installDir = Join-Path $env:ProgramFiles 'Podbag Desktop'
$engine = Join-Path $installDir 'podbag.exe'
$gui = Join-Path $installDir 'podbag-desktop.exe'
$startMenu = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\Podbag Desktop\Podbag Desktop.lnk'
$desktopLink = Join-Path $env:PUBLIC 'Desktop\Podbag Desktop.lnk'
if (Test-Path $installDir) { throw "An installation already exists at $installDir" }
if (@(Get-RelatedProducts).Count -ne 0) { throw 'Podbag Desktop is already installed' }

function Invoke-Msi([string]$Operation, [string]$Package, [string]$Name) {
    $log = Join-Path $env:TEMP "podbag-msi-$Arch-$Name.log"
    $p = Start-Process msiexec.exe -Wait -PassThru -ArgumentList "/$Operation `"$Package`" /qn /norestart /lv `"$log`""
    if ($p.ExitCode -ne 0) {
        Get-Content $log -Tail 100
        throw "Windows Installer returned $($p.ExitCode) for $Name"
    }
}

function Get-ShortcutTarget([string]$Path) {
    if (!(Test-Path $Path)) { throw "There is no shortcut $Path" }
    (New-Object -ComObject WScript.Shell).CreateShortcut($Path).TargetPath
}

# Stop every program that runs from the install folder: the app and the engine it started.
function Stop-Installed {
    Get-Process | Where-Object { $_.Path -and $_.Path.StartsWith("$installDir\", 'OrdinalIgnoreCase') } |
        ForEach-Object {
            Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
            $_.WaitForExit(10000) | Out-Null
        }
}

if ($Previous) {
    Invoke-Msi i (Resolve-Path $Previous).Path previous
    $old = @(Get-RelatedProducts)
    if ($old.Count -ne 1) { throw "The previous .msi installed $($old.Count) products" }
    if (!(Test-Path $gui)) { throw "The previous .msi did not install $gui" }
    $oldVersion = Get-ProductInfo $old[0] VersionString
    Write-Output "Installed the previous Podbag Desktop $oldVersion ($($old[0]))."
}

Invoke-Msi i $msi install
try {
    # One Podbag Desktop: the product of this .msi, in the folder of the earlier one.
    $products = @(Get-RelatedProducts)
    if ($products.Count -ne 1) { throw "Want one Podbag Desktop product, found $($products.Count): $products" }
    $code = Get-MsiProperty $msi ProductCode
    if ($products[0] -ne $code) { throw "The installed product is $($products[0]), not $code of the new .msi" }
    if ($Previous -and $products[0] -eq $old[0]) { throw 'The previous product is still installed' }
    $want = Get-MsiProperty $msi ProductVersion
    $have = Get-ProductInfo $code VersionString
    if ($have -ne $want) { throw "Windows Installer has version $have, want $want" }
    $location = (Get-ProductInfo $code InstallLocation).TrimEnd('\')
    if ($location -ne $installDir) { throw "The product is installed in '$location', want '$installDir'" }
    $entries = @(Get-PodbagUninstallEntries)
    if ($entries.Count -ne 1) { throw "Want one Podbag Desktop in Installed apps, found $($entries.Count)" }

    Assert-PeMachine $gui $Arch
    Assert-PeMachine $engine $Arch
    if (Test-Path $App) {
        $built = (Get-FileHash $App).Hash
        if ((Get-FileHash $gui).Hash -ne $built) { throw "$gui is not the app of the new .msi" }
    }
    $info = (Get-Item $gui).VersionInfo
    if ($info.ProductName -ne 'Podbag Desktop' -or $info.CompanyName -ne 'SteelCrab') {
        throw "Unexpected version information: '$($info.ProductName)' by '$($info.CompanyName)'"
    }
    foreach ($link in $startMenu, $desktopLink) {
        $target = Get-ShortcutTarget $link
        if ($target -ne $gui) { throw "$link opens '$target', want '$gui'" }
    }
    & $engine --version
    if ($LASTEXITCODE -ne 0) { throw 'The installed engine failed' }

    # Saved answers: no first-launch question covers the window, and English text.
    $config = Join-Path $env:APPDATA 'io.github.steelcrab.podbag'
    New-Item -ItemType Directory -Force $config | Out-Null
    Set-Content -Encoding ascii (Join-Path $config 'settings.json') `
        '{"language":"en","cliNoticeDismissed":true,"compressionPromptSeen":true}'
    # A closed local address: the Training view does not sync the live catalog.
    $env:PODBAG_TRAINING_CATALOG = 'http://127.0.0.1:9/training/catalog.json'
    $stderr = Join-Path $env:TEMP "podbag-window-$Arch.log"
    $app = Start-Process $gui -PassThru -RedirectStandardError $stderr
    try {
        powershell.exe -NoProfile -File "$PSScriptRoot/test-window.ps1" -AppPid $app.Id -OutputFile $Screenshot
        if ($LASTEXITCODE -ne 0) {
            Get-Content $stderr -ErrorAction SilentlyContinue
            $app.Refresh()
            if ($app.HasExited) { throw "Podbag Desktop exited with code $($app.ExitCode)" }
            throw "Could not verify the installed window (title: '$($app.MainWindowTitle)')"
        }
        # Closing the window may keep the app in the notification area.
        if (!$app.WaitForExit(10000)) { Write-Output 'The app stayed open after its window closed.' }
    } finally {
        Stop-Installed
    }
} finally {
    Stop-Installed
    Invoke-Msi x $msi uninstall
}
if ((Test-Path $engine) -or (Test-Path $gui)) { throw 'Uninstall left application binaries behind' }
if (Test-Path $startMenu) { throw 'Uninstall left the Start menu shortcut behind' }
if (@(Get-RelatedProducts).Count -ne 0) { throw 'Uninstall left a Podbag Desktop product behind' }
$mode = if ($Previous) { 'upgrade from the previous .msi' } else { 'install' }
Write-Output "Verified $mode, launch, and uninstall on Windows $Arch."
