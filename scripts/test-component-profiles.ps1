[CmdletBinding()]
param(
    [ValidateSet('Integrated', 'Threaded', 'All')][string]$Mode = 'All',
    [ValidateSet('Software', 'D3d11', 'All')][string]$Backend = 'Software',
    [string]$CefPath,
    [string]$TargetDir,
    [ValidateRange(1, 900)][int]$TimeoutSeconds = 120
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'common.ps1')
. (Join-Path $PSScriptRoot 'component-process-observer.ps1')
. (Join-Path $PSScriptRoot 'component-probe-launcher.ps1')

function Write-ProfileJson($Value, [string]$Path) {
    [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 16), (New-Object Text.UTF8Encoding($false)))
}
function Get-ProfileArtifact($Messages, [string]$Name) {
    $artifacts = @($Messages | Where-Object { $_.reason -eq 'compiler-artifact' -and $_.target.name -eq $Name -and $null -ne $_.executable })
    if ($artifacts.Count -ne 1) { throw "Expected one Cargo executable artifact for $Name; got $($artifacts.Count)" }
    [IO.Path]::GetFullPath($artifacts[0].executable)
}

$run = Resolve-ValidationDir 'profiles' ([guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $run | Out-Null
$summary = @{ schema = 1; stage = 'Profiles'; cases = @(); error = $null; success = $false }
$oldTarget = $env:CARGO_TARGET_DIR
$oldCef = $env:CEF_PATH
try {
    $cef = Resolve-CefPath $CefPath
    $env:CEF_PATH = $cef
    $env:CARGO_TARGET_DIR = Resolve-TargetDir $TargetDir
    Push-Location $PocRoot
    try {
        $ErrorActionPreference = 'Continue'
        $build = @(& cargo build --locked --target $PocTarget -p rust-cef-runtime -p rust-cef-helper --features rust-cef-runtime/test-support --bin profile-probe --bin profile-restart-probe --bin rust-cef-helper --message-format=json-render-diagnostics 2>&1 | Tee-Object -FilePath (Join-Path $run 'build.log') | ForEach-Object { Write-Host $_; $_ })
        $buildExit = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        if ($buildExit -ne 0) { throw "Cargo build failed: $buildExit" }
    } finally { $ErrorActionPreference = 'Stop'; Pop-Location }
    $messages = @()
    foreach ($line in $build) { if ($line -is [string] -and $line.StartsWith('{')) { $messages += ($line | ConvertFrom-Json) } }
    $probeArtifact = Get-ProfileArtifact $messages 'profile-probe'
    $helperArtifact = Get-ProfileArtifact $messages 'rust-cef-helper'
    $bundle = Join-Path $run ([string][char]0x53d1 + [char]0x884c + ' Profiles bundle with spaces')
    & (Join-Path $PSScriptRoot 'package-component.ps1') -HostExecutable $probeArtifact -HelperExecutable $helperArtifact -CefPath $cef -OutputDirectory $bundle -HostBuildVersion 'profile-probe-test-support'
    $probeFile = Join-Path $bundle ([IO.Path]::GetFileName($probeArtifact))
    $helperFile = Join-Path $bundle ([IO.Path]::GetFileName($helperArtifact))
    Write-ProfileJson @{ host_artifact = $probeArtifact; helper_artifact = $helperArtifact; host_sha256 = (Get-FileHash -LiteralPath $probeFile).Hash; helper_sha256 = (Get-FileHash -LiteralPath $helperFile).Hash } (Join-Path $run 'artifact-identity.json')
    $modes = if ($Mode -eq 'All') { @('Integrated', 'Threaded') } else { @($Mode) }
    $backends = if ($Backend -eq 'All') { @('Software', 'D3d11') } else { @($Backend) }
    $cells = @(foreach ($selectedMode in $modes) {
        foreach ($selectedBackend in $backends) { @{ mode = $selectedMode; backend = $selectedBackend } }
    })
    $summary.plan = $cells
    foreach ($cell in $cells) {
        $selectedMode = $cell.mode
        $selectedBackend = $cell.backend
        $case = Join-Path $run "$selectedMode-$selectedBackend"
        $cwd = Join-Path $case 'unrelated cwd'
        New-Item -ItemType Directory -Path $cwd -Force | Out-Null
        $reportPath = Join-Path $case 'probe.json'
        $gate = Join-Path $case 'observer.release'
        $launch = Invoke-Probe $probeFile @('--mode', $selectedMode, '--backend', $selectedBackend, '--report', $reportPath, '--helper', $helperFile, '--observer-gate', $gate, '--scenario', 'Smoke') $cwd $gate $helperFile
        Write-ProfileJson @{ children = $launch.children; image_verified = $launch.image_verified; observer_error = $launch.observer_error; remaining_processes = $launch.remaining_processes; observation_samples = $launch.observation_samples; residual_observations = $launch.residual_observations } (Join-Path $case 'child-images.json')
        [IO.File]::WriteAllText((Join-Path $case 'stdout.log'), $launch.stdout)
        [IO.File]::WriteAllText((Join-Path $case 'stderr.log'), $launch.stderr)
        $observed = $null
        $passed = $false
        if (Test-Path -LiteralPath $reportPath) {
            $observed = Get-Content -LiteralPath $reportPath -Raw -Encoding UTF8 | ConvertFrom-Json
            $threadRule = ($selectedMode -eq 'Integrated' -and $observed.cef_ui_thread_id -eq $observed.main_thread_id) -or
                ($selectedMode -eq 'Threaded' -and $observed.cef_ui_thread_id -ne $observed.main_thread_id -and $observed.manual_pump_count -eq 0)
            $storage = @($observed.storage_checks)
            $identity = @($observed.identity_checks)
            $passed = $launch.exit_code -eq 0 -and -not $launch.timed_out -and $launch.streams_complete -and $launch.image_verified -and
                $null -eq $launch.observer_error -and $launch.remaining_processes.Count -eq 0 -and
                $observed.schema -eq 1 -and $observed.kind -eq 'profiles' -and $observed.success -eq $true -and
                $observed.pid -eq $launch.pid -and $observed.exit_code -eq 0 -and $null -eq $observed.error -and
                $observed.requested_mode -eq $selectedMode -and $observed.actual_mode -eq $selectedMode -and $observed.requested_backend -eq $selectedBackend -and $observed.actual_backend -eq $selectedBackend -and
                $observed.main_thread_id -gt 0 -and $observed.cef_ui_thread_id -gt 0 -and $threadRule -and
                $observed.init_thread_id -eq $observed.main_thread_id -and $observed.shutdown_thread_id -eq $observed.main_thread_id -and
                $observed.created -ge 6 -and $observed.closed -eq $observed.created -and
                $observed.tasks_posted -gt 0 -and $observed.pending_tasks -eq 0 -and ($observed.tasks_completed + $observed.task_post_failures) -eq $observed.tasks_posted -and
                $observed.cef_shutdown_calls -eq 1 -and $observed.pending_requests -eq 0 -and $observed.native_calls_after_shutdown -eq 0 -and $observed.app_reference_retained -eq $false -and
                $storage.Count -ge 12 -and @($storage | Where-Object { $_.passed -ne $true }).Count -eq 0 -and
                $identity.Count -ge 5 -and @($identity | Where-Object { $_.passed -ne $true }).Count -eq 0
        }
        $summary.cases += @{ kind = 'profiles'; mode = $selectedMode; backend = $selectedBackend; pid = $launch.pid; exit_code = $launch.exit_code; timed_out = $launch.timed_out; passed = $passed; renderer_image_verified = $launch.image_verified; report = $reportPath; observed = $observed }
        Write-Host "profiles/$selectedMode/$selectedBackend pid=$($launch.pid) exit=$($launch.exit_code) passed=$passed"
    }
    . (Join-Path $PSScriptRoot 'component-profile-restart.ps1')
    $restart = Invoke-ProfileRestartCases -RunDirectory $run -Cells $cells -Messages $messages -HelperArtifact $helperArtifact -Cef $cef -TimeoutSeconds $TimeoutSeconds
    $summary.cases += @($restart.cases)
    $summary.fixture = $restart.fixture
    if ($null -ne $restart.error) { throw $restart.error }
    # One sharing case plus the four distinct restart phases per cell.
    $summary.success = $cells.Count -gt 0 -and $summary.cases.Count -eq ($cells.Count * 5) -and
        @($summary.cases | Where-Object { -not $_.passed }).Count -eq 0
    foreach ($cell in $cells) {
        $rows = @($summary.cases | Where-Object { $_.mode -eq $cell.mode -and $_.backend -eq $cell.backend })
        if ($rows.Count -ne 5 -or @($rows | Where-Object { $_.kind -eq 'profiles' }).Count -ne 1) {
            $summary.success = $false
            continue
        }
        foreach ($storage in @('Memory', 'Persistent')) {
            foreach ($phase in @('Write', 'Read')) {
                if (@($rows | Where-Object { $_.kind -eq 'profile-restart' -and $_.storage -eq $storage -and $_.phase -eq $phase }).Count -ne 1) {
                    $summary.success = $false
                }
            }
        }
    }
} catch { $summary.error = $_.Exception.Message; Write-Host "Profile validation failed: $($summary.error)" }
finally {
    $env:CARGO_TARGET_DIR = $oldTarget
    $env:CEF_PATH = $oldCef
    Write-ProfileJson $summary (Join-Path $run 'result.json')
    Write-Host "Evidence: $run"
}
if (-not $summary.success) { exit 1 }
