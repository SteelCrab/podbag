# Build the Podbag Desktop installer with a pinned WiX, from the workspace's release
# binaries, and check the identity that upgrades earlier installs:
#   desktop/windows/build-msi.ps1 -Arch x86_64|aarch64 [-App EXE] [-Engine EXE] [-Out MSI]
# It needs the .NET SDK (for the wix tool) and cargo.
param(
    [Parameter(Mandatory)][ValidateSet('x86_64', 'aarch64')][string]$Arch,
    [string]$App = 'target/release/podbag-desktop.exe',
    [string]$Engine = 'target/windows-engine/podbag.exe',
    [string]$Out = "target/msi/podbag-desktop-$Arch-windows.msi"
)
$ErrorActionPreference = 'Stop'
$wixVersion = '6.0.2'
. "$PSScriptRoot/msi.ps1"
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
Push-Location $root
try {
    Assert-PeMachine $App $Arch
    Assert-PeMachine $Engine $Arch
    $metadata = cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
    $version = ($metadata.packages | Where-Object name -eq 'podbag-desktop').version

    $tools = Join-Path $root 'target/wix'
    $wix = Join-Path $tools 'wix.exe'
    if (!(Test-Path $wix)) {
        dotnet tool install wix --version $wixVersion --tool-path $tools
        if ($LASTEXITCODE -ne 0) { throw 'Cannot install WiX' }
    }
    $have = (& $wix --version) -replace '\+.*$', ''
    if ($have -ne $wixVersion) { throw "Want WiX $wixVersion, found $have in $tools" }
    $ui = "WixToolset.UI.wixext/$wixVersion"
    & $wix extension add -g $ui
    if ($LASTEXITCODE -ne 0) { throw "Cannot add $ui" }

    New-Item -ItemType Directory -Force (Split-Path $Out) | Out-Null
    $platform = @{ x86_64 = 'x64'; aarch64 = 'arm64' }[$Arch]
    & $wix build (Join-Path $PSScriptRoot 'podbag-desktop.wxs') `
        -arch $platform -ext $ui `
        -d "Version=$version" `
        -d "App=$((Resolve-Path $App).Path)" `
        -d "Engine=$((Resolve-Path $Engine).Path)" `
        -d "Icon=$(Join-Path $root 'desktop/icons/icon.ico')" `
        -o $Out
    if ($LASTEXITCODE -ne 0) { throw 'wix build failed' }

    # The values that make Windows Installer replace an earlier install in place.
    $want = [ordered]@{
        UpgradeCode    = $PodbagUpgradeCode
        ProductName    = 'Podbag Desktop'
        Manufacturer   = 'SteelCrab'
        ProductVersion = $version
        ALLUSERS       = '1'
    }
    foreach ($name in $want.Keys) {
        $value = Get-MsiProperty $Out $name
        if ($value -ne $want[$name]) { throw "$name is '$value', want '$($want[$name])'" }
    }
    $template = Get-MsiTemplate $Out
    $wantTemplate = @{ x86_64 = 'x64;1033'; aarch64 = 'Arm64;1033' }[$Arch]
    if ($template -ne $wantTemplate) { throw "The platform is '$template', want '$wantTemplate'" }
    $dir = Get-MsiRows $Out "SELECT ``Directory_Parent``, ``DefaultDir`` FROM ``Directory`` WHERE ``Directory`` = 'INSTALLDIR'"
    if ($dir -notmatch "^ProgramFiles64Folder`t(.*\|)?Podbag Desktop$") { throw "INSTALLDIR is '$dir'" }
    $files = @(Get-MsiRows $Out "SELECT ``FileName``, ``Component_`` FROM ``File``" |
        ForEach-Object { ($_ -split "`t")[0] -replace '^.*\|', '' } | Sort-Object)
    if (($files -join ',') -ne 'podbag-desktop.exe,podbag.exe') { throw "The files are $($files -join ', ')" }
    Write-Output "Built $Out ($version, $template)"
} finally {
    Pop-Location
}
