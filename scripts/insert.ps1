param (
    [int]$Count = 1,
    [string]$Endpoint = "http://127.0.0.1:7700/articles"
)

function Get-RandomTitle {
    $adjectives = @("Amazing", "Blazing", "Quantum", "Smart", "Dark", "Spicy", "Silent", "Naked", "Rusty")
    $nouns = @("Hex", "Engine", "Protocol", "Shard", "Tessellation", "Vertex", "Cluster", "Log")

    return "$($adjectives | Get-Random) $($nouns | Get-Random)"
}

for ($i = 1; $i -le $Count; $i++) {
    $doc = @{
        title     = Get-RandomTitle
        views     = Get-Random -Minimum 10 -Maximum 1000
        published = $true
        tags      = @("hexdb", "rust", "ai")
    } | ConvertTo-Json -Depth 3

    Write-Host "[$i/$Count] Inserting document: $($doc -replace '\s+', ' ')"

    Invoke-RestMethod -Uri $Endpoint `
                      -Method Post `
                      -Headers @{ "Content-Type" = "application/json" } `
                      -Body $doc
}
