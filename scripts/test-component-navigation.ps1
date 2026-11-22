# Stage driver for the 8.2 navigation probe. Builds only the probe and the real
# CEF helper, stages the actual artifacts into a Chinese/space bundle, and runs
# one new process per mode. This is not a workspace or --bins build.
[CmdletBinding()]
param(
    [ValidateSet('Integrated', 'Threaded', 'All')][string]$Mode = 'All',
    [ValidateSet('Software', 'D3d11', 'All')][string]$Backend = 'Software',
    [string]$CefPath,
    [string]$TargetDir,
    [switch]$RetiredLoadIsolation,
    [ValidateRange(1, 900)][int]$TimeoutSeconds = 120
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($RetiredLoadIsolation -and ($Mode -ne 'All' -or $Backend -ne 'Software')) { throw 'RetiredLoadIsolation requires Mode All and Software without backend fallback' }
. (Join-Path $PSScriptRoot 'common.ps1')
. (Join-Path $PSScriptRoot 'component-process-observer.ps1')
. (Join-Path $PSScriptRoot 'component-probe-launcher.ps1')

function Write-NavigationJson($Value, [string]$Path) {
    [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 16), (New-Object Text.UTF8Encoding($false)))
}
function Get-NavigationArtifact($Messages, [string]$Name) {
    $artifacts = @($Messages | Where-Object {
        $_.reason -eq 'compiler-artifact' -and $_.target.name -eq $Name -and $null -ne $_.executable
    })
    if ($artifacts.Count -ne 1) { throw "Expected one Cargo executable artifact for $Name; got $($artifacts.Count)" }
    [IO.Path]::GetFullPath($artifacts[0].executable)
}

# The probe's fixed evidence order. A different count, order or unknown id is
# rejected even when the candidate exited zero with success=true.
$requiredChecks = @(
    'runtime_ready',
    'browser_created',
    'first_frame',
    'initial_address',
    'initial_title',
    'initial_loading_false',
    'initial_history_no_back',
    'observer_released',
    'redirect_address',
    'redirect_title',
    'redirect_history',
    'redirect_http_requests',
    'redirect_event',
    'back_address',
    'back_can_forward',
    'forward_address',
    'forward_history',
    'reload_request_count',
    'reload_loading_cycle',
    'main_error',
    'main_error_runtime_alive',
    'child_main_clean',
    'child_subframe_error',
    'cancel_event',
    'cancel_no_load_failed',
    'cancel_runtime_alive',
    'recovery_address',
    'recovery_state'
)

function Test-NavigationReport($Observed, $Launch, [string]$SelectedMode) {
    if ($null -eq $Observed) { return $false }
    $threadRule = ($SelectedMode -eq 'Integrated' -and
            $Observed.cef_ui_thread_id -eq $Observed.main_thread_id -and
            $Observed.manual_pump_count -gt 0) -or
        ($SelectedMode -eq 'Threaded' -and
            $Observed.cef_ui_thread_id -ne $Observed.main_thread_id -and
            $Observed.manual_pump_count -eq 0)
    $checks = @($Observed.checks)
    $actualIds = @($checks | ForEach-Object { [string]$_.id })
    $checkSetExact = $actualIds.Count -eq $requiredChecks.Count
    if ($checkSetExact) {
        for ($index = 0; $index -lt $requiredChecks.Count; $index++) {
            if ($actualIds[$index] -ne $requiredChecks[$index]) {
                $checkSetExact = $false
                break
            }
        }
    }
    $allChecksPassed = @($checks | Where-Object { $_.passed -ne $true }).Count -eq 0
    $mainError = $Observed.main_error_code
    $subError = $Observed.sub_error_code
    return (
        $Observed.schema -eq 1 -and
        $Observed.kind -eq 'navigation' -and
        $Observed.success -eq $true -and
        $null -eq $Observed.error -and
        $Observed.exit_code -eq 0 -and
        $Observed.pid -eq $Launch.pid -and
        $Observed.requested_mode -eq $SelectedMode -and
        $Observed.actual_mode -eq $SelectedMode -and
        $Observed.requested_backend -eq 'Software' -and
        $Observed.actual_backend -eq 'Software' -and
        $Observed.main_thread_id -gt 0 -and
        $Observed.cef_ui_thread_id -gt 0 -and
        $threadRule -and
        $Observed.init_thread_id -eq $Observed.main_thread_id -and
        $Observed.shutdown_thread_id -eq $Observed.main_thread_id -and
        $Observed.created -eq 1 -and
        $Observed.closed -eq 1 -and
        $Observed.valid_frame_browsers -eq 1 -and
        $Observed.frames -gt 0 -and
        $Observed.first_frame_width -eq 320 -and
        $Observed.first_frame_height -eq 240 -and
        $Observed.tasks_posted -gt 0 -and
        $Observed.pending_tasks -eq 0 -and
        ($Observed.tasks_completed + $Observed.task_post_failures) -eq $Observed.tasks_posted -and
        $Observed.cef_shutdown_calls -eq 1 -and
        $Observed.native_calls -gt 0 -and
        $Observed.native_calls_after_shutdown -eq 0 -and
        $Observed.pending_requests -eq 0 -and
        $Observed.pending_profiles -eq 0 -and
        $Observed.pending_cookie_flushes -eq 0 -and
        $Observed.profile_references_retained -eq 0 -and
        $Observed.app_reference_retained -eq $false -and
        $Observed.root_lease_held -eq $false -and
        $Observed.runtime_generation -gt 0 -and
        $Observed.default_profile_id -eq 0 -and
        $Observed.default_profile_ready -eq $true -and
        $Observed.observer_released -eq $true -and
        $null -ne $mainError -and $mainError -ne 0 -and $mainError -ne -3 -and
        $null -ne $subError -and $subError -ne 0 -and $subError -ne -3 -and
        $Observed.cancel_code -eq -3 -and
        $checks.Count -eq $requiredChecks.Count -and
        $checkSetExact -and
        $allChecksPassed
    )
}

function Test-RetiredLoadIsolation($Probe, $Launch, [string]$SelectedMode) {
    if ($null -eq $Probe -or $Probe.kind -cne 'retired-load-isolation' -or $Probe.schema -ne 1 -or
        $Probe.success -isnot [bool] -or -not $Probe.success -or $null -ne $Probe.error -or $Probe.exit_code -ne 0 -or $Probe.pid -ne $Launch.pid -or
        $Probe.mode -cne $SelectedMode -or $Probe.backend -cne 'Software' -or
        $Probe.observer_released -isnot [bool] -or -not $Probe.observer_released) { return $false }
    $slow = $Probe.slow_cut; $closed = $Probe.original_closed; $next = $Probe.replacement_before_release; $write = $Probe.server_write
    $d = $Probe.audit; $moves = @($Probe.survivor_navigations); $events = @($Probe.events)
    $handle = $Probe.PSObject.Properties['retired_handle_refused']
    if ($null -eq $handle -or $null -eq $handle.Value) { return $false }
    $h = $handle.Value
    if ((@($Probe.original_pair[0]) -join ',') -cne '1,1' -or (@($Probe.original_pair[1]) -join ',') -cne '1,2' -or
        (@($next.browser) -join ',') -cne '1,3' -or $slow.requests -ne 1 -or $slow.writes -ne 0 -or $closed.writes -ne 0 -or $next.writes -ne 0 -or
        (@($slow.command[0..1]) -join ',') -cne '1,1' -or $slow.command[2] -le 0 -or $slow.loading_event -le 0 -or
        (@($closed.ticket[0..1]) -join ',') -cne '1,1' -or $closed.ticket[2] -le 0 -or $closed.sequence -le $slow.loading_event -or
        $closed.frame_source_closed -isnot [bool] -or -not $closed.frame_source_closed -or
        $closed.b_original_alive -isnot [bool] -or -not $closed.b_original_alive -or $next.created_event -le $closed.sequence -or
        ($h.browser -join ',') -cne '1,1' -or ($h.replacement -join ',') -cne '1,3' -or $h.navigate -cne 'Closed' -or $h.reload -cne 'Closed' -or
        $h.posted_before -le 0 -or $h.posted_before -ne $h.posted_after -or $h.posted_after -ge $d.tasks_posted -or $h.slow_requests -ne 1 -or
        $h.b_c_snapshots_unchanged -isnot [bool] -or -not $h.b_c_snapshots_unchanged -or
        $write.released -ne 1 -or $write.expired -ne 0 -or $write.error -ne 0 -or
        $write.socket_write_ok -notin @(0, 1) -or $write.peer_gone -notin @(0, 1) -or
        ($write.socket_write_ok + $write.peer_gone) -ne 1 -or $moves.Count -ne 2 -or $events.Count -gt 256) { return $false }

    # Check receipts against sink order, not just plausible sequence numbers.
    $position = @{}
    $created = @{}
    $retired = @{}
    $loading = @{}
    for ($i = 0; $i -lt $events.Count; $i++) {
        $e = $events[$i]
        $id = $e.browser -join ','
        if ($id -cnotin @('1,1', '1,2', '1,3') -or $e.sequence -le 0 -or
            $position.ContainsKey($e.sequence) -or $retired.ContainsKey($id) -or
            $e.name -cin @('slow-title', 'main-failed', 'capture-failed', 'create-failed') -or
            ($e.name -ceq 'slow-address' -and $id -cne '1,1') -or
            ($e.name -ceq 'main-cancelled' -and ($id -cne '1,1' -or $e.code -ne -3))) { return $false }
        $position[$e.sequence] = $i
        if ($e.name -ceq 'created') {
            if ($created.ContainsKey($id)) { return $false }
            $created[$id] = $e.sequence
        } elseif ($e.name -ceq 'closed') {
            if (-not $created.ContainsKey($id)) { return $false }
            $retired[$id] = $e.sequence
        } elseif ($e.name -ceq 'loading' -and $id -ceq '1,1') {
            $loading[$e.sequence] = $i
        }
    }
    if ($retired.Count -ne 3 -or -not $loading.ContainsKey($slow.loading_event) -or
        $retired['1,1'] -ne $closed.sequence -or $created['1,3'] -ne $next.created_event -or
        $position[$created['1,2']] -ge $loading[$slow.loading_event] -or
        $loading[$slow.loading_event] -ge $position[$closed.sequence] -or
        $position[$closed.sequence] -ge $position[$next.created_event]) { return $false }

    $priorEnd = $position[$next.created_event]
    for ($i = 0; $i -lt 2; $i++) {
        $m = $moves[$i]
        $id = "1,$($i + 2)"
        $title = if ($i -eq 0) { 'title-a' } else { 'title-b' }
        if (($m.command[0..1] -join ',') -cne $id -or $m.command[2] -le 0 -or
            $m.event_floor -le $priorEnd -or $m.event_floor -ge $events.Count -or
            $m.requests_before -lt 0 -or $m.requests_after -ne ($m.requests_before + 1) -or
            -not $position.ContainsKey($m.title_event) -or -not $position.ContainsKey($m.finish_event)) { return $false }
        $titleIndex = $position[$m.title_event]
        $finishIndex = $position[$m.finish_event]
        if ($titleIndex -lt $m.event_floor -or $finishIndex -lt $m.event_floor) { return $false }
        $titleEvent = $events[$titleIndex]
        $finishEvent = $events[$finishIndex]
        if (($titleEvent.browser -join ',') -cne $id -or $titleEvent.name -cne $title -or $null -ne $titleEvent.code -or
            ($finishEvent.browser -join ',') -cne $id -or $finishEvent.name -cne 'main-finished' -or $finishEvent.code -ne 200) { return $false }
        $priorEnd = [Math]::Max($titleIndex, $finishIndex)
    }
    $threads = if ($SelectedMode -ceq 'Integrated') { $d.ui -eq $d.main -and $d.pump -gt 0 } else { $d.ui -ne $d.main -and $d.pump -eq 0 }
    return $threads -and $d.ui -gt 0 -and $d.main -gt 0 -and $d.init -eq $d.main -and $d.shutdown_thread -eq $d.main -and
        $d.observed_backend -ceq 'Some(SoftwareRequired)' -and $d.finished -is [bool] -and $d.finished -and $d.shutdown -eq 1 -and $d.native_after -eq 0 -and
        $d.tasks_posted -gt 0 -and $d.tasks_completed -eq $d.tasks_posted -and $d.post_failed -eq 0 -and $d.pending_tasks -eq 0 -and
        $d.pending_requests -eq 0 -and $d.pending_profiles -eq 0 -and $d.pending_flush -eq 0 -and $d.profile_refs -eq 0 -and
        -not $d.app_retained -and -not $d.root_lease -and $d.frames_after_closed -eq 0 -and $d.ledger_overflow -eq 0 -and @($d.fixture_errors).Count -eq 0
}

$run = New-PocDirectory (Resolve-ValidationDir 'navigation' ([guid]::NewGuid().ToString('N')))
$summary = @{ schema = 1; stage = 'Navigation'; cases = @(); error = $null; success = $false }
if ($RetiredLoadIsolation) {
    $summary.validation_scope = 'late-loopback-response-after-retired-browser'
    $summary.expected_cases = 2
    $summary.task_13_2_accepted = $false
}
$oldTarget = $env:CARGO_TARGET_DIR
$oldCef = $env:CEF_PATH
try {
    # No silent software fallback and no empty accelerated matrix.
    if ($Backend -ne 'Software') {
        throw 'Navigation validation currently implements SoftwareRequired only; requested accelerated coverage is not implemented'
    }
    $modes = if ($Mode -eq 'All') { @('Integrated', 'Threaded') } else { @($Mode) }
    $modeCount = @($modes).Count
    if ($modeCount -lt 1) { throw 'At least one mode is required' }
    $cef = Resolve-CefPath $CefPath
    $env:CEF_PATH = $cef
    $env:CARGO_TARGET_DIR = Resolve-TargetDir $TargetDir
    Push-Location $PocRoot
    try {
        $ErrorActionPreference = 'Continue'
        # Only the navigation probe and the real helper. No workspace-wide or
        # --bins build; unrelated binaries are not rebuilt by this stage.
        $build = @(& cargo build --locked --target $PocTarget -p rust-cef-runtime -p rust-cef-helper --features rust-cef-runtime/test-support --bin navigation-probe --bin rust-cef-helper --message-format=json-render-diagnostics 2>&1 | Tee-Object -FilePath (Join-Path $run 'build.log') | ForEach-Object { Write-Host $_; $_ })
        $buildExit = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        if ($buildExit -ne 0) { throw "Cargo build failed: $buildExit" }
    } finally { $ErrorActionPreference = 'Stop'; Pop-Location }
    $messages = @()
    foreach ($line in $build) {
        if ($line -is [string] -and $line.StartsWith('{')) { $messages += ($line | ConvertFrom-Json) }
    }
    $probeArtifact = Get-NavigationArtifact $messages 'navigation-probe'
    $helperArtifact = Get-NavigationArtifact $messages 'rust-cef-helper'
    if (-not (Test-Path -LiteralPath $probeArtifact)) { throw "Probe artifact does not exist: $probeArtifact" }
    if (-not (Test-Path -LiteralPath $helperArtifact)) { throw "Helper artifact does not exist: $helperArtifact" }
    # Chinese characters and spaces are intentional: staging must resolve real
    # filesystem paths, not a guessed target directory layout.
    $bundle = Join-Path $run ([string][char]0x5bfc + [char]0x822a + ' Navigation bundle with spaces')
    & (Join-Path $PSScriptRoot 'package-component.ps1') -HostExecutable $probeArtifact -HelperExecutable $helperArtifact -CefPath $cef -OutputDirectory $bundle -HostBuildVersion 'navigation-probe-test-support'
    $probeFile = Join-Path $bundle ([IO.Path]::GetFileName($probeArtifact))
    $helperFile = Join-Path $bundle ([IO.Path]::GetFileName($helperArtifact))
    if (-not (Test-Path -LiteralPath $probeFile)) { throw "Staged probe does not exist: $probeFile" }
    if (-not (Test-Path -LiteralPath $helperFile)) { throw "Staged helper does not exist: $helperFile" }
    Write-NavigationJson @{
        host_artifact = $probeArtifact
        helper_artifact = $helperArtifact
        host_sha256 = (Get-FileHash -LiteralPath $probeFile).Hash
        helper_sha256 = (Get-FileHash -LiteralPath $helperFile).Hash
    } (Join-Path $run 'artifact-identity.json')

    foreach ($selectedMode in $modes) {
        $case = Join-Path $run "$selectedMode-Software"
        $cwd = Join-Path $case 'unrelated cwd'
        New-Item -ItemType Directory -Path $cwd -Force | Out-Null
        $reportPath = Join-Path $case 'probe.json'
        $gate = Join-Path $case 'observer.release'
        $argsForProbe = @('--mode', $selectedMode, '--backend', 'Software', '--report', $reportPath, '--helper', $helperFile, '--observer-gate', $gate, '--scenario', 'Smoke')
        if ($RetiredLoadIsolation) { $argsForProbe = @('--retired-load-isolation') + $argsForProbe }
        $launch = Invoke-Probe $probeFile $argsForProbe $cwd $gate $helperFile
        if ($RetiredLoadIsolation) { Write-NavigationJson $launch (Join-Path $case 'isolation-launch.json') }
        Write-NavigationJson @{
            children = $launch.children
            image_verified = $launch.image_verified
            live_images_verified = $launch.live_images_verified
            observer_error = $launch.observer_error
            remaining_processes = $launch.remaining_processes
            observation_samples = $launch.observation_samples
            residual_observations = $launch.residual_observations
        } (Join-Path $case 'child-images.json')
        [IO.File]::WriteAllText((Join-Path $case 'stdout.log'), $launch.stdout)
        [IO.File]::WriteAllText((Join-Path $case 'stderr.log'), $launch.stderr)
        $observed = $null
        $reportValid = $false
        if (Test-Path -LiteralPath $reportPath) {
            $observed = Get-Content -LiteralPath $reportPath -Raw -Encoding UTF8 | ConvertFrom-Json
            $reportValid = if ($RetiredLoadIsolation) { Test-RetiredLoadIsolation $observed $launch $selectedMode } else { Test-NavigationReport $observed $launch $selectedMode }
        }
        $passed = $launch.exit_code -eq 0 -and
            -not $launch.timed_out -and
            $launch.streams_complete -and
            $launch.image_verified -and
            $null -eq $launch.observer_error -and
            $launch.remaining_processes.Count -eq 0 -and
            $reportValid
        $summary.cases += @{
            kind = 'navigation'
            mode = $selectedMode
            backend = 'Software'
            pid = $launch.pid
            exit_code = $launch.exit_code
            timed_out = $launch.timed_out
            passed = $passed
            renderer_image_verified = $launch.image_verified
            report = $reportPath
            observed = $observed
        }
        Write-Host "navigation/$selectedMode/Software pid=$($launch.pid) exit=$($launch.exit_code) passed=$passed"
    }
    $summary.success = $summary.cases.Count -eq $modeCount -and
        $modeCount -gt 0 -and
        @($summary.cases | Where-Object { -not $_.passed }).Count -eq 0
    if (-not $summary.success) {
        $summary.error = "expected $modeCount passing navigation cases, got $(@($summary.cases | Where-Object { $_.passed }).Count)"
    }
} catch {
    $summary.error = $_.Exception.Message
    Write-Host "Navigation validation failed: $($summary.error)"
} finally {
    $env:CARGO_TARGET_DIR = $oldTarget
    $env:CEF_PATH = $oldCef
    Write-NavigationJson $summary (Join-Path $run 'result.json')
    Write-Host "Evidence: $run"
}
if (-not $summary.success) { exit 1 }
