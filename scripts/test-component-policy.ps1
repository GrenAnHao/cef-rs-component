# Targeted 8.3 policy/beforeunload evidence, not a workspace/UI regression suite.
[CmdletBinding()]
param(
    [ValidateSet('Integrated','Threaded','All')][string]$Mode='All',
    [ValidateSet('Memory','Persistent','All')][string]$Storage='All',
    [ValidateSet('Software','D3d11','All')][string]$Backend='Software',
    [string]$CefPath, [string]$TargetDir,
    [ValidateRange(30,900)][int]$TimeoutSeconds=120,
    [switch]$InteractiveInputApproved
)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'common.ps1')
. (Join-Path $PSScriptRoot 'component-process-observer.ps1')
. (Join-Path $PSScriptRoot 'component-probe-launcher.ps1')
if (-not $InteractiveInputApproved) { throw 'This probe activates its own visible window and uses global SendInput. Obtain quiet-desktop consent, then explicitly pass -InteractiveInputApproved; focus checks are not atomic with OS input.' }
function Write-PolicyJson($Value,[string]$Path) {
    [IO.File]::WriteAllText($Path,($Value|ConvertTo-Json -Depth 24),(New-Object Text.UTF8Encoding($false)))
}
function Find-PolicyArtifact($Messages,[string]$Name) {
    $found=@($Messages|Where-Object { $_.reason -eq 'compiler-artifact' -and $_.target.name -eq $Name -and $null -ne $_.executable })
    if ($found.Count -ne 1) { throw "Expected exactly one Cargo executable artifact for $Name" }
    [IO.Path]::GetFullPath($found[0].executable)
}
$requiredChecks=@('runtime_ready','own_frame_and_observer','four_click_activation',
 'main_subresource_redirect_zero','service_worker_zero','file_dialog_refused','permissions_refused',
 'certificate_refused','js_dialogs_suppressed','popup_download_protocol_refused','native_windows_observed_clear',
 'graceful_denial_alive','cancelled_ticket_retained','pending_force_once','reload_reset_stale_reply',
 'timeout_denial_alive','data_reload_history','data_cross_browser_denied','data_unapproved_denied',
 'complete_browser_ownership','native_shutdown_and_tasks','mode_threads')
function Test-PolicyReport($Report,$Launch,[string]$SelectedMode,[string]$SelectedStorage) {
    if ($null -eq $Report -or $Report.success -ne $true -or $null -ne $Report.error) { return $false }
    $checks=@($Report.checks)
    if ($checks.Count -ne $requiredChecks.Count) { return $false }
    for ($i=0;$i -lt $requiredChecks.Count;$i++) {
        if ($checks[$i].id -ne $requiredChecks[$i] -or $checks[$i].passed -ne $true) { return $false }
    }
    $inputs=@($Report.input_diagnostics)
    if ($inputs.Count -ne 16 -or $Report.observed_mouse_messages -ne 16 -or
        @($inputs|Where-Object {$_.marker_match -ne $true -or $_.source_is_injected_mouse -ne $true}).Count -ne 0) { return $false }
    $d=$Report.diagnostics
    $threadRule=($SelectedMode -eq 'Integrated' -and $d.cef_ui_thread_id -eq $d.main_thread_id -and $d.manual_pump_count -gt 0) -or
       ($SelectedMode -eq 'Threaded' -and $d.cef_ui_thread_id -gt 0 -and $d.cef_ui_thread_id -ne $d.main_thread_id -and $d.manual_pump_count -eq 0)
    return ($Report.schema -eq 1 -and $Report.kind -eq 'policy' -and $Report.pid -eq $Launch.pid -and
      $Report.requested_mode -eq $SelectedMode -and $Report.storage -eq $SelectedStorage -and $Report.exit_code -eq 0 -and
      $Report.created -eq 7 -and $Report.closed -eq 7 -and $Report.valid_frame_browsers -eq 7 -and $Report.painted_frame_browsers -eq 7 -and $Report.frames -gt 0 -and
      $Report.injected_clicks -eq 8 -and $Report.os_mouse_messages -eq 16 -and $Report.painted_frames -gt 0 -and
      $Report.unexpected_visible_windows_peak -eq 0 -and $Report.timeout_wait_ms -ge 9000 -and $Report.certificate_main_error -eq -202 -and
      $Report.native_reset_enable_ack -eq $true -and $Report.native_reset_handle_ack -eq $true -and $Report.native_reset_disable_ack -eq $true -and
      $Report.native_reset_notifications -gt 0 -and $Report.probe_dialog_registrations -eq 0 -and
      $Report.data_reload_document_changed -eq $true -and $Report.data_cross_browser_native_denials -gt 0 -and $Report.data_unapproved_native_denials -gt 0 -and
      $d.mode -eq $SelectedMode -and $threadRule -and $d.main_thread_id -gt 0 -and
      $d.init_thread_id -eq $d.main_thread_id -and $d.shutdown_thread_id -eq $d.main_thread_id -and
      $d.actual_backend -eq 'Some(SoftwareRequired)' -and $d.cef_shutdown_calls -eq 1 -and
      $d.native_calls -gt 0 -and $d.native_calls_after_shutdown -eq 0 -and
      $d.tasks_posted -gt 0 -and $d.tasks_completed+$d.task_post_failures -eq $d.tasks_posted -and $d.pending_tasks -eq 0 -and
      $d.pending_requests -eq 0 -and $d.pending_profiles -eq 0 -and $d.pending_cookie_flushes -eq 0 -and
      $d.profile_references_retained -eq 0 -and $d.app_reference_retained -eq $false -and $d.root_lease_held -eq $false -and
      $d.before_unload_requested -ge 4 -and $d.before_unload_answered -ge 1 -and $d.before_unload_invalidated -ge 1 -and $d.before_unload_timeouts -ge 1 -and
      $Report.fixture_state.forbidden_requests -eq 0 -and $Report.fixture_state.tls_requests -eq 0 -and @($Report.fixture_state.errors).Count -eq 0 -and
      $Launch.exit_code -eq 0 -and -not $Launch.timed_out -and $Launch.streams_complete -and $Launch.image_verified -and
      $null -eq $Launch.observer_error -and $Launch.remaining_processes.Count -eq 0)
}
$run = New-PocDirectory (Resolve-ValidationDir 'policy' ([guid]::NewGuid().ToString('N')))
New-Item -ItemType Directory -Path $run|Out-Null
$summary=@{schema=1;stage='Policy';cases=@();expected_cases=0;error=$null;success=$false;server_stopped=$false}
$oldTarget=$env:CARGO_TARGET_DIR; $oldCef=$env:CEF_PATH; $server=$null; $ready=$null
try {
    if ($Backend -ne 'Software') { throw 'Policy validation implements SoftwareRequired only; no accelerated fallback or empty accelerated matrix' }
    $modes=@(if($Mode -eq 'All'){'Integrated';'Threaded'}else{$Mode})
    $storages=@(if($Storage -eq 'All'){'Memory';'Persistent'}else{$Storage})
    $summary.expected_cases=$modes.Count*$storages.Count
    $cef=Resolve-CefPath $CefPath; $env:CEF_PATH=$cef; $env:CARGO_TARGET_DIR=Resolve-TargetDir $TargetDir
    Push-Location $PocRoot
    try {
        $ErrorActionPreference='Continue'
        $build=@(& cargo build --locked --target $PocTarget -p rust-cef-runtime -p rust-cef-helper --features rust-cef-runtime/test-support --bin policy-probe --bin rust-cef-helper --message-format=json-render-diagnostics 2>&1 | Tee-Object -FilePath (Join-Path $run 'build.log') | ForEach-Object {Write-Host $_;$_})
        $buildExit=$LASTEXITCODE; $ErrorActionPreference='Stop'
        if($buildExit -ne 0){throw "Cargo build failed: $buildExit"}
    } finally {$ErrorActionPreference='Stop';Pop-Location}
    $messages=@();foreach($line in $build){if($line -is [string] -and $line.StartsWith('{')){$messages+=($line|ConvertFrom-Json)}}
    $artifact=Find-PolicyArtifact $messages 'policy-probe';$helperArtifact=Find-PolicyArtifact $messages 'rust-cef-helper'
    $bundle=Join-Path $run ([string][char]0x53d1+[char]0x884c+' Policy bundle with spaces')
    & (Join-Path $PSScriptRoot 'package-component.ps1') -HostExecutable $artifact -HelperExecutable $helperArtifact -CefPath $cef -OutputDirectory $bundle -HostBuildVersion 'policy-probe-test-support'
    $hostFile=Join-Path $bundle ([IO.Path]::GetFileName($artifact));$helperFile=Join-Path $bundle ([IO.Path]::GetFileName($helperArtifact))
    Write-PolicyJson @{host_artifact=$artifact;helper_artifact=$helperArtifact;host_sha256=(Get-FileHash -LiteralPath $hostFile).Hash;helper_sha256=(Get-FileHash -LiteralPath $helperFile).Hash} (Join-Path $run 'artifact-identity.json')
    $openssl=(Get-Command openssl.exe -ErrorAction Stop).Source
    $cert=Join-Path $run 'fixture-cert.pem';$key=Join-Path $run 'fixture-key.pem'
    # Isolated test certificate only. Never installed in a machine/user trust store.
    & $openssl req -x509 -newkey rsa:2048 -nodes -keyout $key -out $cert -days 1 -subj '/CN=GPUI-CEF Policy Fixture' -addext 'subjectAltName=IP:127.0.0.1' 2>&1 | Tee-Object -FilePath (Join-Path $run 'certificate-generation.log') | ForEach-Object {Write-Host $_}
    if($LASTEXITCODE -ne 0){throw 'Isolated test certificate generation failed'}
    $readyFile=Join-Path $run 'server-ready.json';$finalFile=Join-Path $run 'server-final.json'
    $psi=New-Object Diagnostics.ProcessStartInfo
    $psi.FileName=(Get-Command python.exe -ErrorAction Stop).Source;$psi.UseShellExecute=$false
    $psi.RedirectStandardOutput=$true;$psi.RedirectStandardError=$true;$psi.WorkingDirectory=$PocRoot
    foreach($arg in @('-u',(Join-Path $PSScriptRoot 'component-policy-server.py'),'--ready',$readyFile,'--final',$finalFile,'--cert',$cert,'--key',$key)){[void]$psi.ArgumentList.Add($arg)}
    $server=New-Object Diagnostics.Process;$server.StartInfo=$psi
    if(-not $server.Start()){throw 'Policy fixture did not start'}
    $serverOut=$server.StandardOutput.ReadToEndAsync();$serverErr=$server.StandardError.ReadToEndAsync()
    $deadline=[DateTime]::UtcNow.AddSeconds(15)
    while(-not(Test-Path -LiteralPath $readyFile)){
        if($server.HasExited -or [DateTime]::UtcNow -ge $deadline){throw 'Policy fixture readiness failed'}
        Start-Sleep -Milliseconds 100
    }
    $ready=Get-Content -LiteralPath $readyFile -Raw -Encoding UTF8|ConvertFrom-Json
    if($ready.pid -ne $server.Id){throw 'Policy fixture PID mismatch'}
    foreach($selectedMode in $modes){foreach($selectedStorage in $storages){
        $case=Join-Path $run "$selectedMode-$selectedStorage";$cwd=Join-Path $case 'unrelated cwd'
        New-Item -ItemType Directory -Path $cwd -Force|Out-Null
        $reportFile=Join-Path $case 'probe.json';$gate=Join-Path $case 'observer.release';$caseId=[guid]::NewGuid().ToString('N')
        $args=@('--mode',$selectedMode,'--backend','Software','--report',$reportFile,'--helper',$helperFile,'--observer-gate',$gate,
            '--fixture-origin',$ready.allowed_origin,'--storage',$selectedStorage,'--case',$caseId)
        $launch=Invoke-Probe $hostFile $args $cwd $gate $helperFile
        Write-PolicyJson @{children=$launch.children;image_verified=$launch.image_verified;live_images_verified=$launch.live_images_verified;observer_error=$launch.observer_error;
            remaining_processes=$launch.remaining_processes;observation_samples=$launch.observation_samples;residual_observations=$launch.residual_observations} (Join-Path $case 'child-images.json')
        [IO.File]::WriteAllText((Join-Path $case 'stdout.log'),$launch.stdout);[IO.File]::WriteAllText((Join-Path $case 'stderr.log'),$launch.stderr)
        $observed=$null;if(Test-Path -LiteralPath $reportFile){$observed=Get-Content -LiteralPath $reportFile -Raw -Encoding UTF8|ConvertFrom-Json}
        $passed=Test-PolicyReport $observed $launch $selectedMode $selectedStorage
        $saved=@{mode=$selectedMode;storage=$selectedStorage;pid=$launch.pid;exit_code=$launch.exit_code;passed=$passed;report=$reportFile;timed_out=$launch.timed_out}
        $summary.cases+=@($saved);Write-Host "policy/$selectedMode/$selectedStorage pid=$($launch.pid) exit=$($launch.exit_code) passed=$passed"
        if(-not $passed){throw 'Policy case failed; raw report and image evidence retained'}
        if(@(Get-ChildItem -LiteralPath $cwd -Filter 'policy-probe.bin' -Recurse -File).Count -ne 0){throw 'Unexpected saved download in controlled CWD'}
    }}
} catch {$summary.error=$_.Exception.Message;Write-Warning $summary.error}
finally {
    if($null -ne $server){
        try {
            if(-not $server.HasExited -and $null -ne $ready){
                $null=Invoke-RestMethod -Method Post -Uri ($ready.allowed_origin+'/stop') -ContentType 'application/json' -Body '{}' -TimeoutSec 3
            }
            if(-not $server.WaitForExit(10000)){ $server.Kill($true);$server.WaitForExit();throw 'Policy fixture required forced termination' }
            [IO.File]::WriteAllText((Join-Path $run 'server-stdout.log'),$serverOut.GetAwaiter().GetResult())
            [IO.File]::WriteAllText((Join-Path $run 'server-stderr.log'),$serverErr.GetAwaiter().GetResult())
            if(Test-Path -LiteralPath (Join-Path $run 'server-final.json')){
                $final=Get-Content -LiteralPath (Join-Path $run 'server-final.json') -Raw -Encoding UTF8|ConvertFrom-Json
                $summary.server_stopped=$server.ExitCode -eq 0 -and $final.stopped -eq $true -and $final.ok -eq $true -and
                    @($final.errors).Count -eq 0 -and $final.counters.forbidden_http -eq 0 -and $final.counters.tls_http -eq 0
            }
        } catch {if($null -eq $summary.error){$summary.error=$_.Exception.Message}}
        finally {if(-not $server.HasExited){$server.Kill($true);$server.WaitForExit()};$server.Dispose()}
    }
    $env:CARGO_TARGET_DIR=$oldTarget;$env:CEF_PATH=$oldCef
    $summary.success=$null -eq $summary.error -and $summary.expected_cases -gt 0 -and $summary.cases.Count -eq $summary.expected_cases -and
        @($summary.cases|Where-Object {$_.passed -ne $true}).Count -eq 0 -and $summary.server_stopped
    Write-PolicyJson $summary (Join-Path $run 'result.json');Write-Host "Evidence: $run"
}
if(-not $summary.success){exit 1}
exit 0
