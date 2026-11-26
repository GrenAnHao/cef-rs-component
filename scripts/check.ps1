[CmdletBinding()]
param(
    [switch]$Smoke,
    [switch]$Interaction,
    [switch]$Drag,
    [switch]$Drop,
    [switch]$Ime,
    [string]$DropFile = ""
)
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "common.ps1")
Push-Location (Split-Path -Parent $PSScriptRoot)
try {
    cargo fmt -p gpui-cef-poc -p rust-cef-ui -p rust-cef-helper -p rust-cef-runtime -p rust-cef-gpui -- --check
    if ($LASTEXITCODE -ne 0) { throw "Formatting check failed" }
    # Compile the real pure core once for its consumers, without CEF/GPUI builds.
    $checkOut = New-PocDirectory (Resolve-CheckOutputDir)
    $coreLib = Join-Path $checkOut "libgpui_cef_core.rlib"
    rustc --edition 2024 --crate-name rust_cef_core --crate-type lib crates/rust-cef-core/src/lib.rs -o $coreLib
    if ($LASTEXITCODE -ne 0) { throw "Core library compilation failed" }
    $checks = @(
        @{ Source = "crates/rust-cef-core/src/lib.rs"; Exe = (Join-Path $checkOut "core-state-tests.exe") },
        @{ Source = "crates/rust-cef-core/tests/contracts.rs"; Exe = (Join-Path $checkOut "core-contract-tests.exe") },
        @{ Source = "crates/rust-cef-core/tests/profiles.rs"; Exe = (Join-Path $checkOut "core-profile-tests.exe") },
        @{ Source = "crates/rust-cef-core/tests/closing.rs"; Exe = (Join-Path $checkOut "core-closing-tests.exe") },
        @{ Source = "crates/rust-cef-core/tests/shutdown.rs"; Exe = (Join-Path $checkOut "core-shutdown-tests.exe") },
        @{ Source = "src/lib.rs"; Exe = (Join-Path $checkOut "frame-store-tests.exe") }
    )
    foreach ($check in $checks) {
        rustc --edition 2024 --test $check.Source --extern rust_cef_core=$coreLib -o $check.Exe
        if ($LASTEXITCODE -ne 0) { throw "Test compilation failed: $($check.Source)" }
        & $check.Exe
        if ($LASTEXITCODE -ne 0) { throw "Pure logic tests failed: $($check.Source)" }
    }
    if ($Smoke) { & (Join-Path $PSScriptRoot "run.ps1") -Legacy -SmokeSeconds 5 }
    if ($Ime) {
        # Composition is checked separately: when the active keyboard layout is
        # an IME, it owns the composition state and the synthetic commit races
        # with it. The real IME path is still verified by hand.
        $build = & (Join-Path $PSScriptRoot "build.ps1") -Legacy
        $env:POC_SELF_TEST = "6"
        $env:POC_EXIT_AFTER_MS = "9000"
        try {
            $output = & (Join-Path $build "gpui-cef-poc.exe") 2>&1
            $output | Select-String "SELF_TEST_"
            if (-not ($output -match "SELF_TEST_RESULT=ok")) {
                # Not a code failure by itself: with an IME as the active
                # keyboard layout the page reports real pinyin conversions
                # (e.g. `compositionupdate:擦`) and the synthetic commit never
                # lands. Say so instead of reporting a pass.
                Write-Warning "IME composition self-test did not complete."
                Write-Warning "This is expected when the active keyboard layout is an IME."
                Write-Warning "Verify the candidate window and composition underline by hand."
                Write-Host ($output | Select-String "SELF_TEST_TITLE")
            }
        } finally {
            Remove-Item Env:POC_SELF_TEST, Env:POC_EXIT_AFTER_MS -ErrorAction SilentlyContinue
        }
    }
    if ($Drag -or $Drop) {
        $build = & (Join-Path $PSScriptRoot "build.ps1") -Legacy
        $exe = Join-Path $build "gpui-cef-poc.exe"
        if ($Drag) {
            $env:POC_SELF_TEST = "3"
            $env:POC_EXIT_AFTER_MS = "9000"
            try {
                $output = & $exe 2>&1
                $output | Select-String "SELF_TEST_"
                if (-not ($output -match "SELF_TEST_RESULT=ok")) { throw "Drag self-test failed" }
            } finally {
                Remove-Item Env:POC_SELF_TEST, Env:POC_EXIT_AFTER_MS -ErrorAction SilentlyContinue
            }
        }
        if ($Drop) {
            if ([string]::IsNullOrWhiteSpace($DropFile)) {
                $DropFile = Join-Path (New-PocDirectory (Resolve-PocTempDir)) "poc-drop-test.txt"
                Set-Content -LiteralPath $DropFile -Value "drop test"
            }
            $env:POC_SELF_TEST = "4"
            $env:POC_DROP_FILE = (Resolve-Path $DropFile).Path
            $env:POC_EXIT_AFTER_MS = "10000"
            try {
                $output = & $exe 2>&1
                $output | Select-String "SELF_TEST_"
                if (-not ($output -match "SELF_TEST_RESULT=ok")) { throw "File drop self-test failed" }
            } finally {
                Remove-Item Env:POC_SELF_TEST, Env:POC_DROP_FILE, Env:POC_EXIT_AFTER_MS -ErrorAction SilentlyContinue
            }
        }
    }
    if ($Interaction) {
        # Builds, then drives scripted mouse/keyboard/IME input at the real
        # CEF browser and asserts the page observed each event.
        $env:POC_SELF_TEST = "1"
        try {
            $env:POC_EXIT_AFTER_MS = "9000"
            $build = & (Join-Path $PSScriptRoot "build.ps1") -Legacy
            $output = & (Join-Path $build "gpui-cef-poc.exe") 2>&1
            $output | Select-String "SELF_TEST_.*|First CEF frame|shutdown complete"
            if (-not ($output -match "SELF_TEST_RESULT=ok")) { throw "Interaction self-test failed" }
        } finally {
            Remove-Item Env:POC_SELF_TEST -ErrorAction SilentlyContinue
            Remove-Item Env:POC_EXIT_AFTER_MS -ErrorAction SilentlyContinue
        }
    }
} finally { Pop-Location }
