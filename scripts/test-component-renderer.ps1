# 8.4 renderer-only driver; no workspace/--bins/GPI/Profiles regression.
[CmdletBinding()]
param(
    [ValidateSet('Integrated','Threaded','All')][string]$Mode='All',
    [ValidateSet('Software','D3d11','All')][string]$Backend='Software',
    [string]$CefPath,[string]$TargetDir,
    [ValidateRange(1,900)][int]$TimeoutSeconds=120
)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'common.ps1')
. (Join-Path $PSScriptRoot 'component-process-observer.ps1')
. (Join-Path $PSScriptRoot 'component-probe-launcher.ps1')
function Write-RendererJson($Value,[string]$Path){
    [IO.File]::WriteAllText($Path,($Value|ConvertTo-Json -Depth 18),(New-Object Text.UTF8Encoding($false)))
}
function Get-RendererArtifact($Messages,[string]$Name){
    $found=@($Messages|Where-Object {$_.reason -eq 'compiler-artifact' -and $_.target.name -eq $Name -and $null -ne $_.executable})
    if($found.Count -ne 1){throw "Expected one Cargo executable artifact for $Name"}
    [IO.Path]::GetFullPath($found[0].executable)
}
$requiredChecks=@('runtime_ready','initial_state','helper_observer','static_not_hung','devtools_enabled',
    'crash_submitted','terminated_event','browser_not_closed','terminated_snapshot','no_automatic_reload',
    'explicit_reload_admitted','render_view_ready','fresh_request_after_recovery','fresh_frame_after_recovery',
    'same_browser_recovered','normal_close','native_shutdown','threads')
function Test-RendererReport($Observed,$Launch,[string]$SelectedMode,[string]$SelectedBackend){
    if($null -eq $Observed -or $Observed.schema -ne 1 -or $Observed.kind -ne 'renderer' -or
        $Observed.success -ne $true -or $null -ne $Observed.error -or $Observed.exit_code -ne 0){return $false}
    $checks=@($Observed.checks)
    if($checks.Count -ne $requiredChecks.Count){return $false}
    for($i=0;$i -lt $requiredChecks.Count;$i++){
        if($checks[$i].id -ne $requiredChecks[$i] -or $checks[$i].passed -ne $true){return $false}
    }
    $requiredFields=@('pid','requested_mode','actual_mode','requested_backend','actual_backend',
        'main_thread_id','cef_ui_thread_id','init_thread_id','shutdown_thread_id','manual_pump_count',
        'created','closed','valid_frame_browsers','frames','first_frame_width','first_frame_height',
        'tasks_posted','tasks_completed','task_post_failures','pending_tasks','pending_requests','pending_profiles',
        'pending_cookie_flushes','profile_references_retained','app_reference_retained','root_lease_held',
        'cef_shutdown_calls','native_calls','native_calls_after_shutdown','observer_released','renderer_references_retained','renderer_probe_enabled','crash_command_submitted','terminated_events','terminated_native_status','terminated_code',
        'requests_before_crash','requests_after_quiescence','requests_after_recovery','static_quiet_observed','no_automatic_reload_observed',
        'reload_command_accepted','fresh_request_after_recovery','fresh_frame_after_recovery','same_browser_recovered',
        'pre_crash_frame_sequence','post_recovery_frame_sequence','frames_before_crash','frames_after_recovery',
        'runtime_generation','default_profile_id','default_profile_ready','initial_renderer_state')
    foreach($name in $requiredFields){if($null -eq $Observed.PSObject.Properties[$name]){return $false}}
    $threadRule=($SelectedMode -eq 'Integrated' -and $Observed.cef_ui_thread_id -eq $Observed.main_thread_id -and $Observed.manual_pump_count -gt 0) -or
        ($SelectedMode -eq 'Threaded' -and $Observed.cef_ui_thread_id -ne $Observed.main_thread_id -and $Observed.manual_pump_count -eq 0)
    return ($Observed.pid -eq $Launch.pid -and $Observed.requested_mode -eq $SelectedMode -and $Observed.actual_mode -eq $SelectedMode -and
        $Observed.requested_backend -eq $SelectedBackend -and $Observed.actual_backend -eq $SelectedBackend -and
        $Observed.main_thread_id -gt 0 -and $Observed.cef_ui_thread_id -gt 0 -and $threadRule -and
        $Observed.init_thread_id -eq $Observed.main_thread_id -and $Observed.shutdown_thread_id -eq $Observed.main_thread_id -and
        $Observed.created -eq 1 -and $Observed.closed -eq 1 -and $Observed.valid_frame_browsers -eq 1 -and $Observed.frames -ge 2 -and
        $Observed.first_frame_width -eq 320 -and $Observed.first_frame_height -eq 240 -and
        $Observed.tasks_posted -gt 0 -and $Observed.tasks_completed -eq $Observed.tasks_posted -and $Observed.task_post_failures -eq 0 -and
        $Observed.pending_tasks -eq 0 -and $Observed.pending_requests -eq 0 -and $Observed.pending_profiles -eq 0 -and $Observed.pending_cookie_flushes -eq 0 -and
        $Observed.profile_references_retained -eq 0 -and $Observed.renderer_references_retained -eq 0 -and
        $Observed.runtime_generation -gt 0 -and $Observed.default_profile_id -eq 0 -and $Observed.default_profile_ready -eq $true -and
        $Observed.initial_renderer_state -in @('Starting','Running') -and
        $Observed.renderer_probe_enabled -eq $true -and $Observed.crash_command_submitted -eq $true -and $Observed.terminated_events -eq 1 -and
        $null -ne $Observed.terminated_native_status -and $null -ne $Observed.terminated_code -and
        $Observed.requests_before_crash -eq 1 -and $Observed.requests_after_quiescence -eq $Observed.requests_before_crash -and
        $Observed.requests_after_recovery -eq ($Observed.requests_before_crash + 1) -and
        $Observed.static_quiet_observed -eq $true -and $Observed.no_automatic_reload_observed -eq $true -and
        $Observed.reload_command_accepted -eq $true -and $Observed.fresh_request_after_recovery -eq $true -and
        $Observed.fresh_frame_after_recovery -eq $true -and $Observed.same_browser_recovered -eq $true -and
        $Observed.frames_before_crash -gt 0 -and $Observed.frames_after_recovery -gt 0 -and
        $Observed.pre_crash_frame_sequence -gt 0 -and $Observed.post_recovery_frame_sequence -gt $Observed.pre_crash_frame_sequence -and
        $Observed.app_reference_retained -eq $false -and $Observed.root_lease_held -eq $false -and
        $Observed.cef_shutdown_calls -eq 1 -and $Observed.native_calls -gt 0 -and $Observed.native_calls_after_shutdown -eq 0 -and $Observed.observer_released -eq $true)
}
$run = New-PocDirectory (Resolve-ValidationDir 'renderer' ([guid]::NewGuid().ToString('N')))
New-Item -ItemType Directory -Path $run|Out-Null
$summary=@{schema=1;stage='Renderer';expected_cases=0;cases=@();error=$null;success=$false}
$oldTarget=$env:CARGO_TARGET_DIR;$oldCef=$env:CEF_PATH
try{
    $modes=@(if($Mode -eq 'All'){'Integrated';'Threaded'}else{$Mode})
    $backends=@(if($Backend -eq 'All'){'Software';'D3d11'}else{$Backend})
    $summary.expected_cases=$modes.Count * $backends.Count
    $cef=Resolve-CefPath $CefPath;$env:CEF_PATH=$cef;$env:CARGO_TARGET_DIR=Resolve-TargetDir $TargetDir
    Push-Location $PocRoot
    try{
        $ErrorActionPreference='Continue'
        $build=@(& cargo build --locked --target $PocTarget -p rust-cef-runtime -p rust-cef-helper --features rust-cef-runtime/test-support --bin renderer-probe --bin rust-cef-helper --message-format=json-render-diagnostics 2>&1|
            Tee-Object -FilePath (Join-Path $run 'build.log')|ForEach-Object {Write-Host $_;$_})
        $buildExit=$LASTEXITCODE;$ErrorActionPreference='Stop'
        if($buildExit -ne 0){throw "Cargo build failed: $buildExit"}
    }finally{$ErrorActionPreference='Stop';Pop-Location}
    $messages=@();foreach($line in $build){if($line -is [string] -and $line.StartsWith('{')){$messages+=($line|ConvertFrom-Json)}}
    $probeArtifact=Get-RendererArtifact $messages 'renderer-probe';$helperArtifact=Get-RendererArtifact $messages 'rust-cef-helper'
    $bundle=Join-Path $run ([string][char]0x6545+[char]0x969c+' Renderer bundle with spaces')
    & (Join-Path $PSScriptRoot 'package-component.ps1') -HostExecutable $probeArtifact -HelperExecutable $helperArtifact -CefPath $cef -OutputDirectory $bundle -HostBuildVersion 'renderer-probe-test-support'
    $probe=Join-Path $bundle ([IO.Path]::GetFileName($probeArtifact));$helper=Join-Path $bundle ([IO.Path]::GetFileName($helperArtifact))
    Write-RendererJson @{host_artifact=$probeArtifact;helper_artifact=$helperArtifact;host_sha256=(Get-FileHash -LiteralPath $probe).Hash;helper_sha256=(Get-FileHash -LiteralPath $helper).Hash} (Join-Path $run 'artifact-identity.json')
    foreach($selectedMode in $modes){
      foreach($selectedBackend in $backends){
        $case=Join-Path $run "$selectedMode-$selectedBackend";$cwd=Join-Path $case 'unrelated cwd';New-Item -ItemType Directory -Path $cwd -Force|Out-Null
        $report=Join-Path $case 'probe.json';$gate=Join-Path $case 'observer.release'
        $launch=Invoke-Probe $probe @('--mode',$selectedMode,'--backend',$selectedBackend,'--report',$report,'--helper',$helper,'--observer-gate',$gate,'--scenario','Smoke') $cwd $gate $helper
        Write-RendererJson @{children=$launch.children;image_verified=$launch.image_verified;live_images_verified=$launch.live_images_verified;observer_error=$launch.observer_error;remaining_processes=$launch.remaining_processes;observation_samples=$launch.observation_samples;residual_observations=$launch.residual_observations} (Join-Path $case 'child-images.json')
        [IO.File]::WriteAllText((Join-Path $case 'stdout.log'),$launch.stdout);[IO.File]::WriteAllText((Join-Path $case 'stderr.log'),$launch.stderr)
        $observed=$null;$valid=$false
        if(Test-Path -LiteralPath $report){$observed=Get-Content -LiteralPath $report -Raw -Encoding UTF8|ConvertFrom-Json;$valid=Test-RendererReport $observed $launch $selectedMode $selectedBackend}
        $passed=$launch.exit_code -eq 0 -and -not $launch.timed_out -and $launch.streams_complete -and $launch.image_verified -and
            $null -eq $launch.observer_error -and $launch.remaining_processes.Count -eq 0 -and $valid
        $summary.cases+=@{mode=$selectedMode;backend=$selectedBackend;pid=$launch.pid;exit_code=$launch.exit_code;timed_out=$launch.timed_out;passed=$passed;report=$report}
        Write-Host "renderer/$selectedMode/$selectedBackend pid=$($launch.pid) exit=$($launch.exit_code) passed=$passed"
        if(-not $passed){throw 'Renderer case failed; all raw evidence retained'}
      }
    }
    $summary.success=$summary.cases.Count -eq $summary.expected_cases -and $summary.expected_cases -gt 0 -and @($summary.cases|Where-Object {$_.passed -ne $true}).Count -eq 0
}catch{$summary.error=$_.Exception.Message;Write-Warning $summary.error}
finally{
    $env:CARGO_TARGET_DIR=$oldTarget;$env:CEF_PATH=$oldCef
    if($null -ne $summary.error){$summary.success=$false}
    Write-RendererJson $summary (Join-Path $run 'result.json');Write-Host "Evidence: $run"
}
if(-not $summary.success){exit 1}
exit 0
