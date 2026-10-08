<#
.SYNOPSIS
Installs (or removes) the HexDB ODBC driver on Windows.

.DESCRIPTION
Copies hexdb_odbc.dll to "$env:ProgramFiles\HexDB\ODBC" and registers it with
the Windows ODBC driver manager as "HexDB" (64-bit). Registering a driver
writes to HKEY_LOCAL_MACHINE, so run this from an elevated PowerShell.

Optionally creates a DSN, so applications can pick "HexDB" from their list of
data sources. The DSN holds the server address; give credentials when you
connect (ApiKey=... in the connection string, or the user name and password
your application asks for). Pass -ApiKey only on machines where storing the
key in the registry is acceptable.

.EXAMPLE
.\install-windows.ps1 -Dll .\target\release\hexdb_odbc.dll
.\install-windows.ps1 -Dll .\hexdb_odbc.dll -Dsn HexDB -Server http://127.0.0.1:7700
.\install-windows.ps1 -Uninstall
#>
param(
    [string]$Dll = (Join-Path $PSScriptRoot "hexdb_odbc.dll"),
    [string]$Dsn,
    [string]$Server = "http://127.0.0.1:7700",
    [string]$ApiKey,
    [ValidateSet("User", "System")][string]$DsnScope = "User",
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"
$driverName = "HexDB"
$installDir = Join-Path $env:ProgramFiles "HexDB\ODBC"
$driverKey = "HKLM:\SOFTWARE\ODBC\ODBCINST.INI\$driverName"
$driversList = "HKLM:\SOFTWARE\ODBC\ODBCINST.INI\ODBC Drivers"

$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) {
    throw "Run this from an elevated PowerShell (Run as administrator): registering an ODBC driver writes to HKEY_LOCAL_MACHINE."
}
if (-not [Environment]::Is64BitProcess) {
    throw "Run this from 64-bit PowerShell; the driver is 64-bit."
}

function Remove-Dsn([string]$root, [string]$name) {
    $key = "$root\SOFTWARE\ODBC\ODBC.INI\$name"
    if (Test-Path $key) { Remove-Item $key -Recurse }
    $list = "$root\SOFTWARE\ODBC\ODBC.INI\ODBC Data Sources"
    if ((Test-Path $list) -and (Get-ItemProperty $list -Name $name -ErrorAction SilentlyContinue)) {
        Remove-ItemProperty $list -Name $name
    }
}

if ($Uninstall) {
    foreach ($root in "HKCU:", "HKLM:") {
        $list = "$root\SOFTWARE\ODBC\ODBC.INI\ODBC Data Sources"
        if (Test-Path $list) {
            (Get-ItemProperty $list).PSObject.Properties |
                Where-Object { $_.Value -eq $driverName } |
                ForEach-Object { Remove-Dsn $root $_.Name; Write-Host "Removed DSN $($_.Name) ($root)" }
        }
    }
    if (Test-Path $driverKey) { Remove-Item $driverKey -Recurse }
    if (Get-ItemProperty $driversList -Name $driverName -ErrorAction SilentlyContinue) { Remove-ItemProperty $driversList -Name $driverName }
    if (Test-Path $installDir) { Remove-Item $installDir -Recurse -Force }
    Write-Host "Removed the $driverName ODBC driver."
    return
}

if (-not (Test-Path $Dll)) { throw "Driver library not found: $Dll (build it with: cargo build --release -p hexdb_odbc)" }
New-Item -ItemType Directory -Force $installDir | Out-Null
$target = Join-Path $installDir "hexdb_odbc.dll"
Copy-Item $Dll $target -Force

New-Item -Force $driverKey | Out-Null
Set-ItemProperty $driverKey -Name "Driver" -Value $target
Set-ItemProperty $driverKey -Name "APILevel" -Value "1"
Set-ItemProperty $driverKey -Name "ConnectFunctions" -Value "YYN"
Set-ItemProperty $driverKey -Name "DriverODBCVer" -Value "03.80"
Set-ItemProperty $driverKey -Name "FileUsage" -Value "0"
Set-ItemProperty $driverKey -Name "SQLLevel" -Value "1"
Set-ItemProperty $driverKey -Name "UsageCount" -Value 1 -Type DWord
if (-not (Test-Path $driversList)) { New-Item -Force $driversList | Out-Null }
Set-ItemProperty $driversList -Name $driverName -Value "Installed"
Write-Host "Installed the $driverName ODBC driver: $target"

if ($Dsn) {
    $root = if ($DsnScope -eq "System") { "HKLM:" } else { "HKCU:" }
    Remove-Dsn $root $Dsn
    $key = "$root\SOFTWARE\ODBC\ODBC.INI\$Dsn"
    New-Item -Force $key | Out-Null
    Set-ItemProperty $key -Name "Driver" -Value $target
    Set-ItemProperty $key -Name "Server" -Value $Server
    if ($ApiKey) { Set-ItemProperty $key -Name "ApiKey" -Value $ApiKey }
    $list = "$root\SOFTWARE\ODBC\ODBC.INI\ODBC Data Sources"
    if (-not (Test-Path $list)) { New-Item -Force $list | Out-Null }
    Set-ItemProperty $list -Name $Dsn -Value $driverName
    Write-Host "Created $DsnScope DSN '$Dsn' for $Server"
}

Write-Host ""
Write-Host "Connect with:  Driver={$driverName};Server=$Server;ApiKey=<your API key>"
