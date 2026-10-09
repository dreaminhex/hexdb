<#
.SYNOPSIS
Builds the HexDB Windows installer (an .msi) from a release archive's folder.

.DESCRIPTION
Needs the WiX Toolset 5 .NET tool and its UI extension, once:

    dotnet tool install --global wix --version 5.0.2
    wix extension add -g WixToolset.UI.wixext/5.0.2

-StageDir is an unpacked release archive (or any folder with the same layout):
hexdb.exe, hexdb_api.exe, ui\, odbc\hexdb_odbc.dll, README.md, MANUAL.md and
LICENSE. The release workflow passes the Windows archive it just built.

.EXAMPLE
# From a local build, after building the admin UI and the release binaries:
.\packaging\windows\build-msi.ps1 -Version 1.0.0 -StageDir .\stage -Out .\hexdb-windows-x64.msi
#>
param(
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][string]$StageDir,
    [string]$Out = "hexdb-windows-x64.msi"
)

$ErrorActionPreference = "Stop"
$here = $PSScriptRoot
# Windows Installer versions are numeric: "v1.2.3" and "1.2.3-rc.1" both become 1.2.3.
$numeric = ($Version.TrimStart("v") -split "[-+]")[0]
$stage = (Resolve-Path $StageDir).Path
$Out = [IO.Path]::GetFullPath($(if ([IO.Path]::IsPathRooted($Out)) { $Out } else { Join-Path (Get-Location) $Out }))

foreach ($f in "hexdb.exe", "hexdb_api.exe", "ui\index.html", "odbc\hexdb_odbc.dll", "README.md", "MANUAL.md", "LICENSE") {
    if (-not (Test-Path (Join-Path $stage $f))) { throw "Missing $f in $stage" }
}

Push-Location $here
try {
    wix build hexdb.wxs -arch x64 -ext WixToolset.UI.wixext -d "Version=$numeric" -d "StageDir=$stage" -o $Out
    if ($LASTEXITCODE -ne 0) { throw "wix build failed ($LASTEXITCODE)" }
} finally {
    Pop-Location
}
Write-Host "Built $Out (HexDB $numeric)"
