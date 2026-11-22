# Metadata-only classifier regression. This never launches a CEF process.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'component-process-observer.ps1')
$helper = 'C:\owned bundle\helper.exe'
$renderer = @{ role = 'renderer'; executable_path = $helper; pid = 11; parent_pid = 10 }
if (-not (Test-ObservedChildImages @($renderer) $helper)) { throw 'complete matching renderer must be accepted' }
$unknown = @{ role = $null; executable_path = $helper; pid = 12; parent_pid = 10 }
if (Test-ObservedChildImages @($renderer, $unknown) $helper) { throw 'unclassified child must make observation incomplete' }
$wrong = @{ role = 'renderer'; executable_path = 'C:\owned bundle\host.exe'; pid = 13; parent_pid = 10 }
if (Test-ObservedChildImages @($wrong) $helper) { throw 'host fallback must be rejected' }
if (Test-ObservedChildImages @() $helper) { throw 'empty observations must fail' }
$survivors = @(Get-ObservedSurvivors @(@{ pid = 11; executable_path = $helper }) $helper)
if ($survivors.Count -ne 1 -or $survivors[0] -ne 11) { throw 'actual owned survivor must be reported' }
$unreadableRejected = $false
try { Get-ObservedSurvivors @(@{ pid = 12; executable_path = $null }) $helper | Out-Null } catch { $unreadableRejected = $true }
if (-not $unreadableRejected) { throw 'unreadable same-name candidate cannot mean zero survivors' }
$currentState = Get-ObservedProcessState $PID
if ($currentState.exited_confirmed -or [string]::IsNullOrEmpty($currentState.executable_path)) { throw 'own process must have an actual readable image' }
$identity = [Diagnostics.Process]::GetCurrentProcess()
try { $wrongBirth = $identity.StartTime.ToUniversalTime().Ticks + [TimeSpan]::TicksPerSecond } finally { $identity.Dispose() }
$mismatch = Get-ObservedProcessState $PID 0 $wrongBirth
if ($mismatch.exited_confirmed -or $null -ne $mismatch.executable_path -or [string]::IsNullOrEmpty($mismatch.error)) { throw 'PID identity mismatch must remain unknown, never exited or verified' }
$childInfo = New-Object Diagnostics.ProcessStartInfo
$childInfo.FileName = $env:ComSpec
$childInfo.Arguments = '/d /c exit 0'
$childInfo.UseShellExecute = $false
$child = [Diagnostics.Process]::Start($childInfo)
try {
    if (-not $child.WaitForExit(5000)) { $child.Kill(); throw 'owned exit fixture timed out' }
    $gone = Get-ObservedProcessState $child.Id
    if (-not $gone.exited_confirmed) { throw 'terminated owned process must be confirmed exited, not unknown' }
} finally { $child.Dispose() }
$deadRenderer = @{ role = 'renderer'; executable_path = $helper; exited_confirmed = $true; pid = 14 }
if (Test-ObservedChildImages @($deadRenderer) $helper) { throw 'an exited renderer is not live image evidence' }
$history = @(@{ children = @($wrong, $deadRenderer) }, @{ children = @($renderer) })
if (Test-ObservedSnapshotHistory $history $helper) { throw 'a later correct snapshot cannot erase observed live host fallback' }
if (-not (Test-ObservedSnapshotHistory @(@{children=@($renderer,$deadRenderer)}, @{children=@($renderer)}) $helper)) { throw 'confirmed exited member may trigger a fresh correct snapshot only when all live members were valid' }
Write-Output 'OBSERVER_CLASSIFIER_RESULT=ok cases=12 metadata_and_owned_os_fixture=true'
