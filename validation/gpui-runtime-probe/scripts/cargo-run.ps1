$ErrorActionPreference = "Stop"
& (Join-Path $PSScriptRoot "../../../scripts/cargo-run.ps1") @args
exit $LASTEXITCODE
