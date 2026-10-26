$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$targetExe = Join-Path $projectRoot 'target/release/kimi-switch.exe'

function Assert-TargetAvailable {
    $running = @(Get-CimInstance Win32_Process -Filter "Name = 'kimi-switch.exe'" |
        Where-Object { $_.ExecutablePath -and $_.ExecutablePath -eq $targetExe })
    if ($running.Count -gt 0) {
        throw "Close the running kimi-switch.exe (PID $($running[0].ProcessId)) before building: $targetExe"
    }
}

Push-Location $projectRoot
try {
    Assert-TargetAvailable
    npm run check:ui
    if ($LASTEXITCODE -ne 0) { throw "UI checks failed (exit $LASTEXITCODE)" }
    Assert-TargetAvailable
    cargo build --release -p kimi-switch
    if ($LASTEXITCODE -ne 0) { throw "GUI build failed (exit $LASTEXITCODE)" }
    cargo build --release -p kimi-switch-cli
    if ($LASTEXITCODE -ne 0) { throw "CLI build failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}
