# Historical end-to-end regression: build the explicit legacy PoC and keep its frame/shutdown assertions.
param([ValidateRange(1, 7200)][int]$TimeoutSeconds = 900)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
$root = Split-Path -Parent $PSScriptRoot
$oldSmoke = $env:POC_EXIT_AFTER_MS
Push-Location $root
try {
    New-Item -ItemType Directory -Force -Path "target" | Out-Null
    & rustc --edition 2024 scripts/test-cargo-runner.rs -o target/cargo-runner-probe.exe
    if ($LASTEXITCODE -ne 0) { throw "Cargo runner probe compilation failed" }
    & .\target\cargo-runner-probe.exe
    if ($LASTEXITCODE -ne 0) { throw "Cargo runner argument/exit-code check failed" }
    $stdout = Join-Path $root "target\cargo-run.stdout.log"
    $stderr = Join-Path $root "target\cargo-run.stderr.log"
    $env:POC_EXIT_AFTER_MS = "5000"
    $process = Start-Process -FilePath (Get-Command cargo).Source -ArgumentList @('run','--locked','--no-default-features','--features','legacy-demo','--bin','gpui-cef-poc') `
        -WorkingDirectory $root -RedirectStandardOutput $stdout -RedirectStandardError $stderr -PassThru
    try {
        # Wait for Cargo itself, not long-lived compiler helpers in its process tree.
        $null = $process.Handle
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $process.Kill()
            $process.WaitForExit()
            throw "cargo run timed out after $TimeoutSeconds seconds; see $stdout and $stderr"
        }
        $exitCode = $process.ExitCode
    } finally {
        $process.Dispose()
    }
    Get-Content -LiteralPath $stdout
    Get-Content -LiteralPath $stderr
    if ($exitCode -ne 0) { throw "cargo run failed with exit code $exitCode" }
    $output = (Get-Content -Raw -LiteralPath $stdout) + (Get-Content -Raw -LiteralPath $stderr)
    if ($output -notmatch 'First (?:accelerated )?CEF frame: \d+x\d+ ') { throw "No CEF frame received" }
    if ($output -notmatch 'CEF browser closed; shutdown complete') { throw "CEF did not shut down normally" }
    Write-Host "Cargo startup regression passed."
} finally {
    $env:POC_EXIT_AFTER_MS = $oldSmoke
    Pop-Location
}
