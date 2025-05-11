param (
    [string]$Tessellation = "articles",
    [string]$Id
)

if (-not $Id) {
    Write-Host "❌ You must supply a document ID using -Id."
    exit 1
}

$uri = "http://127.0.0.1:7700/$Tessellation/$Id"

try {
    $response = Invoke-RestMethod -Uri $uri -Method Get
    Write-Host "✅ Document found:" -ForegroundColor Green
    $response | ConvertTo-Json -Depth 5
} catch {
    Write-Host "❌ Document not found or server error." -ForegroundColor Yellow
}
