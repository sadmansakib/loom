param(
    [string]$ComputerName = "localhost"
)

$row = Get-Content -Raw config.json | ConvertFrom-Json
Write-Host "Table: $row.Name"
Write-Host "Host: $ComputerName"
