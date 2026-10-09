$ErrorActionPreference = 'Stop'
$version = '1.0.0'
$packageArgs = @{
  packageName    = 'hexdb'
  unzipLocation  = (Split-Path -Parent $MyInvocation.MyCommand.Definition)
  url64bit       = "https://github.com/dreaminhex/hexdb/releases/download/v$version/hexdb-v$version-x86_64-pc-windows-msvc.zip"
  checksum64     = 'REPLACE_WITH_SHA256_FROM_SHA256SUMS'
  checksumType64 = 'sha256'
}
Install-ChocolateyZipPackage @packageArgs
# Chocolatey shims the .exe files it finds, so hexdb and hexdb_api are on PATH.
Write-Host "HexDB installed. Create a config with a storage key (hexdb secret), then run: hexdb_api --config <path>\hexdb.toml"
