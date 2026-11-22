[CmdletBinding()]
param(
    [string]$CefPath,
    [string]$TargetDir,
    [ValidateSet('Integrated','Threaded','All')][string]$Mode = 'Threaded',
    [ValidateSet('Software','D3d11','All')][string]$Backend = 'Software',
    [ValidateRange(1,900)][int]$TimeoutSeconds = 180,
    [switch]$OutboundDenial
)
# The default is still one windowless COPY cell. Neither path sends OS input.
$ErrorActionPreference='Stop';Set-StrictMode -Version Latest
if ($OutboundDenial) {
    if ($Mode -ne 'All' -or $Backend -ne 'All') { throw 'OutboundDenial requires Mode All and Backend All' }
} elseif ($Mode -eq 'All' -or $Backend -eq 'All') {
    throw 'The inbound COPY fixture requires one explicit mode and backend'
}
. (Join-Path $PSScriptRoot 'common.ps1')
. (Join-Path $PSScriptRoot 'component-process-observer.ps1')
. (Join-Path $PSScriptRoot 'component-probe-launcher.ps1')
function Save-Json($Value,[string]$Path){[IO.File]::WriteAllText($Path,($Value|ConvertTo-Json -Depth 18),[Text.UTF8Encoding]::new($false))}
function Artifact($Messages,[string]$Name){$rows=@($Messages|Where-Object{$_.reason -eq 'compiler-artifact' -and $_.target.name -eq $Name -and $_.executable});if($rows.Count -ne 1){throw 'Actual executable artifact not unique'};[IO.Path]::GetFullPath($rows[0].executable)}
function Test-OutboundDenial($Probe, $Launch, [string]$Thread, [string]$Render) {
    if ($null -eq $Probe -or $Probe.kind -cne 'windowless-outbound-drag-denial' -or $Probe.success -isnot [bool] -or
        -not $Probe.success -or $null -ne $Probe.error -or $Probe.pid -ne $Launch.pid -or $Launch.exit_code -ne 0 -or
        $Launch.timed_out -or -not $Launch.streams_complete -or -not $Launch.image_verified -or -not $Launch.live_images_verified -or
        $null -ne $Launch.observer_error -or @($Launch.remaining_processes).Count -ne 0 -or
        $Probe.mode -cne $Thread -or $Probe.backend -cne "${Render}Required" -or $Probe.observed_backend -cne "Some(${Render}Required)") { return $false }
    $r = $Probe.outbound_drag
    if ($null -eq $r -or $null -eq $r.live -or $null -eq $r.live.native -or $null -eq $r.retired_callback_state -or
        $null -eq $Probe.input_after_close) { return $false }
    $w = $r.live; $n = $w.native; $retired = $r.retired_callback_state; $input = $Probe.input_after_close
    foreach ($flag in @($n.active,$n.arguments_present,$retired.active,$retired.arguments_present,$w.same_address,$w.trusted_recovery_click,$Probe.finished)) {
        if ($flag -isnot [bool] -or -not $flag) { return $false }
    }
    foreach ($flag in @($r.fixture_file_created,$r.physical_input_sent,$r.system_ole_started,$Probe.app_retained,$Probe.root_lease,
        $Probe.foreground_requested,$Probe.task_12_3_accepted,$Probe.file_drop_verified,$Probe.cancel_verified,$Probe.active_drag_at_shutdown,
        $Probe.gpui_or_win32_drag_verified,$input.file_drag_active,$input.cleanup_pending,$input.native_execution_unconfirmed)) {
        if ($flag -isnot [bool] -or $flag) { return $false }
    }
    $threadOk = if ($Thread -ceq 'Integrated') {
        $Probe.cef_ui_thread -eq $Probe.main_thread -and $Probe.manual_pump -gt 0
    } else { $Probe.cef_ui_thread -ne $Probe.main_thread -and $Probe.manual_pump -eq 0 }
    return $threadOk -and $Probe.main_thread -gt 0 -and $Probe.main_thread -eq $Probe.init_thread -and $Probe.main_thread -eq $Probe.shutdown_thread -and
        $Probe.cef_ui_thread -gt 0 -and $n.thread -eq $Probe.cef_ui_thread -and $n.callbacks -eq 1 -and
        $retired.callbacks -eq $n.callbacks -and $retired.thread -eq $n.thread -and
        ($w.browser -join ',') -ceq '1,1' -and $w.generation -gt 0 -and $w.initial_sequence -gt 0 -and $w.recovered_sequence -gt $w.initial_sequence -and
        $w.started_title -ceq 'outbound-start:1' -and $w.recovered_title -ceq 'outbound-recovered:1' -and
        ($w.recovered_center_bgra -join ',') -ceq '70,130,44,255' -and $Probe.payload_bytes -eq 0 -and $null -eq $Probe.expected_payload_fnv32 -and
        @($Probe.samples).Count -eq 0 -and @($Probe.terminal_observations).Count -eq 0 -and
        $Probe.created -eq 1 -and $Probe.closed -eq 1 -and $Probe.shutdown_calls -eq 1 -and $Probe.native_after -eq 0 -and $Probe.native_calls -gt 0 -and
        $Probe.tasks_posted -gt 0 -and $Probe.tasks_posted -eq $Probe.tasks_completed -and $Probe.pending_tasks -eq 0 -and
        $Probe.pending_profiles -eq 0 -and $Probe.pending_requests -eq 0 -and $r.profile_references_retained -eq 0 -and
        $r.pending_cookie_flushes -eq 0 -and $r.pressed_after_close -eq 0 -and $r.inflight_after_close -eq 0 -and $input.phase -ceq 'Idle'
}
$run=New-PocDirectory (Resolve-ValidationDir 'file-drop' ([guid]::NewGuid().ToString('N')))
$kind = if ($OutboundDenial) { 'windowless-outbound-drag-denial' } else { 'windowless-file-drop-slice' }
$modes = @(if ($OutboundDenial) { 'Integrated'; 'Threaded' } else { $Mode })
$backends = @(if ($OutboundDenial) { 'Software'; 'D3d11' } else { $Backend })
$summary=@{kind=$kind;expected_cases=($modes.Count*$backends.Count);cases=@();success=$false;error=$null;task_12_3_accepted=$false;foreground_requested=$false;gpui_or_win32_drag_verified=$false;bundle=$null}
$oldCef=$env:CEF_PATH;$oldTarget=$env:CARGO_TARGET_DIR
try{
 $cef=Resolve-CefPath $CefPath;$env:CEF_PATH=$cef;$env:CARGO_TARGET_DIR=Resolve-TargetDir $TargetDir
 Push-Location $PocRoot
 try{
  $ErrorActionPreference='Continue'
  $raw=@(& cargo build --locked --target $PocTarget -p rust-cef-runtime -p rust-cef-helper --features rust-cef-runtime/test-support --bin file-drop-probe --bin rust-cef-helper --message-format=json-render-diagnostics 2>&1|Tee-Object -FilePath (Join-Path $run 'build.log')|ForEach-Object{Write-Host $_;$_})
  $code=$LASTEXITCODE;$ErrorActionPreference='Stop';if($code -ne 0){throw "File-drop build failed $code"}
 }finally{$ErrorActionPreference='Stop';Pop-Location}
 $messages=@();foreach($line in $raw){if($line -is [string] -and $line.StartsWith('{')){$messages+=($line|ConvertFrom-Json)}}
 $hostArtifact=Artifact $messages 'file-drop-probe';$helperArtifact=Artifact $messages 'rust-cef-helper'
 $bundle=Join-Path $run '发行 file drop bundle with spaces'
 $summary.bundle=$bundle
 & (Join-Path $PSScriptRoot 'package-component.ps1') -HostExecutable $hostArtifact -HelperExecutable $helperArtifact -CefPath $cef -OutputDirectory $bundle -HostBuildVersion 'file-drop-probe-test-support'
 $hostFile=Join-Path $bundle ([IO.Path]::GetFileName($hostArtifact));$helperFile=Join-Path $bundle ([IO.Path]::GetFileName($helperArtifact))
 Save-Json @{host_artifact=$hostArtifact;helper_artifact=$helperArtifact;host_sha256=(Get-PocFileSha256 $hostFile);helper_sha256=(Get-PocFileSha256 $helperFile)} (Join-Path $run 'artifact-identity.json')
 foreach ($thread in $modes) {
  foreach ($render in $backends) {
   $case=Join-Path $run "$thread-$render"
   $cwd=Join-Path $case 'unrelated cwd'
   New-Item -Path $cwd -ItemType Directory -Force|Out-Null
   $report=Join-Path $case 'probe.json'; $gate=Join-Path $case 'observer.release'
   $arguments=@('--mode',$thread,'--backend',$render,'--report',$report,'--helper',$helperFile,'--observer-gate',$gate)
   if ($OutboundDenial) { $arguments=@('--outbound-denial')+$arguments }
   $launch=Invoke-Probe $hostFile $arguments $cwd $gate $helperFile
   Save-Json $launch (Join-Path $case 'launch.json')
   $observed=if(Test-Path -LiteralPath $report){Get-Content -LiteralPath $report -Raw -Encoding utf8|ConvertFrom-Json}else{$null}
   $passed=if ($OutboundDenial) { Test-OutboundDenial $observed $launch $thread $render } else {
    $launch.exit_code -eq 0 -and -not $launch.timed_out -and $launch.image_verified -and @($launch.remaining_processes).Count -eq 0 -and $null -ne $observed -and $observed.success
   }
   $summary.cases+=@{mode=$thread;backend=$render;pid=$launch.pid;exit_code=$launch.exit_code;passed=$passed;report=$report;observed=$observed;image_verified=$launch.image_verified;remaining_processes=$launch.remaining_processes}
   Write-Host "$kind/$thread/$render pid=$($launch.pid) exit=$($launch.exit_code) passed=$passed"
   if (-not $passed) { throw 'File-drop actual slice failed; preserve original error and stop' }
  }
 }
 $summary.success=$summary.cases.Count -eq $summary.expected_cases
}catch{$summary.error=$_.Exception.Message;Write-Host "File-drop failure: $($summary.error)"}
finally{$env:CEF_PATH=$oldCef;$env:CARGO_TARGET_DIR=$oldTarget;Save-Json $summary (Join-Path $run 'result.json');Write-Host "Evidence: $run"}
if(-not $summary.success){exit 1}
