[CmdletBinding()]
param([string]$CefPath,[string]$TargetDir,[ValidateSet('Integrated','Threaded')][string]$Mode='Threaded',[ValidateSet('Software','D3d11')][string]$Backend='Software',[ValidateRange(1,900)][int]$TimeoutSeconds=180)
# Explicit validation feature/bin. Real GPUI + injected events, NOT physical/OLE.
$ErrorActionPreference='Stop';Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'common.ps1')
. (Join-Path $PSScriptRoot 'component-process-observer.ps1')
. (Join-Path $PSScriptRoot 'component-probe-launcher.ps1')
function Save-Json($v,[string]$p){[IO.File]::WriteAllText($p,($v|ConvertTo-Json -Depth 30),[Text.UTF8Encoding]::new($false))}
function Artifact($Messages,[string]$Name){$rows=@($Messages|Where-Object{$_.reason -eq 'compiler-artifact' -and $_.target.name -eq $Name -and $_.executable});if($rows.Count -ne 1){throw 'Actual executable artifact not unique'};[IO.Path]::GetFullPath($rows[0].executable)}
$run=New-PocDirectory (Resolve-ValidationDir 'file-drag-events' ([guid]::NewGuid().ToString('N')))
$summary=@{kind='real-gpui-injected-file-events';expected_cases=1;cases=@();success=$false;error=$null;task_12_3_accepted=$false;foreground_requested=$false;real_win32_or_ole_verified=$false;raw_platform_submit_verified=$false}
$oldCef=$env:CEF_PATH;$oldTarget=$env:CARGO_TARGET_DIR
try{
 $cef=Resolve-CefPath $CefPath;$env:CEF_PATH=$cef;$env:CARGO_TARGET_DIR=Resolve-TargetDir $TargetDir
 Push-Location $PocRoot
 try{
  $ErrorActionPreference='Continue'
  $raw=@(& cargo build --locked --manifest-path validation/component-host/Cargo.toml --target $PocTarget --features file-drag-events --bin file-drag-events --message-format=json-render-diagnostics 2>&1|Tee-Object -FilePath (Join-Path $run 'host-build.log')|ForEach-Object{Write-Host $_;$_})
  $code=$LASTEXITCODE;$ErrorActionPreference='Stop';if($code -ne 0){throw "Event host build failed $code"}
  $ErrorActionPreference='Continue'
  $helperRaw=@(& cargo build --locked --target $PocTarget -p rust-cef-helper --bin rust-cef-helper --message-format=json-render-diagnostics 2>&1|Tee-Object -FilePath (Join-Path $run 'helper-build.log')|ForEach-Object{Write-Host $_;$_})
  $code=$LASTEXITCODE;$ErrorActionPreference='Stop';if($code -ne 0){throw "Event helper build failed $code"}
 }finally{$ErrorActionPreference='Stop';Pop-Location}
 $messages=@();foreach($line in $raw){if($line -is [string] -and $line.StartsWith('{')){$messages+=($line|ConvertFrom-Json)}}
 $helpers=@();foreach($line in $helperRaw){if($line -is [string] -and $line.StartsWith('{')){$helpers+=($line|ConvertFrom-Json)}}
 $hostArtifact=Artifact $messages 'file-drag-events';$helperArtifact=Artifact $helpers 'rust-cef-helper'
 $bundle=Join-Path $run '发行 GPUI event bundle with spaces'
 & (Join-Path $PSScriptRoot 'package-component.ps1') -HostExecutable $hostArtifact -HelperExecutable $helperArtifact -CefPath $cef -OutputDirectory $bundle -HostBuildVersion 'file-drag-events-validation'
 $hostFile=Join-Path $bundle ([IO.Path]::GetFileName($hostArtifact));$helperFile=Join-Path $bundle ([IO.Path]::GetFileName($helperArtifact))
 Save-Json @{host_artifact=$hostArtifact;helper_artifact=$helperArtifact;host_sha256=(Get-FileHash $hostFile).Hash;helper_sha256=(Get-FileHash $helperFile).Hash} (Join-Path $run 'artifact-identity.json')
 $case=Join-Path $run ($Mode+'-'+$Backend);$cwd=Join-Path $case 'unrelated cwd';New-Item $cwd -ItemType Directory -Force|Out-Null;$report=Join-Path $case 'probe.json';$gate=Join-Path $case 'observer.release'
 $launch=Invoke-Probe $hostFile @('--mode',$Mode,'--backend',$Backend,'--report',$report,'--helper',$helperFile,'--observer-gate',$gate) $cwd $gate $helperFile
 Save-Json $launch (Join-Path $case 'launch.json')
 $observed=if(Test-Path $report){Get-Content $report -Raw -Encoding utf8|ConvertFrom-Json}else{$null}
 $passed=$launch.exit_code -eq 0 -and -not $launch.timed_out -and $launch.streams_complete -and $launch.image_verified -and @($launch.remaining_processes).Count -eq 0 -and $null -ne $observed -and $observed.success
 $summary.cases+=@{mode=$Mode;backend=$Backend;pid=$launch.pid;exit_code=$launch.exit_code;passed=$passed;observed=$observed;image_verified=$launch.image_verified;remaining_processes=$launch.remaining_processes}
 if(-not $passed){throw 'Injected GPUI event slice failed; preserve first failure and stop'}
 $summary.success=$true
}catch{$summary.error=$_.Exception.Message;Write-Host "GPUI event fixture failure: $($summary.error)"}
finally{$env:CEF_PATH=$oldCef;$env:CARGO_TARGET_DIR=$oldTarget;Save-Json $summary (Join-Path $run 'result.json');Write-Host "Evidence: $run"}
if(-not $summary.success){exit 1}
