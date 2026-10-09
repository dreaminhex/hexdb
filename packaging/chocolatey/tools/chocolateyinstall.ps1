$ErrorActionPreference = 'Stop'
# The release workflow sets the version in this URL and the checksum (from the
# release's SHA256SUMS) before packing.
$packageArgs = @{
  packageName    = $env:ChocolateyPackageName
  fileType       = 'msi'
  url64bit       = 'https://github.com/dreaminhex/hexdb/releases/download/v1.0.0/hexdb-windows-x64.msi'
  checksum64     = 'REPLACE_WITH_SHA256_OF_THE_MSI'
  checksumType64 = 'sha256'
  silentArgs     = "/qn /norestart /l*v `"$($env:TEMP)\$($env:ChocolateyPackageName).$($env:ChocolateyPackageVersion).MsiInstall.log`""
  validExitCodes = @(0, 3010, 1641)
}
Install-ChocolateyPackage @packageArgs
Write-Host "HexDB is installed. Open a new terminal and run: hexdb start"
