# Cargo executes member tests in their package directory; preserve the native argv path.
& (Join-Path $PSScriptRoot "../../../scripts/cargo-run.ps1") @args
exit $LASTEXITCODE
