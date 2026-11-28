# Helpers for build-msi.ps1 and test-installed.ps1: Windows Installer through its COM
# API, and PE checks. Dot-source it.

# The Podbag Desktop products of every version, 0.3.2 (Tauri) and later.
$PodbagUpgradeCode = '{43294F5F-5CA2-5C9B-AD98-D421422A0586}'

function Invoke-Com($Object, [string]$Name, [string]$Kind, [object[]]$Arguments = @()) {
    # The comma keeps a collection (such as the StringList of RelatedProducts) whole; without
    # it, PowerShell unrolls the result, and an empty list comes back as $null.
    return , ($Object.GetType().InvokeMember($Name, [Reflection.BindingFlags]$Kind, $null, $Object, $Arguments))
}

function New-Installer {
    New-Object -ComObject WindowsInstaller.Installer
}

# The rows of a query on an .msi, each a string with its columns joined by tabs.
function Get-MsiRows([string]$Msi, [string]$Sql) {
    $installer = New-Installer
    $db = Invoke-Com $installer OpenDatabase InvokeMethod @((Resolve-Path $Msi).Path, 0)
    $view = Invoke-Com $db OpenView InvokeMethod @($Sql)
    try {
        [void](Invoke-Com $view Execute InvokeMethod)
        while ($null -ne ($record = Invoke-Com $view Fetch InvokeMethod)) {
            $n = Invoke-Com $record FieldCount GetProperty
            # A for loop: ForEach-Object wraps $_ in a PSObject, which COM rejects.
            $fields = for ($i = 1; $i -le $n; $i++) { Invoke-Com $record StringData GetProperty @($i) }
            @($fields) -join "`t"
        }
    } finally {
        [void](Invoke-Com $view Close InvokeMethod)
        [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($view)
        [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($db)
    }
}

function Get-MsiProperty([string]$Msi, [string]$Name) {
    Get-MsiRows $Msi "SELECT ``Value`` FROM ``Property`` WHERE ``Property`` = '$Name'"
}

# The template of the summary information: "<platform>;<language>", such as x64;1033.
function Get-MsiTemplate([string]$Msi) {
    $installer = New-Installer
    $info = Invoke-Com $installer SummaryInformation GetProperty @((Resolve-Path $Msi).Path, 0)
    try { Invoke-Com $info Property GetProperty @(7) }
    finally { [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($info) }
}

# The product codes installed with the UpgradeCode.
function Get-RelatedProducts([string]$UpgradeCode = $PodbagUpgradeCode) {
    $list = Invoke-Com (New-Installer) RelatedProducts GetProperty @($UpgradeCode)
    $n = Invoke-Com $list Count GetProperty
    for ($i = 0; $i -lt $n; $i++) { Invoke-Com $list Item GetProperty @($i) }
}

function Get-ProductInfo([string]$ProductCode, [string]$Name) {
    Invoke-Com (New-Installer) ProductInfo GetProperty @($ProductCode, $Name)
}

# The Add or remove programs entries named Podbag Desktop.
function Get-PodbagUninstallEntries {
    $keys = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall',
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall',
        'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall'
    foreach ($key in $keys) {
        if (Test-Path $key) {
            Get-ChildItem $key | Get-ItemProperty | Where-Object { $_.DisplayName -eq 'Podbag Desktop' }
        }
    }
}

# Stop when a PE file is not for the CPU: x86_64 or aarch64.
function Assert-PeMachine([string]$Path, [string]$Arch) {
    $expected = @{ x86_64 = 0x8664; aarch64 = 0xaa64 }[$Arch]
    $stream = [IO.File]::OpenRead((Resolve-Path $Path).Path)
    try {
        $reader = [IO.BinaryReader]::new($stream)
        $stream.Position = 0x3c
        $stream.Position = $reader.ReadUInt32()
        if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne $expected) {
            throw "$Path is not a Windows $Arch program"
        }
    } finally {
        $stream.Dispose()
    }
}
