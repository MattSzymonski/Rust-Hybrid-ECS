<#
.SYNOPSIS
    Collect a managed EventPipe trace from a running Pill host.

.DESCRIPTION
    Captures GC, JIT, thread-pool, and allocation events without requiring the
    native debugger. The host can continue running while dotnet-trace records
    the session.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [int] $ProcessId,
    [string] $Output = "csharp-trace-$ProcessId.nettrace",
    [int] $DurationSeconds = 30
)

if (-not (Get-Command dotnet-trace -ErrorAction SilentlyContinue)) {
    throw "dotnet-trace was not found. Install it with: dotnet tool install --global dotnet-trace"
}
Get-Process -Id $ProcessId -ErrorAction Stop | Out-Null
dotnet-trace collect --process-id $ProcessId --duration ([TimeSpan]::FromSeconds($DurationSeconds)) `
    --output $Output --format nettrace
if ($LASTEXITCODE -ne 0) {
    throw "dotnet-trace failed with exit code $LASTEXITCODE"
}
Write-Host "Managed trace written to $((Resolve-Path -LiteralPath $Output).Path)"
