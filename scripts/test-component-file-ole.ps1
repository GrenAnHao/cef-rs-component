[CmdletBinding()]
param([string]$CefPath,[string]$TargetDir,[switch]$OleInputApproved,[switch]$OleSourceForegroundApproved,[switch]$OleManualSourceFocusApproved,[switch]$OleCloseWhileDragging,[ValidateRange(1,900)][int]$TimeoutSeconds=180)
# One separately-approved real native OLE source->target case. Never part of All.
$ErrorActionPreference='Stop';Set-StrictMode -Version Latest
if(-not $OleInputApproved){throw 'Separate physical OLE input approval required before launching'}
if($OleManualSourceFocusApproved -and $OleSourceForegroundApproved){throw 'Manual source focus and ordinary foreground request are mutually exclusive'}
. (Join-Path $PSScriptRoot 'common.ps1')
. (Join-Path $PSScriptRoot 'component-process-observer.ps1')
. (Join-Path $PSScriptRoot 'component-probe-launcher.ps1')
function Save-Json($v,[string]$p){[IO.File]::WriteAllText($p,($v|ConvertTo-Json -Depth 40),[Text.UTF8Encoding]::new($false))}
function Artifact($m,[string]$name){$rows=@($m|Where-Object{$_.reason -eq 'compiler-artifact' -and $_.target.name -eq $name -and $_.executable});if($rows.Count -ne 1){throw 'Actual artifact not unique'};[IO.Path]::GetFullPath($rows[0].executable)}
function Start-Owned([string]$exe,[string[]]$argv,[string]$cwd,[hashtable]$owner,[string]$slot){
 if($null -eq $owner -or $slot -notin @('source','driver') -or $null -ne $owner[$slot]){throw 'Unique owned process slot required'}
 $quoted=foreach($argument in $argv){$escaped=[regex]::Replace($argument,'(\\*)"','$1$1\"');'"'+[regex]::Replace($escaped,'(\\+)$','$1$1')+'"'}
 $si=[Diagnostics.ProcessStartInfo]::new();$si.FileName=$exe;$si.Arguments=$quoted -join ' ';$si.WorkingDirectory=$cwd;$si.UseShellExecute=$false;$si.RedirectStandardOutput=$true;$si.RedirectStandardError=$true
 $p=[Diagnostics.Process]::new();$p.StartInfo=$si
 $record=@{process=$p;started=$false;pid=$null;creation_ticks=$null;executable=$exe;stdout=$null;stderr=$null;setup_error=$null}
 # Retain the object BEFORE Start/metadata/stream setup can fail.
 $owner[$slot]=$record
 try{
  if(-not $p.Start()){throw 'Owned fixture process did not start'}
  $record.started=$true;$record.pid=$p.Id
  $record.stdout=$p.StandardOutput.ReadToEndAsync();$record.stderr=$p.StandardError.ReadToEndAsync()
  $record.creation_ticks=$p.StartTime.ToUniversalTime().Ticks
 }catch{$record.setup_error=$_.Exception.Message;throw}
 $record
}
function Collect-Owned($v,[string]$name,[string]$dir){
 if($null -eq $v){return $null}
 $p=$v.process
 $r=@{pid=$v.pid;creation_ticks=$v.creation_ticks;expected_executable=$v.executable;started=$v.started;setup_error=$v.setup_error;exit_code=$null;exited_confirmed=$false;timed_out=$false;streams_complete=$false;stdout=$null;stderr=$null;collection_ok=$false;collection_errors=@()}
 try{
  if($v.started){
   $r.timed_out=-not $p.WaitForExit(15000)
   if($r.timed_out){
    try{if(-not $p.HasExited){$p.Kill()}}catch{if(-not $p.HasExited){$r.collection_errors+=$_.Exception.Message}}
    $null=$p.WaitForExit(5000)
   }
   $r.exited_confirmed=$p.HasExited
   if($r.exited_confirmed){$r.exit_code=$p.ExitCode}
   $complete=$true
   foreach($stream in @('stdout','stderr')){
    $task=$v[$stream]
    if($null -eq $task){$complete=$false;$r[$stream]='[stream task not created]';continue}
    try{if($task.Wait(3000)){$r[$stream]=$task.Result}else{$complete=$false;$r[$stream]='[stream remains open]'}}catch{$complete=$false;$r.collection_errors+=$_.Exception.Message;$r[$stream]='[stream read failed]'}
   }
   $r.streams_complete=$complete
  }
 }catch{$r.collection_errors+=$_.Exception.Message}
 finally{try{$p.Dispose()}catch{$r.collection_errors+=$_.Exception.Message}}
 $r.collection_ok=$r.started -and $null -eq $r.setup_error -and $r.exited_confirmed -and -not $r.timed_out -and $r.streams_complete -and $r.collection_errors.Count -eq 0
 try{Save-Json $r (Join-Path $dir ($name+'-launch.json'))}catch{$r.collection_ok=$false;$r.collection_errors+=('record write: '+$_.Exception.Message)}
 $r
}
$run=New-PocDirectory (Resolve-ValidationDir 'file-ole' ([guid]::NewGuid().ToString('N')))
$kind=if($OleCloseWhileDragging){'real-native-ole-active-close-slice'}else{'real-native-ole-file-slice'}
$summary=@{kind=$kind;expected_cases=1;cases=@();success=$false;error=$null;task_12_3_accepted=$false;ole_input_approved=$true;ordinary_source_foreground_approved=[bool]$OleSourceForegroundApproved;manual_source_focus_approved=[bool]$OleManualSourceFocusApproved;close_while_dragging=[bool]$OleCloseWhileDragging;physical_input_sent=$null}
$oleProcesses=@{source=$null;driver=$null};$sourceLaunch=$null;$driverLaunch=$null
$oldCef=$env:CEF_PATH;$oldTarget=$env:CARGO_TARGET_DIR
try{
 $cef=Resolve-CefPath $CefPath;$env:CEF_PATH=$cef;$env:CARGO_TARGET_DIR=Resolve-TargetDir $TargetDir
 Push-Location $PocRoot
 try{
  $ErrorActionPreference='Continue'
  $raw=@(& cargo build --locked --manifest-path validation/component-host/Cargo.toml --target $PocTarget --features file-drag-events --bin file-drag-events --message-format=json-render-diagnostics 2>&1|Tee-Object -FilePath (Join-Path $run 'host-build.log')|ForEach-Object{Write-Host $_;$_});$code=$LASTEXITCODE;$ErrorActionPreference='Stop';if($code -ne 0){throw "OLE target build failed $code"}
  $ErrorActionPreference='Continue'
  $hr=@(& cargo build --locked --target $PocTarget -p rust-cef-helper --bin rust-cef-helper --message-format=json-render-diagnostics 2>&1|Tee-Object -FilePath (Join-Path $run 'helper-build.log')|ForEach-Object{Write-Host $_;$_});$code=$LASTEXITCODE;$ErrorActionPreference='Stop';if($code -ne 0){throw "OLE helper build failed $code"}
 }finally{$ErrorActionPreference='Stop';Pop-Location}
 $hm=@();foreach($l in $raw){if($l -is [string] -and $l.StartsWith('{')){$hm+=($l|ConvertFrom-Json)}};$helpers=@();foreach($l in $hr){if($l -is [string] -and $l.StartsWith('{')){$helpers+=($l|ConvertFrom-Json)}}
 $hostArtifact=Artifact $hm 'file-drag-events';$helperArtifact=Artifact $helpers 'rust-cef-helper';$bundle=Join-Path $run '发行 native OLE bundle with spaces'
 & (Join-Path $PSScriptRoot 'package-component.ps1') -HostExecutable $hostArtifact -HelperExecutable $helperArtifact -CefPath $cef -OutputDirectory $bundle -HostBuildVersion 'file-ole-test-support'
 $hostFile=Join-Path $bundle 'file-drag-events.exe';$helperFile=Join-Path $bundle 'rust-cef-helper.exe'
 Save-Json @{host_artifact=$hostArtifact;helper_artifact=$helperArtifact;host_sha256=(Get-FileHash $hostFile).Hash;helper_sha256=(Get-FileHash $helperFile).Hash} (Join-Path $run 'artifact-identity.json')
 $case=Join-Path $run 'Threaded-Software';$cwd=Join-Path $case 'unrelated cwd';$control=Join-Path $case 'ole-control';New-Item $cwd,$control -ItemType Directory -Force|Out-Null
 $gate=Join-Path $case 'observer.release';$report=Join-Path $case 'probe.json';$plan=Join-Path $control 'ole-target-ready.json';$nonce=Get-Random -Minimum 1 -Maximum 2147483647
 $legacy=Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe'
 $python=(& python -c 'import sys; print(sys.executable)');if($LASTEXITCODE -ne 0 -or $python -isnot [string] -or -not [IO.Path]::IsPathRooted($python) -or -not (Test-Path -LiteralPath $python -PathType Leaf)){throw 'One actual absolute Python executable required'}
 $startOwned=${function:Start-Owned};$sourceScript=Join-Path $PSScriptRoot 'component-file-ole-source.ps1';$driverScript=Join-Path $PSScriptRoot 'component-file-ole-driver.py'
 $before={param($targetPid)
  $sourceArgs=@('-NoProfile','-STA','-File',$sourceScript,'-Plan',$plan,'-OleInputApproved')
  if($OleCloseWhileDragging){$sourceArgs+='-OleCloseWhileDragging'}
  if($OleManualSourceFocusApproved){
   $sourceArgs+='-OleManualSourceFocusApproved'
   Write-Host 'Two test windows: select the teal OLE SOURCE - select this window by its title bar or taskbar. Do NOT select OLE TARGET - do not click. Release the mouse and leave both windows alone.'
  }
  $oleProcesses.source=& $startOwned $legacy $sourceArgs $cwd $oleProcesses 'source'
  $driverArgs=@($driverScript,'--control',$control,'--source-pid',[string]$oleProcesses.source.pid,'--target-pid',[string]$targetPid,'--nonce',[string]$nonce,'--source-executable',$legacy,'--target-executable',$hostFile,'--ole-input-approved')
  if($OleCloseWhileDragging){$driverArgs+='--close-while-dragging'}
  if($OleSourceForegroundApproved){$driverArgs+='--ordinary-source-foreground-approved'}
  if($OleManualSourceFocusApproved){$driverArgs+='--manual-source-focus-approved'}
  $oleProcesses.driver=& $startOwned $python $driverArgs $cwd $oleProcesses 'driver'
 }.GetNewClosure()
 $targetArgs=@('--mode','Threaded','--backend','Software','--report',$report,'--helper',$helperFile,'--observer-gate',$gate,'--input-source','ole','--ole-input-approved','true','--ole-control',$control,'--ole-nonce',[string]$nonce)
 if($OleCloseWhileDragging){$targetArgs+=@('--ole-close-while-dragging','true')}
 if($OleManualSourceFocusApproved){$targetArgs+=@('--ole-manual-source-focus','true')}
 $launch=Invoke-Probe $hostFile $targetArgs $cwd $gate $helperFile $before
 Save-Json $launch (Join-Path $case 'launch.json')
 $driverLaunch=Collect-Owned $oleProcesses.driver 'ole-driver' $control;$oleProcesses.driver=$null
 $sourceLaunch=Collect-Owned $oleProcesses.source 'ole-source' $control;$oleProcesses.source=$null
 $p=if(Test-Path $report){Get-Content $report -Raw -Encoding utf8|ConvertFrom-Json}else{$null};$d=if(Test-Path (Join-Path $control 'ole-driver-result.json')){Get-Content (Join-Path $control 'ole-driver-result.json') -Raw -Encoding utf8|ConvertFrom-Json}else{$null};$s=if(Test-Path (Join-Path $control 'ole-source-result.json')){Get-Content (Join-Path $control 'ole-source-result.json') -Raw -Encoding utf8|ConvertFrom-Json}else{$null}
 $passed=$launch.exit_code -eq 0 -and -not $launch.timed_out -and $launch.streams_complete -and $launch.image_verified -and @($launch.remaining_processes).Count -eq 0 -and $null -ne $p -and $p.success -and $p.real_win32_or_ole_verified -and -not $p.platform_events_injected -and $null -ne $d -and $d.success -and $null -ne $s -and $null -eq $s.error -and $null -ne $driverLaunch -and $driverLaunch.exit_code -eq 0 -and $driverLaunch.collection_ok -and $driverLaunch.streams_complete -and -not $driverLaunch.timed_out -and $null -ne $sourceLaunch -and $sourceLaunch.collection_ok -and $sourceLaunch.exit_code -eq 0 -and $sourceLaunch.streams_complete -and -not $sourceLaunch.timed_out
 if($OleCloseWhileDragging){
  $passed=$passed -and $p.kind -ceq $kind -and $p.ole_active_close_verified -is [bool] -and $p.ole_active_close_verified -and
      $p.raw_platform_submit_verified -is [bool] -and -not $p.raw_platform_submit_verified -and $d.close_while_dragging -is [bool] -and $d.close_while_dragging -and
      $s.effect -eq 0 -and $s.terminal_action -ceq 'Cancel' -and $s.terminal_cancel_on_release -is [bool] -and $s.terminal_cancel_on_release -and
      $null -eq $d.possibly_held -and ($p.drag_retirement | ConvertTo-Json -Compress -Depth 10) -ceq ($d.active_close.retirement | ConvertTo-Json -Compress -Depth 10)
 }else{$passed=$passed -and $p.kind -ceq $kind -and $s.effect -eq 1}
 if($OleManualSourceFocusApproved){
  $passed=$passed -and $d.manual_source_focus_approved -is [bool] -and $d.manual_source_focus_approved -and
      $d.manual_source_focus_observed -is [bool] -and $d.manual_source_focus_observed -and
      $null -ne $d.manual_gesture_ready -and $d.manual_gesture_ready.pid -eq $launch.pid -and $d.manual_gesture_ready.nonce -eq $nonce -and
      @($p.observations|Where-Object { $_.label -ceq 'manual-ole-target-gesture-ready' }).Count -eq 1 -and
      $d.foreground_requested -is [bool] -and -not $d.foreground_requested -and $d.foreground_attempts -eq 0 -and $null -eq $d.foreground_fence
 }
 if($null -ne $d -and $d.input_sent -is [bool]){$summary.physical_input_sent=$d.input_sent}
 $summary.cases+=@{mode='Threaded';backend='Software';pid=$launch.pid;exit_code=$launch.exit_code;passed=$passed;observed=$p;driver=$d;source=$s;source_launch=$sourceLaunch;driver_launch=$driverLaunch;image_verified=$launch.image_verified;remaining_processes=$launch.remaining_processes}
 if(-not $passed){throw 'Real OLE slice failed; no implicit retry'};$summary.success=$true
}catch{$summary.error=$_.Exception.Message;Write-Host "Real OLE failure: $($summary.error)"}
finally{
 foreach($role in @('driver','source')){
  if($null -ne $oleProcesses[$role]){
   try{$collected=Collect-Owned $oleProcesses[$role] ('ole-'+$role) $control;$summary[($role+'_exceptional_cleanup')]=$collected}
   catch{$summary.success=$false;if($null -eq $summary.error){$summary.error='owned cleanup: '+$_.Exception.Message};Write-Host "Owned $role cleanup failed: $($_.Exception.Message)"}
  }
 }
 $env:CEF_PATH=$oldCef;$env:CARGO_TARGET_DIR=$oldTarget
 Save-Json $summary (Join-Path $run 'result.json');Write-Host "Evidence: $run"
}
if(-not $summary.success){exit 1}
