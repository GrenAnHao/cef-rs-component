[CmdletBinding()]
param(
    [ValidateSet('Integrated','Threaded','All')][string]$Mode = 'All',
    [ValidateSet('Software','D3d11','All')][string]$Backend = 'All',
    [ValidateSet('Embed','Dispatch','ProfileDispatch','PaintLatency','Popup','Input','ImeObserve','Resize','Unfocused','Exhaustion','Visibility','SourceViewer','FindView','MenuDefault')][string]$Scenario = 'Embed',
    [switch]$InteractiveInputApproved,
    [switch]$ManualForeground,
    [switch]$InitialForegroundApproved,
    [switch]$ExhaustionManualForeground,
    [switch]$VisibilityOperationsApproved,
    [switch]$VisibilityManualForeground,
    [switch]$ResizeCaptureEvidence,
    [switch]$ResizeManualForeground,
    [switch]$ResizePassive,
    [switch]$ResizePassiveMatrix,
    [switch]$ResizePassiveRight,
    [switch]$ResizeExhaustion,
    [switch]$ResizeVerifiedTransitions,
    [switch]$MenuServicesProbe,
    [switch]$MenuForegroundApproved,
    [switch]$MenuDismissProbe,
    [switch]$MenuDevtoolsProbe,
    [switch]$MenuFindProbe,
    [switch]$MenuSourceProbe,
    [switch]$MenuEditProbe,
    [switch]$MenuSpellingProbe,
    [switch]$MenuStructureProbe,
    [switch]$MenuSubmenuProbe,
    [string]$SpellingDictionaryFixture,
    [string]$SpellingDictionaryLicense,
    [switch]$StageBundleInTemp,
    [switch]$TimingHostNoPdb,
    [switch]$TimingProfilePeer,
    [switch]$TimingProfileWindows,
    [switch]$TimingMeasureBoth,
    [switch]$TimingReadbackAdapter,
    [switch]$TimingRendererAdapter,
    [switch]$TimingWindowPressure,
    [switch]$TimingWindowMenuPressure,
    [switch]$TimingWindowMenuFlushPressure,
    [switch]$TimingExtraFlushFault,
    [switch]$TimingWindowSourcePressure,
    [switch]$TimingWindowFindPressure,
    [switch]$TimingWindowEventPressure,
    [switch]$TimingWindowPrintPressure,
    [string]$CefPath,
    [string]$TargetDir,
    [ValidateRange(30,900)][int]$TimeoutSeconds = 120
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$isolatedStagingScenarios=@('PaintLatency','SourceViewer','FindView','MenuDefault')
if($TimingHostNoPdb -and ($Scenario -notin $isolatedStagingScenarios -or -not $StageBundleInTemp)){throw 'TimingHostNoPdb requires a passive timing, source or Find scenario with isolated temporary staging'}
if($StageBundleInTemp -and $Scenario -notin $isolatedStagingScenarios){throw 'StageBundleInTemp is limited to PaintLatency, SourceViewer, FindView or MenuDefault'}
if($TimingExtraFlushFault -and ($Scenario -ne 'PaintLatency' -or -not $TimingWindowMenuFlushPressure -or $Mode -ne 'Integrated' -or $Backend -ne 'Software' -or $InteractiveInputApproved)){throw 'TimingExtraFlushFault requires passive Integrated Software flush pressure'}
if ($TimingRendererAdapter -and -not $TimingReadbackAdapter) { throw 'TimingRendererAdapter requires TimingReadbackAdapter' }
if ($TimingReadbackAdapter -and ($Scenario -ne 'PaintLatency' -or -not $TimingMeasureBoth -or $Mode -ne 'All' -or $Backend -ne 'D3d11' -or $InteractiveInputApproved)) {
    throw 'TimingReadbackAdapter requires passive PaintLatency, TimingMeasureBoth, Mode All and Backend D3d11'
}
if($Scenario -eq 'PaintLatency' -and ($InteractiveInputApproved -or (-not $TimingExtraFlushFault -and -not $TimingReadbackAdapter -and ($Mode -ne 'All' -or $Backend -ne 'All')))){throw 'PaintLatency requires Mode All and Backend All without interactive input'}
if($TimingProfilePeer -and ($Scenario -ne 'PaintLatency' -or -not $StageBundleInTemp)){throw 'TimingProfilePeer requires passive PaintLatency with isolated temporary staging'}
if($TimingProfileWindows -and -not $TimingProfilePeer){throw 'TimingProfileWindows requires TimingProfilePeer'}
if($TimingMeasureBoth -and (-not $TimingProfileWindows -or $TimingWindowPressure)){throw 'TimingMeasureBoth requires passive two-window timing without pressure'}
if($TimingWindowPressure -and -not $TimingProfileWindows){throw 'TimingWindowPressure requires two passive timing windows'}
if($TimingWindowMenuPressure -and -not $TimingWindowPressure){throw 'TimingWindowMenuPressure requires passive two-window pressure'}
if($TimingWindowMenuFlushPressure -and -not $TimingWindowMenuPressure){throw 'TimingWindowMenuFlushPressure requires passive menu window pressure'}
if($TimingWindowSourcePressure -and (-not $TimingWindowMenuFlushPressure -or $TimingExtraFlushFault)){throw 'TimingWindowSourcePressure requires passive flush pressure without fault'}
if($TimingWindowFindPressure -and -not $TimingWindowSourcePressure){throw 'TimingWindowFindPressure requires passive Source pressure'}
if($TimingWindowEventPressure -and -not $TimingWindowFindPressure){throw 'TimingWindowEventPressure requires passive Find pressure'}
if($TimingWindowPrintPressure -and (-not $TimingWindowEventPressure -or $TimingExtraFlushFault)){throw 'TimingWindowPrintPressure requires passive mixed-event pressure without an extra flush fault'}
if($Scenario -eq 'ProfileDispatch' -and $InteractiveInputApproved){throw 'ProfileDispatch is a passive pixels-only fixture'}
# MenuDefault is no longer pinned to one cell: the owner approved running the
# same real-click slice on both modes and both required backends. Desktop input
# approval is still mandatory, and each invocation still runs exactly one cell.
if($Scenario -eq 'MenuDefault' -and -not $InteractiveInputApproved){throw 'MenuDefault requires InteractiveInputApproved for whichever single mode/backend cell is selected'}
if($MenuForegroundApproved){
    # MenuDefault, SourceViewer and FindView may take the same approved ordinary
    # foreground request, because their readiness gate needs the owner window
    # active while their drivers never activate on their own. The menu cell is
    # free across modes/backends; the approval remains per-invocation.
    $menuForegroundScenarios=@('MenuDefault','SourceViewer','FindView')
    if($Scenario -notin $menuForegroundScenarios){throw 'MenuForegroundApproved is menu/source/find-only'}
    if($Scenario -eq 'MenuDefault' -and -not $InteractiveInputApproved){
        throw 'MenuForegroundApproved for MenuDefault requires InteractiveInputApproved'
    }
}
if($MenuFindProbe -and $Scenario -ne 'MenuDefault'){throw 'MenuFindProbe requires MenuDefault'}
if($MenuStructureProbe -and ($Scenario -ne 'MenuDefault' -or -not $InteractiveInputApproved)){throw 'MenuStructureProbe requires MenuDefault and InteractiveInputApproved'}
if($MenuSubmenuProbe -and ($Scenario -ne 'MenuDefault' -or -not $InteractiveInputApproved)){throw 'MenuSubmenuProbe requires MenuDefault and InteractiveInputApproved'}
if($MenuSpellingProbe -and ($Scenario -ne 'MenuDefault' -or -not $InteractiveInputApproved -or -not $SpellingDictionaryFixture -or -not $SpellingDictionaryLicense)){throw 'MenuSpellingProbe requires MenuDefault, InteractiveInputApproved and the dictionary fixture files'}
if($MenuEditProbe -and ($Scenario -ne 'MenuDefault' -or -not $InteractiveInputApproved)){throw 'MenuEditProbe requires MenuDefault with InteractiveInputApproved'}
if($MenuSourceProbe -and ($Scenario -ne 'MenuDefault' -or -not $InteractiveInputApproved)){throw 'MenuSourceProbe requires MenuDefault with InteractiveInputApproved'}
if($MenuDevtoolsProbe -and ($Scenario -ne 'MenuDefault' -or -not $InteractiveInputApproved)){throw 'MenuDevtoolsProbe requires MenuDefault with InteractiveInputApproved'}
if($MenuDismissProbe -and ($Scenario -ne 'MenuDefault' -or $Mode -ne 'Threaded' -or $Backend -ne 'Software' -or -not $InteractiveInputApproved -or -not $MenuForegroundApproved)){throw 'MenuDismissProbe requires fresh approved MenuDefault Threaded Software mouse/Escape case'}
if ($Scenario -in @('SourceViewer','FindView')) {
    if (-not $PSBoundParameters.ContainsKey('Mode') -or -not $PSBoundParameters.ContainsKey('Backend') -or
        (($Mode -eq 'All') -xor ($Backend -eq 'All')) -or $InteractiveInputApproved) {
        throw "$Scenario requires one explicit mode/backend cell or Mode All and Backend All, without desktop input"
    }
}
if($MenuServicesProbe -and ($Scenario -ne 'Embed' -or $Mode -ne 'Threaded' -or $Backend -ne 'Software')){throw 'MenuServicesProbe requires explicit Embed Threaded Software'}
if($ManualForeground -and $Scenario -ne 'Input'){throw 'ManualForeground setup is Input-only'}
if($InitialForegroundApproved -and $Scenario -ne 'Exhaustion'){throw 'InitialForegroundApproved is Exhaustion-only'}
if($InitialForegroundApproved -and ($Mode -eq 'All' -or $Backend -eq 'All')){throw 'InitialForegroundApproved requires one explicit mode/backend cell'}
if($ExhaustionManualForeground -and ($Scenario -ne 'Exhaustion' -or $Mode -eq 'All' -or $Backend -eq 'All' -or $InitialForegroundApproved)){
    throw 'ExhaustionManualForeground requires one explicit Exhaustion cell without InitialForegroundApproved'
}
if($VisibilityManualForeground -and ($Scenario -ne 'Visibility' -or -not $VisibilityOperationsApproved -or $Mode -eq 'All' -or $Backend -eq 'All')){throw 'VisibilityManualForeground requires one explicit approved Visibility cell'}
if($VisibilityOperationsApproved -and $Scenario -ne 'Visibility'){throw 'VisibilityOperationsApproved is Visibility-only'}
if($Scenario -eq 'Visibility' -and (-not $VisibilityOperationsApproved -or $Mode -eq 'All' -or $Backend -eq 'All')){throw 'Visibility requires explicit window approval and one mode/backend cell'}
if($ResizePassiveMatrix -and (-not $ResizePassive -or $Mode -ne 'All' -or $Backend -ne 'All' -or $ResizeCaptureEvidence)){throw 'ResizePassiveMatrix requires ResizePassive, Mode All, Backend All and uses its own D3d11 diagnostics'}
if($ResizePassive -and ($Scenario -ne 'Resize' -or (-not $ResizePassiveMatrix -and ($Mode -eq 'All' -or $Backend -eq 'All')) -or $InteractiveInputApproved -or $ResizeManualForeground)){throw 'ResizePassive requires one non-interactive Resize cell without manual foreground, or an explicit ResizePassiveMatrix'}
if($ResizePassiveRight -and -not $ResizePassive){throw 'ResizePassiveRight requires ResizePassive'}
if($ResizeVerifiedTransitions -and (-not $ResizePassive -or (-not $ResizePassiveMatrix -and (-not $ResizeCaptureEvidence -or $Backend -ne 'D3d11')))){throw 'ResizeVerifiedTransitions requires passive D3d11 capture evidence or passive matrix'}
if($ResizeManualForeground -and -not $ResizeCaptureEvidence){throw 'ResizeManualForeground requires ResizeCaptureEvidence'}
if($ResizeCaptureEvidence -and ($Scenario -ne 'Resize' -or $Backend -ne 'D3d11' -or $Mode -eq 'All' -or (-not $InteractiveInputApproved -and -not $ResizePassive))){throw 'ResizeCaptureEvidence requires one explicitly selected passive or approved Resize D3d11 cell'}
if($Scenario -in @('Popup','Input','ImeObserve','Resize','Unfocused','Exhaustion') -and -not $InteractiveInputApproved -and -not $ResizePassive){throw 'InteractiveInputApproved required before input/window/owned foreground changes'}
# SourceViewer/FindView keep their no-desktop-input contract; the dedicated
# foreground approval is the only activation they may request.
. (Join-Path $PSScriptRoot 'common.ps1')
. (Join-Path $PSScriptRoot 'component-process-observer.ps1')
. (Join-Path $PSScriptRoot 'component-probe-launcher.ps1')
function Write-JsonFile($Value,[string]$Path) {
    [IO.File]::WriteAllText($Path,($Value | ConvertTo-Json -Depth 20),[Text.UTF8Encoding]::new($false))
}
function Write-PidMarkerAtomic([string]$Path,[int]$OwnedPid) {
    $temporary=$Path + '.pending'
    [IO.File]::WriteAllText($temporary,[string]$OwnedPid,[Text.UTF8Encoding]::new($false))
    [IO.File]::Move($temporary,$Path)
}
function Get-ObservationCaseCount {
    param($SelectedModes,$SelectedBackends)
    @($SelectedModes).Count * @($SelectedBackends).Count
}
function Test-ResizeVerifiedTransitions($Observed, $Stages) {
    if (@($Stages).Count -ne 6) { return $false }
    $q = $Observed.resize.verified_transitions
    if ($null -eq $q -or @($q.cuts).Count -ne 4 -or $q.native_capture_retired -isnot [bool] -or -not $q.native_capture_retired -or
        $q.owner_ui_thread -ne $Observed.cef_ui_thread -or $q.retirement_ui_thread -ne $Observed.cef_ui_thread -or
        -not (Test-TimingCount $q.validated_rejections) -or $q.validated_rejections -ne $q.native_failure_count -or
        $q.capture_events.overflow -or $q.native.native_rejections_overflow -or $q.capture_events.seen -ne $q.validated_rejections -or
        @($q.capture_events.events).Count -ne $q.validated_rejections -or @($q.native.native_rejections).Count -ne $q.validated_rejections) {
        return $false
    }
    $previous = 0
    for ($i=0; $i -lt 4; $i++) {
        $cut = $q.cuts[$i]; $before = $Stages[$i]; $after = $Stages[$i+1]
        $old = $before.metadata.view_raw; $new = $after.metadata.view_raw
        if (($cut.old_frame -join ',') -cne "$($old.generation),$($old.sequence),$($old.width),$($old.height)" -or
            ($cut.target -join ',') -cne "$($new.generation),$($new.width),$($new.height)" -or
            $cut.old_sha256 -cne $before.capture.raw.sha256 -or $cut.recovered_sha256 -cne $after.capture.raw.sha256 -or
            $cut.recovered_sequence -ne $new.sequence -or -not (Test-TimingCount $cut.ack_before) -or
            $cut.ack_after -ne ($cut.ack_before + 1) -or -not (Test-TimingCount $cut.through_rejection) -or
            $cut.through_rejection -lt $previous -or $cut.through_rejection -gt $q.validated_rejections) { return $false }
        $previous = $cut.through_rejection
    }
    return $previous -eq $q.validated_rejections
}
function Get-PassiveResizeMatrixDpi($Cases) {
    $plan = @('Integrated/Software', 'Integrated/D3d11', 'Threaded/Software', 'Threaded/D3d11')
    if ($null -eq $Cases -or @($Cases).Count -ne $plan.Count) { return $null }
    $dpi = $null
    $pids = [Collections.Generic.HashSet[int64]]::new()
    for ($i=0; $i -lt $plan.Count; $i++) {
        $c = $Cases[$i]
        if ("$($c.mode)/$($c.backend)" -cne $plan[$i] -or $c.passed -isnot [bool] -or -not $c.passed -or
            -not (Test-TimingCount $c.exit_code) -or $c.exit_code -ne 0 -or
            -not (Test-TimingCount $c.pid) -or $c.pid -le 0 -or -not $pids.Add($c.pid)) { return $null }
        $r = $c.observed
        if ($null -eq $r -or $r.pid -ne $c.pid -or $r.mode -cne $c.mode -or $r.backend -cne $c.backend -or
            $r.observed_backend -cne "Some($($c.backend)Required)" -or $null -eq $r.resize -or
            $r.resize.passive_noactivation_resize -isnot [bool] -or -not $r.resize.passive_noactivation_resize) { return $null }
        $actual = $r.resize.physical_dpi
        if (-not (Test-TimingCount $actual) -or $actual -le 0 -or ($null -ne $dpi -and $dpi -ne $actual)) { return $null }
        $dpi = $actual
    }
    return $dpi
}
function Test-TimingCount($Value) {
    return ($Value -is [int] -or $Value -is [long]) -and $Value -ge 0
}
function Test-SerialTimingLane($Rows) {
    if ($null -eq $Rows -or @($Rows).Count -eq 0) { return $false }
    $previousPaint = 0L
    foreach ($row in $Rows) {
        foreach ($value in @($row.copy_ns, $row.copy_completed_ns, $row.paint_completed_ns, $row.copy_to_paint_ns)) {
            if (-not (Test-TimingCount $value)) { return $false }
        }
        # The next invalidation follows the previous paint in this same lane.
        $copyStart = $row.copy_completed_ns - $row.copy_ns
        if ($copyStart -lt $previousPaint -or $row.paint_completed_ns -lt $row.copy_completed_ns -or
            $row.copy_to_paint_ns -ne ($row.paint_completed_ns - $row.copy_completed_ns)) { return $false }
        $previousPaint = $row.paint_completed_ns
    }
    return $true
}
function Test-SourceViewerIdentity($Probe,$Before,$After,$First,$Detached) {
    $d = $Probe.source_viewer
    if ($null -eq $d -or $null -eq $Before -or $null -eq $After) { return $false }
    $menu = @($d.source_menu); $operation = @($d.operation)
    return $menu.Count -eq 3 -and $operation.Count -eq 3 -and $menu[0] -gt 0 -and $menu[1] -gt 0 -and $menu[2] -gt 0 -and
        $operation[0] -eq $menu[0] -and $operation[1] -eq $menu[1] -and $operation[2] -gt 0 -and
        ($menu -join ',') -ceq ($Before.source_menu -join ',') -and ($menu -join ',') -ceq ($After.source_menu -join ',') -and
        ($operation -join ',') -ceq ($Before.operation -join ',') -and ($operation -join ',') -ceq ($After.operation -join ',') -and
        $Before.pid -eq $Probe.pid -and $After.pid -eq $Probe.pid -and $Before.stage -eq 0 -and $After.stage -eq 1 -and
        $Before.original.hwnd -eq $First.hwnd -and $After.original.hwnd -eq $Detached.hwnd -and $Before.viewer.hwnd -eq $After.former_viewer.hwnd -and
        $Before.viewer.hwnd -ne $Before.original.hwnd -and [single]$Before.view_scale -eq [single]($Before.original.dpi / 96) -and
        ($Before.view_pixels -join ',') -ceq (@($Probe.paint.frame_width,$Probe.paint.frame_height) -join ',') -and
        $Probe.paint.frame_width -eq [Math]::Ceiling([single](320 * [single]$Before.view_scale)) -and
        $Probe.paint.frame_height -eq [Math]::Ceiling([single](240 * [single]$Before.view_scale))
}
function Test-FindViewIdentity($Probe,$Before,$After,$First,$Detached) {
    $d=$Probe.find_view.identities
    if ($null -eq $d -or $null -eq $Before -or $null -eq $After -or $null -eq $Before.identities) { return $false }
    $initial=$Before.identities; $menus=@($d.menus)
    if($menus.Count -ne 3 -or @($d.first).Count -ne 3 -or @($d.repeat).Count -ne 3 -or $d.first_final -ne $true -or $d.query_bytes -ne 21 -or $d.first_cef_identifier -lt 0){return $false}
    $previous=0
    for($i=0;$i -lt $menus.Count;$i++){
        $m=@($menus[$i]);if($m.Count -ne 3 -or $m[0] -ne $d.first[0] -or $m[1] -ne $d.first[1] -or $m[2] -le $previous -or
            ($m -join ',') -cne ($initial.menus[$i] -join ',')){return $false}
        $previous=$m[2]
    }
    return $d.first[0] -gt 0 -and $d.first[1] -gt 0 -and $d.first[2] -eq 1 -and
        $d.repeat[0] -eq $d.first[0] -and $d.repeat[1] -eq $d.first[1] -and $d.repeat[2] -eq 2 -and
        ($d.first -join ',') -ceq ($initial.first -join ',') -and ($d.repeat -join ',') -ceq ($initial.repeat -join ',') -and
        $initial.first_final -eq $true -and $initial.query_bytes -eq $d.query_bytes -and $initial.first_cef_identifier -eq $d.first_cef_identifier -and
        ($menus[2] -join ',') -ceq ($After.retired_menu -join ',') -and ($d.repeat -join ',') -ceq ($After.retired_operation -join ',') -and
        $Before.pid -eq $Probe.pid -and $After.pid -eq $Probe.pid -and $Before.stage -eq 0 -and $After.stage -eq 1 -and
        $Before.original.hwnd -eq $First.hwnd -and $After.original.hwnd -eq $Detached.hwnd -and $First.hwnd -eq $Detached.hwnd -and
        $Before.find_ui.active -and $Before.find_ui.outcome -ceq 'None' -and $After.find_ui.outcome -ceq 'Some(Cancelled)' -and $After.browser_closed -and
        [single]$Before.view_scale -eq [single]($Before.original.dpi / 96) -and
        ($Before.view_pixels -join ',') -ceq (@($Probe.paint.frame_width,$Probe.paint.frame_height) -join ',') -and
        $Probe.paint.frame_width -eq [Math]::Ceiling([single](320 * [single]$Before.view_scale)) -and $Probe.paint.frame_height -eq [Math]::Ceiling([single](240 * [single]$Before.view_scale))
}
function Test-TimingDistribution($Raw,$Reported) {
    if ($null -eq $Raw) { return $false }
    $n=@($Raw).Count
    if($n -eq 0){return $null -eq $Reported}
    if ($null -eq $Reported) { return $false }
    foreach ($value in $Raw) { if (-not (Test-TimingCount $value)) { return $false } }
    foreach ($value in @($Reported.samples, $Reported.p50_ns, $Reported.p95_ns, $Reported.max_ns)) {
        if (-not (Test-TimingCount $value)) { return $false }
    }
    $sorted=@($Raw | Sort-Object)
    return $Reported.samples -eq $n -and $sorted[-1] -gt 0 -and
        $Reported.p50_ns -eq $sorted[[int][Math]::Ceiling($n*0.50)-1] -and
        $Reported.p95_ns -eq $sorted[[int][Math]::Ceiling($n*0.95)-1] -and $Reported.max_ns -eq $sorted[-1]
}
function Test-PaintLatencyReport($Probe,$Launch,$Window,$Mode,$Backend,[string]$HostFile,[bool]$WithPeer,[bool]$WithPressure=$false) {
    $expectedBrowsers = if ($WithPeer) { 2 } else { 1 }
    $closeOrderOk = if ($WithPressure) { $Probe.pressure_closed_before_view_release -and -not $Probe.view_release_observed_closing } else { $Probe.view_release_observed_closing }
    if ($null -eq $Window -or -not $Launch.streams_complete -or $Launch.timed_out -or $Launch.exit_code -ne 0 -or
        -not $Launch.image_verified -or $null -ne $Launch.observer_error -or @($Launch.remaining_processes).Count -ne 0) { return $false }
    $threadOk=($Mode -eq 'Integrated' -and $Probe.cef_ui_thread -eq $Probe.main_thread -and $Probe.manual_pump -gt 0) -or
        ($Mode -eq 'Threaded' -and $Probe.cef_ui_thread -ne $Probe.main_thread -and $Probe.manual_pump -eq 0)
    $actual=if($Backend -eq 'D3d11'){'Some(D3d11Required)'}else{'Some(SoftwareRequired)'}
    $t=$Probe.paint_timing
    $retired=$Probe.paint_timing_retired
    if ($null -eq $retired -or $null -eq $t -or $null -eq $Probe.paint -or $null -eq $Probe.released_view -or $t.scope -cne 'copy-completion-to-gpui-paint-image-success' -or
        -not $t.observer_released -or $t.desktop_pixels_observed -or $t.swapchain_present_observed -or
        $Probe.pid -ne $Launch.pid -or $Window.pid -ne $Launch.pid -or $Window.hwnd -le 0 -or $Window.creation_filetime -le 0 -or
        $Window.image -ine $HostFile -or $Probe.scenario -cne 'PaintLatency' -or $Probe.mode -cne $Mode -or $Probe.backend -cne $Backend -or
        $Probe.observed_backend -cne $actual -or $t.backend -cne $Backend -or -not $threadOk -or
        $Probe.main_thread -le 0 -or $Probe.cef_ui_thread -le 0 -or $Probe.init_thread -ne $Probe.main_thread -or $Probe.shutdown_thread -ne $Probe.main_thread -or
        -not $Probe.success -or $null -ne $Probe.error -or -not $Probe.finished -or -not $Probe.host_alive_after_view_drop -or -not $closeOrderOk -or
        -not $Probe.duplicate_attachment_refused -or $Probe.created -ne $expectedBrowsers -or $Probe.closed -ne $expectedBrowsers -or $Probe.wake_consumed -le 0 -or
        $Probe.borrow_violations -ne 0 -or $Probe.shutdown_calls -ne 1 -or $Probe.native_after -ne 0 -or $Probe.final_cpu_used -ne 0 -or
        $Probe.pending_tasks -ne 0 -or $Probe.pending_requests -ne 0 -or $Probe.pending_profiles -ne 0 -or $Probe.profile_refs -ne 0 -or
        $Probe.post_failures -ne 0 -or $Probe.tasks_posted -le 0 -or $Probe.tasks_posted -ne $Probe.tasks_completed -or $Probe.root_lease -or $Probe.app_retained -or
        $Probe.released_view.created_images -lt 33 -or $Probe.released_view.created_images -ne $Probe.released_view.released_images) { return $false }
    $rows=@($t.rows); $n=$rows.Count
    if ($n -ne 32 -or $t.measurement_count -ne $n -or $t.generation -le 0 -or $t.baseline_sequence -le 0 -or $t.scale -le 0 -or
        -not (Test-SerialTimingLane $rows)) { return $false }
    $prev=$t.baseline_sequence
    $w=$rows[0].gpui_window
    foreach($r in $rows) {
        if (($r.browser -join ',') -cne ($t.browser -join ',') -or $r.gpui_window -ne $w -or $r.generation -ne $t.generation -or
            $r.sequence -le $prev -or $r.width -ne $Probe.paint.frame_width -or $r.height -ne $Probe.paint.frame_height -or
            $r.capture_ui_thread -ne $Probe.cef_ui_thread) { return $false }
        $prev=$r.sequence
    }
    $pumps=@($t.pump_samples_ns)
    $pumpOk=if($Mode -eq 'Integrated'){$t.pump_calls -gt 0 -and $t.pump_calls -le $Probe.manual_pump -and $pumps.Count -eq [Math]::Min(256,$t.pump_calls)}else{$t.pump_calls -eq 0 -and $pumps.Count -eq 0}
    $copyOk=if($Backend -eq 'D3d11'){$t.software_captures -eq 0 -and $t.d3d11_captures -ge 33 -and $t.maps -eq $t.unmaps -and $t.maps -ge 33}else{$t.software_captures -ge 33 -and $t.d3d11_captures -eq 0 -and $t.maps -eq 0 -and $t.unmaps -eq 0}
    $retiredOk=$retired.retirement_thread -eq $Probe.cef_ui_thread -and $retired.maps -eq $retired.unmaps -and $retired.maps -ge $t.maps -and
        -not $retired.native_resources_live -and $retired.live_imports -eq 0 -and $retired.staging_bytes -eq 0 -and $retired.cpu_bytes -eq 0 -and
        $retired.cpu_limit_bytes -eq 268435456 -and $retired.peak_cpu_bytes -ge ($Probe.paint.frame_width*$Probe.paint.frame_height*4) -and
        $retired.peak_cpu_bytes -le $retired.cpu_limit_bytes
    return $pumpOk -and $copyOk -and $retiredOk -and $prev -eq $Probe.paint.painted_sequence -and $t.generation -eq $Probe.paint.painted_generation -and
        (Test-TimingDistribution $pumps $t.pump_ns) -and
        (Test-TimingDistribution @($rows | ForEach-Object { $_.copy_ns }) $t.copy_ns) -and
        (Test-TimingDistribution @($rows | ForEach-Object { $_.copy_to_paint_ns }) $t.copy_to_paint_ns)
}
function Test-PeerPostCloseRepaint($View) {
    $entry = $View.PSObject.Properties['paint_before_post_close_refresh']
    $sequence = $View.PSObject.Properties['source_sequence_before_post_close_refresh']
    if ($null -eq $entry -or $null -eq $entry.Value -or $null -eq $sequence -or
        -not (Test-TimingCount $sequence.Value)) { return $false }
    $cut = $entry.Value
    $before = $View.paint_before_original_close
    $after = $View.paint_after_original_close
    return $null -eq $cut.last_error -and $cut.paint_calls -gt 0 -and $after.paint_calls -gt $cut.paint_calls -and
        $cut.painted_generation -eq $before.painted_generation -and $after.painted_generation -eq $cut.painted_generation -and
        $cut.frame_width -eq $before.frame_width -and $cut.frame_height -eq $before.frame_height -and
        $cut.painted_sequence -ge $before.painted_sequence -and $sequence.Value -ge $cut.painted_sequence -and
        $after.painted_sequence -gt $sequence.Value
}
function Test-TimingProfilePeer($Probe, [string]$Backend) {
    $peer = $Probe.paint_timing_peer
    $retired = $Probe.paint_timing_peer_retired
    if ($null -eq $peer -or $null -eq $retired) { return $false }
    $p = $peer.profiles; $b = $peer.view
    $start = $b.paint_at_sampling_start; $before = $b.paint_before_original_close; $after = $b.paint_after_original_close
    if ($b.backend -cne $Backend -or -not $b.closed_after_original -or $p.is_same -or $p.is_sharing -or
        $p.native_browser_context_queried -or $p.storage_tested -or -not $p.a.ready_after_views -or -not $p.b.ready_after_views -or
        $p.a.storage -cne 'Memory' -or $p.b.storage -cne 'Persistent' -or
        ($p.a.browser -join ',') -cne ($Probe.paint_timing.browser -join ',') -or ($p.b.browser -join ',') -cne ($b.browser -join ',') -or
        $p.a.browser[0] -ne $p.b.browser[0] -or $p.a.browser[1] -le 0 -or $p.b.browser[1] -le $p.a.browser[1] -or
        $p.a.configured_profile[0] -ne $p.a.browser[0] -or $p.a.configured_profile[1] -ne 0 -or
        $p.b.configured_profile[0] -ne $p.b.browser[0] -or $p.b.configured_profile[1] -le 0) { return $false }
    foreach ($paint in @($start,$before,$after)) {
        if ($paint.paint_calls -le 0 -or $paint.painted_generation -le 0 -or $paint.painted_sequence -le 0 -or
            $paint.frame_width -ne $Probe.paint.frame_width -or $paint.frame_height -ne $Probe.paint.frame_height -or $paint.last_error) { return $false }
    }
    # Compare the peer's own counters, not A's sample lane.
    $captureOk = if ($Backend -ceq 'D3d11') {
        $b.d3d11_captures -gt 1 -and $b.software_captures -eq 0 -and $b.capture_maps -ge 2 -and $b.capture_maps -eq $b.capture_unmaps
    } else { $b.software_captures -gt 1 -and $b.d3d11_captures -eq 0 -and $b.capture_maps -eq 0 -and $b.capture_unmaps -eq 0 }
    return $captureOk -and (Test-PeerPostCloseRepaint $b) -and $start.painted_generation -eq $before.painted_generation -and
        $start.painted_sequence -le $before.painted_sequence -and
        $b.released_view.created_images -ge 2 -and $b.released_view.created_images -eq $b.released_view.released_images -and
        $retired.retirement_thread -eq $Probe.cef_ui_thread -and $retired.maps -eq $retired.unmaps -and $retired.maps -ge $b.capture_maps -and
        -not $retired.native_resources_live -and $retired.live_imports -eq 0 -and $retired.staging_bytes -eq 0 -and $retired.cpu_bytes -eq 0 -and
        $retired.cpu_limit_bytes -eq 268435456 -and $retired.peak_cpu_bytes -ge ($after.frame_width*$after.frame_height*4) -and $retired.peak_cpu_bytes -le $retired.cpu_limit_bytes
}
function Test-TimingWindowPair($Probe,$Window) {
    $pair=$Probe.paint_timing_peer.PSObject.Properties['window_pair']
    if ($null -eq $pair -or $null -eq $pair.Value) { return $false }
    $p=$pair.Value
    $expectedHwnd=switch -CaseSensitive ($Window.title) {
        'GPUI-CEF public host' { $p.a_hwnd }
        'GPUI-CEF profile timing peer' { $p.b_hwnd }
        default { 0 }
    }
    return $p.pid -eq $Probe.pid -and $p.thread -eq $Probe.main_thread -and
        $p.a_hwnd -gt 0 -and $p.b_hwnd -gt 0 -and $p.a_hwnd -ne $p.b_hwnd -and
        $expectedHwnd -eq $Window.hwnd -and $p.a_gpui -gt 0 -and $p.b_gpui -gt 0 -and $p.a_gpui -ne $p.b_gpui -and
        $p.a_gpui -eq $Probe.paint_timing.rows[0].gpui_window -and
        $p.a_dpi -eq (96*$Probe.paint_timing.scale) -and $p.b_dpi -eq $p.a_dpi -and
        $p.a_hwnd_gone -and $p.original_b_hwnd_still_owned
}
function Get-TimingAdapterKey($Value) {
    if ($Value -isnot [array] -or $Value.Count -ne 2) { return $null }
    $high, $low = $Value
    if ((($high -isnot [int]) -and ($high -isnot [long])) -or
        (($low -isnot [int]) -and ($low -isnot [long])) -or
        $high -lt [int]::MinValue -or $high -gt [int]::MaxValue -or
        $low -lt 0 -or $low -gt [uint32]::MaxValue -or ($high -eq 0 -and $low -eq 0)) { return $null }
    return "$high,$low"
}
function Get-TimingReadbackAdapter($Probe, $PreviousKey) {
    if ($null -eq $Probe -or $Probe.backend -cne 'D3d11' -or $Probe.observed_backend -cne 'Some(D3d11Required)') { return $null }
    $peer = $Probe.paint_timing_peer
    if ($null -eq $peer) { return $null }
    $measurement = $peer.PSObject.Properties['two_live_windows_b_timed_after_a']
    if ($null -eq $measurement -or $null -eq $measurement.Value) { return $null }
    $expected = $PreviousKey
    foreach ($lane in @($Probe.paint_timing, $measurement.Value)) {
        if ($null -eq $lane -or @($lane.rows).Count -ne 32) { return $null }
        foreach ($row in $lane.rows) {
            if ($null -eq $row) { return $null }
            $prop = $row.PSObject.Properties['readback_adapter_luid']
            if ($null -eq $prop) { return $null }
            $key = Get-TimingAdapterKey $prop.Value
            if ($null -eq $key) { return $null }
            if ($null -ne $expected -and $expected -cne $key) { return $null }
            $expected = $key
        }
    }
    return $expected
}
function Test-TimingRendererAdapter($Probe, [string]$ReadbackKey) {
    if (-not $ReadbackKey) { return $false }
    try {
        $proof = $Probe.paint_timing_peer.renderer_adapter
        $pair = $Probe.paint_timing_peer.window_pair
        $hwnds = @($pair.a_hwnd, $pair.b_hwnd)
        $gpuis = @($pair.a_gpui, $pair.b_gpui)
        $names = @()
        foreach ($snapshot in @($proof.before, $proof.after)) {
            if ($null -eq $snapshot -or $snapshot.PSObject.Properties['error'] -or
                $snapshot.pid -ne $Probe.pid -or $snapshot.thread -ne $Probe.main_thread -or
                $snapshot.windows -isnot [array] -or $snapshot.windows.Count -ne 2 -or
                $snapshot.dxgi -isnot [array] -or $snapshot.dxgi.Count -lt 1 -or $snapshot.dxgi.Count -gt 32) { return $false }
            $inventory = @{}
            foreach ($adapter in $snapshot.dxgi) {
                $key = Get-TimingAdapterKey $adapter.luid
                if ($null -eq $key -or $inventory.ContainsKey($key) -or $adapter.name -isnot [string] -or
                    $adapter.name.Length -eq 0 -or $adapter.software -isnot [bool]) { return $false }
                $inventory[$key] = $adapter
            }
            if (-not $inventory.ContainsKey($ReadbackKey)) { return $false }
            for ($i=0; $i -lt 2; $i++) {
                $row = $snapshot.windows[$i]
                if (-not (Test-TimingCount $row.hwnd) -or $row.hwnd -ne $hwnds[$i] -or
                    -not (Test-TimingCount $row.gpui) -or $row.gpui -ne $gpuis[$i] -or
                    $row.name -isnot [string] -or $row.name.Length -eq 0 -or
                    $row.software -isnot [bool] -or $row.software) { return $false }
                $matches = @($inventory.GetEnumerator() | Where-Object {
                    $_.Value.name -ceq $row.name -and $_.Value.software -eq $row.software
                })
                if ($matches.Count -ne 1 -or $matches[0].Key -cne $ReadbackKey) { return $false }
                $names += $row.name
            }
        }
        return $names.Count -eq 4 -and $names[0] -ceq $names[2] -and $names[1] -ceq $names[3]
    } catch { return $false }
}
function Test-BothWindowTiming($Probe, [string]$Mode, [string]$Backend) {
    $peer = $Probe.paint_timing_peer
    $entry = $peer.PSObject.Properties['two_live_windows_b_timed_after_a']
    if ($null -eq $entry -or $null -eq $entry.Value -or $null -ne $Probe.paint_timing_peer_partial) { return $false }
    $b = $entry.Value
    $view = $peer.view
    $before = $view.paint_before_original_close
    $rows = @($b.rows)
    $pumps = @($b.pump_samples_ns)
    if ($b.measurement_count -ne 32 -or $rows.Count -ne 32 -or -not (Test-SerialTimingLane $rows) -or
        $b.generation -le 0 -or $b.baseline_sequence -lt $view.paint_at_sampling_start.painted_sequence -or
        $b.generation -ne $view.paint_at_sampling_start.painted_generation -or $b.generation -ne $before.painted_generation -or
        $view.repaint_control_applied -ne 33 -or $view.released_view.created_images -lt 34 -or
        ($b.browser -join ',') -cne ($peer.profiles.b.browser -join ',')) { return $false }
    $previous = $b.baseline_sequence
    foreach ($row in $rows) {
        if (($row.browser -join ',') -cne ($b.browser -join ',') -or $row.gpui_window -ne $peer.window_pair.b_gpui -or
            $row.generation -ne $b.generation -or $row.sequence -le $previous -or
            $row.width -ne $before.frame_width -or $row.height -ne $before.frame_height -or
            $row.capture_ui_thread -ne $Probe.cef_ui_thread) { return $false }
        $previous = $row.sequence
    }
    $pumpOk = if ($Mode -ceq 'Integrated') {
        $b.pump_calls -gt 0 -and ($b.pump_calls + $Probe.paint_timing.pump_calls) -le $Probe.manual_pump -and
            $pumps.Count -eq [Math]::Min(256, $b.pump_calls)
    } else { $b.pump_calls -eq 0 -and $pumps.Count -eq 0 }
    $captures = if ($Backend -ceq 'D3d11') { $view.d3d11_captures } else { $view.software_captures }
    return $pumpOk -and $captures -ge 34 -and $before.painted_sequence -ge $previous -and
        (Test-TimingDistribution $pumps $b.pump_ns) -and (Test-TimingDistribution @($rows.copy_ns) $b.copy_ns) -and
        (Test-TimingDistribution @($rows.copy_to_paint_ns) $b.copy_to_paint_ns)
}
function Test-WindowPressure($Probe,[bool]$WithMenu=$false,[bool]$WithMixed=$false) {
    $entry=$Probe.paint_timing_peer.PSObject.Properties['window_pressure']
    if ($null -eq $entry -or $null -eq $entry.Value) { return $false }
    $w=$entry.Value
    $a=$Probe.paint_timing_peer.profiles.a.browser; $b=$Probe.paint_timing_peer.profiles.b.browser
    $img=$w.a_terminal_images_created_released_before_released_after
    $titles=if($WithMixed){252}else{256}
    return @($img).Count -eq 3 -and $img[0] -gt $img[1] -and $img[0] -eq $img[2] -and $img[2] -eq $Probe.released_view.released_images -and
        $Probe.released_view.binding_retired -and $Probe.pressure_closed_before_view_release -and -not $Probe.view_release_observed_closing -and ($w.a_browser -join ',') -ceq ($a -join ',') -and ($w.b_browser -join ',') -ceq ($b -join ',') -and
        $w.command_thread -eq $Probe.cef_ui_thread -and $w.accepted_commands -eq 256 -and $w.command_pending_at_close -eq 256 -and $w.dispatched_commands -eq 256 -and
        $w.menu_pending_at_close -eq [int]$WithMenu -and $w.overflow_refused -and $w.new_command_refused_after_intent -and
        @($w.close_task).Count -eq 2 -and $w.close_task[0] -eq $a[0] -and $w.close_task[1] -gt 0 -and $w.close_task[1] -le $Probe.tasks_completed -and
        $w.retained_title_events -eq $titles -and $w.event_depth_at_a_closed -eq 256 -and $w.resynced_title_before_close -eq ($titles+1) -and
        @($w.closed_events).Count -eq 2 -and ($w.closed_events[0][0] -join ',') -ceq ($a -join ',') -and ($w.closed_events[1][0] -join ',') -ceq ($b -join ',') -and
        $w.first_title_sequence -gt 0 -and $w.first_title_sequence -lt $w.last_retained_title_sequence -and $w.last_retained_title_sequence -lt $w.rejected_title_sequence -and
        $w.rejected_title_sequence -lt $w.closed_events[0][1] -and $w.closed_events[0][1] -lt $w.closed_events[1][1] -and
        $w.a_b_hwnds_live_after_first_closed -and $w.a_view_consumed_terminal_in_original_window
}
function Test-WindowPressureMenu($Probe,[bool]$WithSource=$false) {
    $w=$Probe.paint_timing_peer.window_pressure
    $entry=$w.PSObject.Properties['managed_menu']
    if ($null -eq $entry -or $null -eq $entry.Value) { return $false }
    $m=$entry.Value; $pair=$Probe.paint_timing_peer.window_pair
    $prior=[int]$WithSource; $n=1+$prior
    return $w.menu_pending_at_close -eq 1 -and @($m.menu).Count -eq 3 -and ($m.menu[0..1] -join ',') -ceq ($w.a_browser -join ',') -and
        $m.menu[2] -gt 0 -and $m.gpui_window -eq $pair.a_gpui -and $m.gpui_window -ne $pair.b_gpui -and
        ($m.close_task -join ',') -ceq ($w.close_task -join ',') -and
        $m.published -eq $n -and @($m.terminal_ids).Count -eq $n -and $m.terminal_ids[$prior] -eq $m.menu[2] -and
        $m.cancellations -eq $n -and $m.native_dismissals -eq $n -and $m.actions_submitted -eq $prior -and $m.outstanding -eq 0 -and $m.lease_drained -and
        $m.late_select_command -eq 117 -and $m.late_select_error -ceq 'InvalidRequest' -and $m.late_cancel_error -ceq 'InvalidRequest' -and
        @($m.tasks_posted).Count -eq 2 -and $m.tasks_posted[0] -ge $w.close_task[1] -and $m.tasks_posted[0] -eq $m.tasks_posted[1] -and
        @($m.native_calls).Count -eq 2 -and $m.native_calls[0] -gt 0 -and $m.native_calls[0] -eq $m.native_calls[1]
}
function Test-WindowPressureFlush($Probe) {
    $entry=$Probe.PSObject.Properties['window_flush']
    if ($null -eq $entry -or $null -eq $entry.Value) { return $false }
    $f=$entry.Value; $p=$Probe.paint_timing_peer; $w=$p.window_pressure
    return @($f.marker).Count -eq 3 -and $f.marker[2] -gt 0 -and $p.profiles.b.storage -ceq 'Persistent' -and
        ($f.marker[0..1] -join ',') -ceq ($p.profiles.b.configured_profile -join ',') -and
        ($f.marker[0..1] -join ',') -cne ($p.profiles.a.configured_profile -join ',') -and
        ($f.marker -join ',') -ceq ($w.peer_flush_marker_at_close -join ',') -and
        ($f.peer -join ',') -ceq ($w.peer_flush_browser_at_close -join ',') -and
        ($f.peer -join ',') -ceq ($w.b_browser -join ',') -and ($f.peer -join ',') -cne ($w.a_browser -join ',') -and
        ($f.peer -join ',') -ceq ($w.closed_events[1][0] -join ',') -and
        $f.callback_thread -eq $Probe.cef_ui_thread -and $f.held_after_a_closed -and $f.held_after_b_closed -and
        $f.poll_and_finish_blocked_only_by_flush -and $f.shutdown_calls_at_barrier -eq 0 -and
        $f.wrong_identity_and_new_flush_rejected -and $f.accepted_before_release -eq 1 -and $f.completed_before_release -eq 0 -and
        @($f.release_task).Count -eq 2 -and $f.release_task[0] -eq $w.close_task[0] -and
        $f.release_task[1] -gt $w.close_task[1] -and $f.release_task[1] -le $Probe.tasks_completed -and
        $f.original_ticket_complete -and $f.accepted_final -eq 1 -and $f.completed_final -eq 1 -and $f.pending_final -eq 0 -and
        $Probe.cookie_flushes_accepted -eq 1 -and $Probe.cookie_flushes_completed -eq 1 -and $Probe.pending_cookie_flushes -eq 0 -and $Probe.shutdown_calls -eq 1
}
function Test-WindowPressureSource($Probe) {
    $field=$Probe.PSObject.Properties['window_source']
    if ($null -eq $field -or $null -eq $field.Value) { return $false }
    $s=$field.Value; $w=$Probe.paint_timing_peer.window_pressure; $m=$w.managed_menu
    return @($s.operation).Count -eq 3 -and $s.operation[2] -gt 0 -and
        ($s.operation -join ',') -ceq ($w.source_operation_at_close -join ',') -and
        ($s.operation[0..1] -join ',') -ceq ($w.a_browser -join ',') -and
        $s.source_menu_sequence -gt 0 -and $s.source_menu_sequence -eq $m.terminal_ids[0] -and $s.source_menu_sequence -lt $m.menu[2] -and
        $s.source_bytes -gt 0 -and $s.source_bytes -le 8192 -and $s.source_hash -gt 0 -and $s.queued_without_viewer -and $s.host_ack -and
        $s.native_held_after_a_closed -and $s.native_held_after_b_closed -and $s.joint_source_flush_shutdown_blocked -and
        $s.source_released_before_flush -and $s.source_outcome -ceq 'Cancelled' -and $s.final_source_pending -eq 0
}
function Test-WindowPressureFind($Probe) {
    $pair=$Probe.paint_timing_peer; $w=$pair.window_pressure
    $entry=$pair.PSObject.Properties['window_find']
    if ($null -eq $entry -or $null -eq $entry.Value) { return $false }
    $f=$entry.Value; $ids=@($f.menu_terminal_ids)
    return @($f.original_repeat).Count -eq 3 -and @($f.first_final).Count -eq 3 -and
        ($f.original_repeat -join ',') -ceq ($w.find_operation_at_close -join ',') -and
        ($f.original_repeat[0..1] -join ',') -ceq ($w.b_browser -join ',') -and
        ($f.first_final[0..1] -join ',') -ceq ($w.b_browser -join ',') -and
        ($f.original_repeat[0..1] -join ',') -ceq ($w.closed_events[1][0] -join ',') -and
        $f.first_final[2] -gt 0 -and $f.original_repeat[2] -gt $f.first_final[2] -and $f.first_match_count -eq 1 -and
        $f.configured_gpui_window -eq $pair.window_pair.b_gpui -and $f.configured_gpui_window -ne $pair.window_pair.a_gpui -and
        $f.unresolved_after_a_closed -and $f.unresolved_after_b_paint -and $f.cancelled_after_b_closed -and
        $f.final_find_pending -eq 0 -and $f.late_results_observed -ge 0 -and $f.menu_actions -eq 3 -and $f.host_lease_drained -and
        $ids.Count -eq 3 -and $ids[0] -gt 0 -and $ids[0] -lt $ids[1] -and $ids[1] -lt $ids[2] -and
        @($ids | Where-Object {$_ -in $w.managed_menu.terminal_ids}).Count -eq 0
}
function Test-WindowMixedEvents($Probe) {
    $w=$Probe.paint_timing_peer.window_pressure
    $entry=$w.PSObject.Properties['mixed_events']
    if($null -eq $entry -or $null -eq $entry.Value){return $false}
    $m=$entry.Value; $seed=@($m.seed_events)
    if($seed.Count -ne 4 -or @($m.seed_reload_command).Count -ne 3 -or $m.seed_reload_command[2] -le 0 -or
        ($m.seed_reload_command[0..1] -join ',') -cne ($m.seed_browser -join ',') -or
        ($m.seed_browser -join ',') -cne ($w.b_browser -join ',') -or ($m.seed_browser -join ',') -ceq ($w.a_browser -join ',') -or
        $m.retained_total -ne 256 -or $m.retained_total -ne ($w.retained_title_events+$seed.Count) -or
        $m.rejected_ordinary -ne 1 -or $m.duplicate_seed_observations_ignored -lt 0){return $false}
    $kinds=@();$prev=0
    foreach($e in $seed){
        if(@($e).Count -ne 2 -or $e[1] -le $prev -or $e[1] -ge $w.first_title_sequence){return $false}
        $kinds += $e[0];$prev=$e[1]
    }
    return (($kinds|Sort-Object) -join ',') -ceq 'loading-end,loading-start,main-load-end,main-load-start' -and
        $w.event_depth_at_a_closed -eq $m.retained_total -and $w.resynced_title_before_close -eq ($w.retained_title_events+1)
}
function Test-WindowPrintPressure($Probe) {
    $peer=$Probe.paint_timing_peer
    $entry=$peer.PSObject.Properties['window_print']
    $close=$peer.window_pressure.PSObject.Properties['print_operations_at_close']
    if($null -eq $entry -or $null -eq $entry.Value -or $null -eq $close -or @($close.Value).Count -ne 2){return $false}
    $p=$entry.Value; $w=$peer.window_pressure; $pair=$peer.window_pair
    if(@($p.ids).Count -ne 2 -or @($p.windows).Count -ne 2 -or @($p.ids[0]).Count -ne 3 -or @($p.ids[1]).Count -ne 3){return $false}
    foreach ($field in @('initial_panel_paints','stale_owner_panel_paints','b_panel_paints_after_a_closed','b_panel_paints_before_refresh','b_panel_paints_at_repaint',
        'original_panel_entities_released','pending_print_rows')) {
        if ($null -eq $p.PSObject.Properties[$field] -or $null -eq $p.$field) { return $false }
    }
    $initial = @($p.initial_panel_paints); $stale = @($p.stale_owner_panel_paints)
    $released = @($p.original_panel_entities_released)
    if ($initial.Count -ne 2 -or $stale.Count -ne 2 -or $released.Count -ne 2) { return $false }
    foreach ($value in $initial + $stale + @($p.b_panel_paints_after_a_closed,$p.b_panel_paints_before_refresh,$p.b_panel_paints_at_repaint) + $p.pending_print_rows) {
        if (-not (Test-TimingCount $value)) { return $false }
    }
    if ($released[0] -isnot [bool] -or $released[1] -isnot [bool] -or -not $released[0] -or -not $released[1] -or
        ($p.pending_print_rows -join ',') -cne '2,2,1,1,0' -or $initial[0] -le 0 -or $initial[1] -le 0 -or
        $stale[0] -lt $initial[0] -or $p.b_panel_paints_after_a_closed -lt $initial[1] -or
        $p.b_panel_paints_before_refresh -lt $p.b_panel_paints_after_a_closed -or
        $p.b_panel_paints_at_repaint -le $p.b_panel_paints_before_refresh -or $stale[1] -lt $p.b_panel_paints_at_repaint) { return $false }
    return ($p.ids[0] -join ',') -ceq ($close.Value[0] -join ',') -and ($p.ids[1] -join ',') -ceq ($close.Value[1] -join ',') -and
        ($p.ids[0][0..1] -join ',') -ceq ($w.a_browser -join ',') -and ($p.ids[1][0..1] -join ',') -ceq ($w.b_browser -join ',') -and
        $p.ids[0][2] -gt 0 -and $p.ids[1][2] -gt $p.ids[0][2] -and
        $p.windows[0] -eq $pair.a_gpui -and $p.windows[1] -eq $pair.b_gpui -and
        $p.first_only_cancelled -is [bool] -and $p.first_only_cancelled -and
        $p.second_pending_after_first_reap -is [bool] -and $p.second_pending_after_first_reap -and
        $p.both_reaped -is [bool] -and $p.both_reaped -and $p.metadata_only -is [bool] -and $p.metadata_only -and
        $p.native_job_submitted -is [bool] -and -not $p.native_job_submitted
}
function Test-ResizeStageFiles([string]$Case, $Receipt, [int]$Stage, [ValidateSet('resize','exhaustion','visibility')][string]$Family = 'resize') {
    $last = if ($Family -ceq 'resize') { 5 } else { 3 }
    if ($Stage -lt 0 -or $Stage -gt $last -or $null -eq $Receipt -or
        $null -eq $Receipt.PSObject.Properties['metadata_sha256'] -or
        $null -eq $Receipt.PSObject.Properties['capture'] -or $null -eq $Receipt.capture) { return $false }
    $capture = $Receipt.capture
    foreach ($name in @('png','sha256','raw')) {
        if ($null -eq $capture.PSObject.Properties[$name]) { return $false }
    }
    $png = "$Family-window-$Stage.png"
    if ($capture.png -isnot [string] -or $capture.png -cne $png) { return $false }
    $files = @(
        @{ Name = $png; Digest = $capture.sha256; Bytes = $null; Limit = $null },
        @{ Name = "$Family-stage-$Stage.json"; Digest = $Receipt.metadata_sha256; Bytes = $null; Limit = 131072 }
    )
    if ($Stage -eq $last) {
        if ($null -ne $capture.raw) { return $false }
    } else {
        $raw = $capture.raw
        if ($null -eq $raw -or @(@('file','bytes','sha256') | Where-Object { $null -eq $raw.PSObject.Properties[$_] }).Count -gt 0) { return $false }
        $expected = "$Family-stage-$Stage-view.bgra"
        if ($raw.file -isnot [string] -or $raw.file -cne $expected -or ($raw.bytes -isnot [int] -and $raw.bytes -isnot [long]) -or
            $raw.bytes -le 0 -or $raw.bytes -gt 268435456) { return $false }
        $files += @{ Name = $expected; Digest = $raw.sha256; Bytes = $raw.bytes; Limit = $null }
    }
    try {
        foreach ($f in $files) {
            if ($f.Digest -isnot [string] -or $f.Digest -notmatch '\A[0-9a-fA-F]{64}\z') { return $false }
            $p = [IO.FileInfo]::new((Join-Path $Case $f.Name))
            if (-not $p.Exists -or $p.Length -le 0 -or ($null -ne $f.Bytes -and $p.Length -ne $f.Bytes) -or
                ($null -ne $f.Limit -and $p.Length -gt $f.Limit) -or
                (Get-PocFileSha256 $p.FullName) -ine $f.Digest) { return $false }
        }
    } catch {
        Write-Host "$Family stage $Stage byte verification failed: $($_.Exception.Message)"
        return $false
    }
    return $true
}
function Test-ManualOwnedStageFiles([string]$Case, $Receipt, [int]$Stage, [int]$OwnedPid, [ValidateSet('exhaustion','visibility','resize')][string]$Family = 'exhaustion') {
    if ($Family -cne 'exhaustion' -and $Stage -ne 0) { return $false }
    if (-not (Test-ResizeStageFiles $Case $Receipt $Stage -Family $Family)) { return $false }
    try {
        $metadataPath = Join-Path $Case "$Family-stage-$Stage.json"
        $stream = [IO.File]::OpenRead($metadataPath)
        try {
            if ($stream.Length -le 0 -or $stream.Length -gt 131072) { return $false }
            $size = [int]$stream.Length
            $reader = [IO.BinaryReader]::new($stream)
            try {
                [byte[]]$data = $reader.ReadBytes($size)
                if ($data.Length -ne $size -or $stream.ReadByte() -ne -1) { return $false }
            } finally { $reader.Dispose() }
        } finally { $stream.Dispose() }
        if ([Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($data)) -ine $Receipt.metadata_sha256) { return $false }
        $disk = [Text.UTF8Encoding]::new($false,$true).GetString($data) | ConvertFrom-Json -Depth 20
        $window = $disk.window
        $hwnd = $Receipt.hwnd
        $pre = $Receipt.manual_foreground_pre
        $post = $Receipt.manual_foreground_post
        $fg = $disk.initial_foreground
        if ($fg.policy -cne 'manual-owned-observation' -or $fg.approved -isnot [bool] -or $fg.approved -or
            ($fg.attempts -isnot [long] -and $fg.attempts -isnot [int]) -or $fg.attempts -ne 0 -or
            $fg.before -isnot [bool] -or $fg.after -isnot [bool] -or -not $fg.after) { return $false }
        if ($Stage -eq 3) {
            if ($null -ne $disk.view_raw) { return $false }
        } elseif ($disk.view_raw.file -cne $Receipt.capture.raw.file -or $disk.view_raw.bytes -ne $Receipt.capture.raw.bytes) { return $false }
        foreach ($id in @($hwnd, $pre, $post)) {
            if (($id -isnot [int] -and $id -isnot [long]) -or $id -le 0) { return $false }
        }
        return $Receipt.success -is [bool] -and $Receipt.success -and
            $Receipt.pid -eq $OwnedPid -and $disk.pid -eq $OwnedPid -and $window.pid -eq $OwnedPid -and
            $Receipt.stage -eq $Stage -and $disk.stage -eq $Stage -and
            $window.hwnd -eq $hwnd -and $pre -eq $hwnd -and $post -eq $hwnd -and
            $Receipt.owner_thread -eq $window.thread -and
            (($disk | ConvertTo-Json -Depth 20 -Compress) -ceq ($Receipt.metadata | ConvertTo-Json -Depth 20 -Compress))
    } catch {
        Write-Host "Manual $Family stage $Stage receipt verification failed: $($_.Exception.Message)"
        return $false
    }
}
function Get-ExecutableArtifact($Messages,[string]$Name) {
    $items = @($Messages | Where-Object { $_.reason -eq 'compiler-artifact' -and $_.target.name -eq $Name -and $null -ne $_.executable })
    if ($items.Count -ne 1) { throw "Expected one actual executable artifact for $Name; found $($items.Count)" }
    [IO.Path]::GetFullPath($items[0].executable)
}
function Build-Executable([string[]]$CargoArguments,[string]$Name,[string]$Log,[string[]]$RustcArguments=@()) {
    $ErrorActionPreference = 'Continue'
    $lines = @(& cargo @CargoArguments --message-format=json-render-diagnostics @RustcArguments 2>&1 | Tee-Object -FilePath $Log | ForEach-Object { Write-Host $_; $_ })
    $code = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($code -ne 0) { throw "Build $Name failed: $code" }
    $messages = @($lines | Where-Object { $_ -is [string] -and $_.StartsWith('{') } | ForEach-Object { $_ | ConvertFrom-Json })
    Get-ExecutableArtifact $messages $Name
}
$run = New-PocDirectory (Resolve-ValidationDir 'component-host' ([guid]::NewGuid().ToString('N')))
$summary = @{ schema=1; stage='ComponentHost'; scenario=$Scenario; cases=@(); success=$false; error=$null }
if($Scenario -eq 'PaintLatency'){
    $summary.validation_scope=if($TimingExtraFlushFault){'passive-extra-flush-failure-cleanup'}elseif($TimingWindowEventPressure){'passive-two-window-mixed-events-source-find-and-flush-pressure'}elseif($TimingWindowFindPressure){'passive-two-window-original-source-find-and-flush-pressure'}elseif($TimingWindowSourcePressure){'passive-two-window-original-source-and-flush-pressure'}elseif($TimingWindowMenuFlushPressure){'passive-two-window-command-title-menu-and-peer-flush-pressure'}elseif($TimingWindowMenuPressure){'passive-two-window-command-title-and-original-menu-pressure'}elseif($TimingWindowPressure){'passive-two-window-command-and-title-pressure'}elseif($TimingMeasureBoth){'passive-two-window-separate-copy-to-gpui-paint-measurements'}elseif($TimingProfileWindows){'passive-two-window-profile-copy-to-gpui-paint-submission'}elseif($TimingProfilePeer){'passive-two-profile-copy-to-gpui-paint-submission'}else{'passive-copy-to-gpui-paint-submission'}
    $summary.task_13_2_accepted=$false;$summary.task_13_3_accepted=$false;$summary.task_14_2_accepted=$false
    if ($TimingWindowPrintPressure) { $summary.validation_scope = 'passive-two-window-mixed-events-owned-print-panels-source-find-flush-pressure' }
    if ($TimingReadbackAdapter) {
        $summary.validation_scope = 'passive-two-window-dxgi-readback-adapter'
        $summary.readback_adapter_luid = $null
        if ($TimingRendererAdapter) { $summary.validation_scope = 'passive-two-window-gpui-dxgi-unique-description-correlation' }
    }
}
if($Scenario -eq 'ProfileDispatch'){$summary.validation_scope='passive-product-ui-multiple-profiles';$summary.task_13_3_accepted=$false;$summary.task_14_2_accepted=$false}
if($Scenario -eq 'ImeObserve'){$summary.validation_scope='ime-environment-only';$summary.task_11_2_accepted=$false;$summary.native_chinese_verified=$false}
if($Scenario -eq 'Resize'){$summary.validation_scope='window-layout-resize-slice';$summary.task_11_3_accepted=$false;$summary.resize_capture_evidence=[bool]$ResizeCaptureEvidence;$summary.resize_manual_foreground=[bool]$ResizeManualForeground;$summary.resize_passive=[bool]$ResizePassive;$summary.resize_passive_matrix=[bool]$ResizePassiveMatrix;$summary.resize_passive_right=[bool]$ResizePassiveRight}
if($ResizePassiveMatrix){$summary.resize_matrix_dpi=$null;$summary.resize_matrix_d3d11_capture_evidence=$true}
if($ResizeVerifiedTransitions){$summary.resize_verified_d3d11_transitions=$true}
if($Scenario -eq 'Unfocused'){$summary.validation_scope='unfocused-animation-slice';$summary.task_11_3_accepted=$false}
if($Scenario -eq 'Exhaustion'){$summary.validation_scope='recovery-exhaustion-slice';$summary.task_11_3_accepted=$false;$summary.validation_feature=$true;$summary.initial_foreground_approved=[bool]$InitialForegroundApproved;$summary.exhaustion_manual_foreground=[bool]$ExhaustionManualForeground}
if($Scenario -eq 'Visibility'){$summary.validation_scope='host-owned-visibility-slice';$summary.task_11_3_accepted=$false;$summary.validation_feature=$true;$summary.visibility_operations_approved=$true;$summary.visibility_manual_foreground=[bool]$VisibilityManualForeground}
if($Scenario -eq 'SourceViewer'){$summary.validation_scope='real-source-viewer-original-entity-release-slice';$summary.task_12_1_accepted=$false;$summary.task_12_2_accepted=$false;$summary.validation_feature=$true;$summary.os_input_sent=$false;$summary.foreground_requested=$false}
if($Scenario -eq 'FindView'){$summary.validation_scope='real-find-result-original-entity-release-slice';$summary.task_12_1_accepted=$false;$summary.task_12_2_accepted=$false;$summary.validation_feature=$true;$summary.os_input_sent=$false;$summary.foreground_requested=$false}
if ($Scenario -in @('SourceViewer','FindView')) { $summary.expected_cases = if ($Mode -eq 'All') { 4 } else { 1 } }
if($Scenario -eq 'MenuDefault'){$summary.validation_scope='real-default-menu-select-all-slice';$summary.task_12_1_accepted=$false;$summary.task_12_2_accepted=$false;$summary.validation_feature=$true;$summary.expected_cases=1;$summary.foreground_requested=$false}
if($Scenario -eq 'MenuDefault'){$summary.menu_foreground_approved=[bool]$MenuForegroundApproved}
if($MenuDismissProbe){$summary.validation_scope='real-menu-escape-outside-slice';$summary.menu_dismiss_probe=$true}
if($MenuServicesProbe){$summary.validation_scope='menu-real-view-entity-release-only';$summary.menu_services_probe=$true;$summary.task_12_1_accepted=$false;$summary.task_12_2_accepted=$false}
$oldCef=$env:CEF_PATH; $oldTarget=$env:CARGO_TARGET_DIR
try {
    $cef=Resolve-CefPath $CefPath; $env:CEF_PATH=$cef; $env:CARGO_TARGET_DIR=Resolve-TargetDir $TargetDir
    $bundleRoot=$run
    if($StageBundleInTemp){
        $prefix=switch($Scenario){'SourceViewer'{'component-source-viewer-'} 'FindView'{'component-find-view-'} 'MenuDefault'{'component-menu-default-'} default{'component-paint-timing-'}}
        $bundleRoot=Resolve-PocTempDir ($prefix + (Split-Path -Leaf $run))
        New-Item -ItemType Directory -Path $bundleRoot -ErrorAction Stop | Out-Null
    }
    if($Scenario -in $isolatedStagingScenarios){$summary.host_pdb_enabled=-not $TimingHostNoPdb}
    Push-Location $PocRoot
    try {
        $helper=Build-Executable @('build','--locked','--target',$PocTarget,'-p','rust-cef-helper','--bin','rust-cef-helper') 'rust-cef-helper' (Join-Path $run 'helper-build.log')
        $hostBuildArgs=@('build','--locked','--manifest-path','validation/component-host/Cargo.toml','--target',$PocTarget)
        if($Scenario -in @('Exhaustion','Visibility','SourceViewer','FindView','MenuDefault','PaintLatency') -or $MenuServicesProbe -or $ResizeCaptureEvidence -or $ResizePassive){$hostBuildArgs+=@('--features','validation')}
        $rustcArgs=@()
        if($TimingHostNoPdb){
            # Scoped response to the MSVC host PDB limit. Do not relink other
            # packages with different flags, delete symbols, or retry implicitly.
            $hostBuildArgs[0]='rustc'
            $hostBuildArgs+=@('--bin','component-host')
            $rustcArgs=@('--','-C','link-arg=/DEBUG:NONE','-C',('incremental='+(Join-Path $bundleRoot 'host-incremental')))
        }
        $hostArtifact=Build-Executable $hostBuildArgs 'component-host' (Join-Path $run 'host-build.log') $rustcArgs
    } finally { Pop-Location }
    $bundle=Join-Path $bundleRoot ([string][char]0x53d1 + [char]0x884c + ' public GPUI bundle with spaces')
    if($Scenario -in $isolatedStagingScenarios){$summary.bundle_path=$bundle}
    & (Join-Path $PSScriptRoot 'package-component.ps1') -HostExecutable $hostArtifact -HelperExecutable $helper -CefPath $cef -OutputDirectory $bundle -HostBuildVersion "component-host-$Scenario"

    if($SpellingDictionaryFixture){
        . (Join-Path $PSScriptRoot 'component-spelling-dictionary-fixture.ps1')
        Add-SpellingDictionaryFixture -Dictionary $SpellingDictionaryFixture -License $SpellingDictionaryLicense -Bundle $bundle -Run $run
        $summary.spelling_dictionary_fixture=$true
    }    $hostFile=Join-Path $bundle 'component-host.exe'; $helperFile=Join-Path $bundle 'rust-cef-helper.exe'
    Write-JsonFile @{host_artifact=$hostArtifact;helper_artifact=$helper;host_sha256=(Get-FileHash -LiteralPath $hostFile).Hash;helper_sha256=(Get-FileHash -LiteralPath $helperFile).Hash} (Join-Path $run 'artifact-identity.json')
    $modes=if($Mode -eq 'All'){@('Integrated','Threaded')}else{@($Mode)}
    $backends=if($Backend -eq 'All'){@('Software','D3d11')}else{@($Backend)}
    if($Scenario -in @('Resize','Unfocused','Exhaustion','Visibility')){$summary.expected_cases=Get-ObservationCaseCount $modes $backends}
    foreach($selectedMode in $modes){ foreach($selectedBackend in $backends){
        $case=Join-Path $run "$selectedMode-$selectedBackend"; $cwd=Join-Path $case 'unrelated cwd'
        New-Item -ItemType Directory -Path $cwd -Force | Out-Null
        $gate=Join-Path $case 'observer.release'; $report=Join-Path $case 'probe.json'
        $checkWindow = {
            param([int]$OwnedPid)
            if($Scenario -eq 'PaintLatency'){
                $proc=Get-Process -Id $OwnedPid -ErrorAction Stop
                try {
                    $hwnd=$proc.MainWindowHandle.ToInt64()
                    $allowedTitle=$proc.MainWindowTitle -ceq 'GPUI-CEF public host' -or ($TimingProfileWindows -and $proc.MainWindowTitle -ceq 'GPUI-CEF profile timing peer')
                    if($proc.Id -ne $OwnedPid -or $proc.Path -ine $hostFile -or $hwnd -le 0 -or -not $allowedTitle){
                        throw 'Timing owned GPUI window did not match the staged process'
                    }
                    Write-JsonFile @{pid=$proc.Id;hwnd=$hwnd;image=$proc.Path;title=$proc.MainWindowTitle;creation_filetime=$proc.StartTime.ToUniversalTime().ToFileTimeUtc();desktop_pixels_observed=$false} (Join-Path $case 'paint-window-identity.json')
                } finally { $proc.Dispose() }
                return
            }
            if($Scenario -eq 'MenuDefault'){
                $stageCount=if($MenuDismissProbe){5}elseif($MenuFindProbe -or $MenuEditProbe){6}else{4}
                for($stage=0;$stage -lt $stageCount;$stage++){
                    $ready=[IO.Path]::ChangeExtension($gate,"menu-default-$stage.ready");$watch=[Diagnostics.Stopwatch]::StartNew()
                    while(-not(Test-Path -LiteralPath $ready)){if($watch.Elapsed.TotalSeconds -gt 25){throw "MenuDefault stage $stage readiness timeout"};if($null -eq (Get-Process -Id $OwnedPid -ErrorAction SilentlyContinue)){throw 'MenuDefault host exited before observer'};Start-Sleep -Milliseconds 50}
                    if([IO.File]::ReadAllText($ready).Trim() -ne [string]$OwnedPid){throw 'MenuDefault stage PID mismatch'}
                    if($MenuDismissProbe){
                        & python (Join-Path $PSScriptRoot 'component-menu-dismiss-test.py') --pid $OwnedPid --case-dir $case --stage $stage --interactive-input-approved --dismiss-input-approved
                    }elseif($MenuSpellingProbe){
                        & python (Join-Path $PSScriptRoot 'component-menu-default-test.py') --pid $OwnedPid --case-dir $case --stage $stage --interactive-input-approved --command 200 --devtools --skip-selectall-title *> (Join-Path $case "menu-default-driver-$stage.log")
                    }elseif($MenuSubmenuProbe){
                        & python (Join-Path $PSScriptRoot 'component-menu-default-test.py') --pid $OwnedPid --case-dir $case --stage $stage --interactive-input-approved --command 28500 --devtools --skip-selectall-title --submenu-flow *> (Join-Path $case "menu-default-driver-$stage.log")
                    }elseif($MenuStructureProbe){
                        & python (Join-Path $PSScriptRoot 'component-menu-default-test.py') --pid $OwnedPid --case-dir $case --stage $stage --interactive-input-approved --command 28500 --devtools --skip-selectall-title *> (Join-Path $case "menu-default-driver-$stage.log")
                    }elseif($MenuEditProbe){
                        $cmd=if($stage -in @(2,3)){112}else{117}
                        & python (Join-Path $PSScriptRoot 'component-menu-default-test.py') --pid $OwnedPid --case-dir $case --stage $stage --interactive-input-approved --command $cmd --find-flow --expect-title menu-default-deleted *> (Join-Path $case "menu-default-driver-$stage.log")
                    }elseif($MenuSourceProbe){
                        & python (Join-Path $PSScriptRoot 'component-menu-default-test.py') --pid $OwnedPid --case-dir $case --stage $stage --interactive-input-approved --command 132 --devtools --skip-selectall-title *> (Join-Path $case "menu-default-driver-$stage.log")
                    }elseif($MenuFindProbe){
                        $cmd=if($stage -in @(2,3)){130}else{117}
                        & python (Join-Path $PSScriptRoot 'component-menu-default-test.py') --pid $OwnedPid --case-dir $case --stage $stage --interactive-input-approved --command $cmd --find-flow *> (Join-Path $case "menu-default-driver-$stage.log")
                    }elseif($MenuDevtoolsProbe){
                        & python (Join-Path $PSScriptRoot 'component-menu-default-test.py') --pid $OwnedPid --case-dir $case --stage $stage --interactive-input-approved --command 28440 --devtools
                    }else{
                        & python (Join-Path $PSScriptRoot 'component-menu-default-test.py') --pid $OwnedPid --case-dir $case --stage $stage --interactive-input-approved
                    }
                    if($LASTEXITCODE -ne 0){throw "MenuDefault actual stage $stage failed"}
                    Write-PidMarkerAtomic ([IO.Path]::ChangeExtension($gate,"menu-default-$stage.release")) $OwnedPid
                }
                return
            }
            if($Scenario -eq 'FindView'){
                for($stage=0;$stage -lt 2;$stage++){
                    $ready=[IO.Path]::ChangeExtension($gate,"find-view-$stage.ready");$watch=[Diagnostics.Stopwatch]::StartNew()
                    while(-not(Test-Path -LiteralPath $ready)){if($watch.Elapsed.TotalSeconds -gt 20){throw "Find View stage $stage readiness timeout"};if($null -eq (Get-Process -Id $OwnedPid -ErrorAction SilentlyContinue)){throw 'Find View host exited before observer'};Start-Sleep -Milliseconds 50}
                    if([IO.File]::ReadAllText($ready).Trim() -ne [string]$OwnedPid){throw 'Find View stage PID mismatch'}
                    & python (Join-Path $PSScriptRoot 'component-find-view-test.py') --pid $OwnedPid --case-dir $case --stage $stage
                    if($LASTEXITCODE -ne 0){throw "Find View stage $stage actual owned-window proof failed"}
                    Write-PidMarkerAtomic ([IO.Path]::ChangeExtension($gate,"find-view-$stage.release")) $OwnedPid
                }
                return
            }
            if($Scenario -eq 'SourceViewer') {
                for($stage=0;$stage -lt 2;$stage++){
                    $ready=[IO.Path]::ChangeExtension($gate,"source-viewer-$stage.ready")
                    $sourceWatch=[Diagnostics.Stopwatch]::StartNew()
                    while(-not(Test-Path -LiteralPath $ready)){
                        if($sourceWatch.Elapsed.TotalSeconds -gt 20){throw "Source viewer stage $stage readiness timeout"}
                        if($null -eq (Get-Process -Id $OwnedPid -ErrorAction SilentlyContinue)){throw "Host exited before source viewer stage $stage"}
                        Start-Sleep -Milliseconds 50
                    }
                    if([IO.File]::ReadAllText($ready).Trim() -ne [string]$OwnedPid){throw 'Source viewer stage PID mismatch'}
                    & python (Join-Path $PSScriptRoot 'component-source-viewer-test.py') --pid $OwnedPid --case-dir $case --stage $stage
                    if($LASTEXITCODE -ne 0){throw "Source viewer stage $stage actual owned-window proof failed"}
                    Write-PidMarkerAtomic ([IO.Path]::ChangeExtension($gate,"source-viewer-$stage.release")) $OwnedPid
                }
                return
            }
            if($Scenario -eq 'Popup') {
                for($stage=0; $stage -lt 9; $stage++) {
                    $ready=[IO.Path]::ChangeExtension($gate,"popup-$stage.ready")
                    $stageWatch=[Diagnostics.Stopwatch]::StartNew()
                    while(-not (Test-Path -LiteralPath $ready)) {
                        if($stageWatch.Elapsed.TotalSeconds -gt 20){throw "Popup stage $stage readiness timeout"}
                        if($null -eq (Get-Process -Id $OwnedPid -ErrorAction SilentlyContinue)){throw "Host exited before popup stage $stage"}
                        Start-Sleep -Milliseconds 50
                    }
                    if([IO.File]::ReadAllText($ready).Trim() -ne [string]$OwnedPid){throw "Popup stage $stage PID mismatch"}
                    & python (Join-Path $PSScriptRoot 'capture-component-popup.py') --pid $OwnedPid --case-dir $case --stage $stage --interactive-input-approved
                    if($LASTEXITCODE -ne 0){throw "Actual popup stage $stage capture/composition/input verification failed"}
                    Write-PidMarkerAtomic ([IO.Path]::ChangeExtension($gate,"popup-$stage.release")) $OwnedPid
                }
                return
            }
            if($Scenario -eq 'Visibility') {
                for($stage=0;$stage -lt 4;$stage++) {
                    $ready=[IO.Path]::ChangeExtension($gate,"visibility-$stage.ready")
                    $stageWatch=[Diagnostics.Stopwatch]::StartNew()
                    while(-not(Test-Path -LiteralPath $ready)) {
                        if($stageWatch.Elapsed.TotalSeconds -gt 20){throw "Visibility stage $stage readiness timeout"}
                        if($null -eq (Get-Process -Id $OwnedPid -ErrorAction SilentlyContinue)){throw "Host exited before visibility stage $stage"}
                        Start-Sleep -Milliseconds 50
                    }
                    if([IO.File]::ReadAllText($ready).Trim() -ne [string]$OwnedPid){throw 'Visibility stage PID mismatch'}
                    $visibilityArgs=@('--pid',$OwnedPid,'--case-dir',$case,'--stage',$stage)
                    if($VisibilityManualForeground){$visibilityArgs+='--manual-foreground'}
                    & python (Join-Path $PSScriptRoot 'component-visibility-test.py') @visibilityArgs
                    if($LASTEXITCODE -ne 0){throw "Visibility stage $stage owner/hidden/pixel proof failed"}
                    if($VisibilityManualForeground -and $stage -eq 0){
                        $receipt=Get-Content -LiteralPath (Join-Path $case 'visibility-window-0.json') -Raw -Encoding utf8 | ConvertFrom-Json -Depth 20
                        if(-not (Test-ManualOwnedStageFiles $case $receipt 0 $OwnedPid -Family visibility)){throw 'Visibility manual initial receipt or artifact mismatch before minimize'}
                    }
                    Write-PidMarkerAtomic ([IO.Path]::ChangeExtension($gate,"visibility-$stage.release")) $OwnedPid
                }
                return
            }
            if($Scenario -eq 'Exhaustion') {
                for($stage=0;$stage -lt 4;$stage++) {
                    $ready=[IO.Path]::ChangeExtension($gate,"exhaustion-$stage.ready")
                    $stageWatch=[Diagnostics.Stopwatch]::StartNew()
                    while(-not(Test-Path -LiteralPath $ready)) {
                        if($stageWatch.Elapsed.TotalSeconds -gt 20){throw "Exhaustion stage $stage readiness timeout"}
                        if($null -eq (Get-Process -Id $OwnedPid -ErrorAction SilentlyContinue)){throw "Host exited before exhaustion stage $stage"}
                        Start-Sleep -Milliseconds 50
                    }
                    if([IO.File]::ReadAllText($ready).Trim() -ne [string]$OwnedPid){throw 'Exhaustion stage PID mismatch'}
                    $observerArgs=@('--pid',$OwnedPid,'--case-dir',$case,'--stage',$stage)
                    if($InitialForegroundApproved){$observerArgs+='--initial-foreground-approved'}
                    if($ExhaustionManualForeground){$observerArgs+='--manual-foreground'}
                    & python (Join-Path $PSScriptRoot 'component-exhaustion-test.py') @observerArgs
                    if($LASTEXITCODE -ne 0){throw "Exhaustion stage $stage real pixel/owner proof failed"}
                    if($ExhaustionManualForeground){
                        $receipt = Get-Content -LiteralPath (Join-Path $case "exhaustion-window-$stage.json") -Raw -Encoding utf8 | ConvertFrom-Json -Depth 20
                        if(-not (Test-ManualOwnedStageFiles $case $receipt $stage $OwnedPid)){throw "Manual Exhaustion stage $stage receipt/asset mismatch before release"}
                    }
                    Write-PidMarkerAtomic ([IO.Path]::ChangeExtension($gate,"exhaustion-$stage.release")) $OwnedPid
                }
                return
            }
            if($Scenario -eq 'Unfocused') {
                for($stage=0; $stage -lt 3; $stage++) {
                    $ready=[IO.Path]::ChangeExtension($gate,"unfocused-$stage.ready")
                    $stageWatch=[Diagnostics.Stopwatch]::StartNew()
                    while(-not (Test-Path -LiteralPath $ready)) {
                        if($stageWatch.Elapsed.TotalSeconds -gt 20){throw "Unfocused stage $stage readiness timeout"}
                        if($null -eq (Get-Process -Id $OwnedPid -ErrorAction SilentlyContinue)){throw "Host exited before unfocused stage $stage"}
                        Start-Sleep -Milliseconds 50
                    }
                    if([IO.File]::ReadAllText($ready).Trim() -ne [string]$OwnedPid){throw 'Unfocused stage PID mismatch'}
                    & python (Join-Path $PSScriptRoot 'component-unfocused-test.py') --pid $OwnedPid --case-dir $case --stage $stage
                    if($LASTEXITCODE -ne 0){throw "Unfocused stage $stage actual owner/frame observation failed"}
                    Write-PidMarkerAtomic ([IO.Path]::ChangeExtension($gate,"unfocused-$stage.release")) $OwnedPid
                }
                return
            }
            if($Scenario -eq 'Resize') {
                for($stage=0; $stage -lt 6; $stage++) {
                    $ready=[IO.Path]::ChangeExtension($gate,"resize-$stage.ready")
                    $stageWatch=[Diagnostics.Stopwatch]::StartNew()
                    while(-not (Test-Path -LiteralPath $ready)) {
                        if($stageWatch.Elapsed.TotalSeconds -gt 20){throw "Resize stage $stage readiness timeout"}
                        if($null -eq (Get-Process -Id $OwnedPid -ErrorAction SilentlyContinue)){throw "Host exited before resize stage $stage"}
                        Start-Sleep -Milliseconds 50
                    }
                    if([IO.File]::ReadAllText($ready).Trim() -ne [string]$OwnedPid){throw 'Resize stage PID mismatch'}
                    $resizeArgs=@('--pid',$OwnedPid,'--case-dir',$case,'--stage',$stage)
                    if($ResizeManualForeground){$resizeArgs+='--manual-foreground'}
                    if($ResizePassive){$resizeArgs+='--passive-window'}
                    & python (Join-Path $PSScriptRoot 'component-resize-test.py') @resizeArgs
                    if($LASTEXITCODE -ne 0){throw "Resize stage $stage actual window/pixel observation failed"}
                    if($ResizeManualForeground -and $stage -eq 0){
                        $receipt=Get-Content -LiteralPath (Join-Path $case 'resize-window-0.json') -Raw -Encoding utf8 | ConvertFrom-Json -Depth 20
                        if(-not (Test-ManualOwnedStageFiles $case $receipt 0 $OwnedPid -Family resize)){throw 'Resize manual initial receipt or artifact mismatch before next size'}
                    }
                    Write-PidMarkerAtomic ([IO.Path]::ChangeExtension($gate,"resize-$stage.release")) $OwnedPid
                }
                return
            }
            if($Scenario -eq 'ImeObserve') {
                for($stage=0; $stage -lt 4; $stage++) {
                    $ready=[IO.Path]::ChangeExtension($gate,"ime-observe-$stage.ready")
                    $stageWatch=[Diagnostics.Stopwatch]::StartNew()
                    while(-not (Test-Path -LiteralPath $ready)) {
                        if($stageWatch.Elapsed.TotalSeconds -gt 20){throw "IME observation stage $stage readiness timeout"}
                        if($null -eq (Get-Process -Id $OwnedPid -ErrorAction SilentlyContinue)){throw "Host exited before IME observation stage $stage"}
                        Start-Sleep -Milliseconds 50
                    }
                    if([IO.File]::ReadAllText($ready).Trim() -ne [string]$OwnedPid){throw 'IME observation stage PID mismatch'}
                    & python (Join-Path $PSScriptRoot 'component-ime-observe.py') --pid $OwnedPid --case-dir $case --stage $stage --interactive-input-approved
                    if($LASTEXITCODE -ne 0){throw "Owned IME environment observation $stage failed; no keyboard attempted"}
                    Write-PidMarkerAtomic ([IO.Path]::ChangeExtension($gate,"ime-observe-$stage.release")) $OwnedPid
                }
                return
            }
            if($Scenario -eq 'Input') {
                for($stage=0; $stage -lt 6; $stage++) {
                    $ready=[IO.Path]::ChangeExtension($gate,"input-$stage.ready")
                    $stageWatch=[Diagnostics.Stopwatch]::StartNew()
                    while(-not (Test-Path -LiteralPath $ready)) {
                        if($stageWatch.Elapsed.TotalSeconds -gt 20){throw "Input stage $stage readiness timeout"}
                        if($null -eq (Get-Process -Id $OwnedPid -ErrorAction SilentlyContinue)){throw "Host exited before input stage $stage"}
                        Start-Sleep -Milliseconds 50
                    }
                    if([IO.File]::ReadAllText($ready).Trim() -ne [string]$OwnedPid){throw "Input stage $stage PID mismatch"}
                    & python (Join-Path $PSScriptRoot 'component-input-test.py') --pid $OwnedPid --case-dir $case --stage $stage --interactive-input-approved
                    if($LASTEXITCODE -ne 0){throw "Actual input stage $stage capture/input verification failed"}
                    Write-PidMarkerAtomic ([IO.Path]::ChangeExtension($gate,"input-$stage.release")) $OwnedPid
                }
                return
            }
            if($Scenario -in @('Dispatch','ProfileDispatch')) {
                for($stage=0; $stage -lt 6; $stage++) {
                    $ready=[IO.Path]::ChangeExtension($gate,"stage-$stage.ready")
                    $stageWatch=[Diagnostics.Stopwatch]::StartNew()
                    while(-not (Test-Path -LiteralPath $ready)) {
                        if($stageWatch.Elapsed.TotalSeconds -gt 20){throw "Dispatch stage $stage readiness timeout"}
                        if($null -eq (Get-Process -Id $OwnedPid -ErrorAction SilentlyContinue)){throw "Host exited before dispatch stage $stage"}
                        Start-Sleep -Milliseconds 50
                    }
                    if([IO.File]::ReadAllText($ready).Trim() -ne [string]$OwnedPid){throw "Dispatch stage $stage PID mismatch"}
                    & python (Join-Path $PSScriptRoot 'capture-component-dispatch.py') --pid $OwnedPid --case-dir $case --stage $stage
                    if($LASTEXITCODE -ne 0){throw "Actual dispatch window stage $stage pixel/owner verification failed"}
                    [IO.File]::WriteAllText([IO.Path]::ChangeExtension($gate,"stage-$stage.release"),[string]$OwnedPid)
                }
                return
            }
            & python (Join-Path $PSScriptRoot 'capture-component-host.py') --pid $OwnedPid --case-dir $case --phase first
            if($LASTEXITCODE -ne 0){throw 'Actual embedded GPUI window pixel verification failed'}
            [IO.File]::WriteAllText([IO.Path]::ChangeExtension($gate,'detach'),[string]$OwnedPid)
            $watch=[Diagnostics.Stopwatch]::StartNew(); $detached=[IO.Path]::ChangeExtension($gate,'detached')
            while(-not (Test-Path -LiteralPath $detached)){
                if($watch.Elapsed.TotalSeconds -gt 10){throw 'View detach handshake timeout'}
                if($null -eq (Get-Process -Id $OwnedPid -ErrorAction SilentlyContinue)){throw 'Host exited while detaching View'}
                Start-Sleep -Milliseconds 50
            }
            if([IO.File]::ReadAllText($detached).Trim() -ne [string]$OwnedPid){throw 'Detached PID mismatch'}
            & python (Join-Path $PSScriptRoot 'capture-component-host.py') --pid $OwnedPid --case-dir $case --phase detached
            if($LASTEXITCODE -ne 0){throw 'Host-alive green window pixel verification failed'}
        }
        $hostArgs=@('--mode',$selectedMode,'--backend',$selectedBackend,'--report',$report,'--helper',$helperFile,'--observer-gate',$gate,'--scenario',$Scenario)
        if($TimingProfilePeer){$hostArgs+=@('--timing-profile-peer','true')}
        if($TimingProfileWindows){$hostArgs+=@('--timing-profile-windows','true')}
        if($TimingMeasureBoth){$hostArgs+=@('--timing-measure-both','true')}
        if($TimingWindowPressure){$hostArgs+=@('--timing-window-pressure','true')}
        if($TimingWindowMenuPressure){$hostArgs+=@('--timing-window-menu-pressure','true')}
        if($TimingWindowMenuFlushPressure){$hostArgs+=@('--timing-window-menu-flush-pressure','true')}
        if($TimingExtraFlushFault){$hostArgs+=@('--timing-extra-flush-fault','true')}
        if($TimingWindowSourcePressure){$hostArgs+=@('--timing-window-source-pressure','true')}
        if($TimingWindowFindPressure){$hostArgs+=@('--timing-window-find-pressure','true')}
        if($TimingWindowEventPressure){$hostArgs+=@('--timing-window-event-pressure','true')}
        if($TimingWindowPrintPressure){$hostArgs+=@('--timing-window-print-pressure','true')}
        if($Scenario -in @('Popup','Input','ImeObserve','Resize','Unfocused','Exhaustion','MenuDefault') -and -not $ResizePassive){$hostArgs+=@('--interactive-input-approved','true')}
        if($ManualForeground){$hostArgs+=@('--manual-foreground','true')}
        if($InitialForegroundApproved){$hostArgs+=@('--initial-foreground-approved','true')}
        if($ExhaustionManualForeground){$hostArgs+=@('--exhaustion-manual-foreground','true')}
        if($VisibilityOperationsApproved){$hostArgs+=@('--visibility-operations-approved','true')}
        if($VisibilityManualForeground){$hostArgs+=@('--visibility-manual-foreground','true')}
        if($ResizeCaptureEvidence -or ($ResizePassiveMatrix -and $selectedBackend -ceq 'D3d11')){$hostArgs+=@('--resize-capture-evidence','true')}
        if($ResizeVerifiedTransitions -and $selectedBackend -ceq 'D3d11'){$hostArgs+=@('--resize-verified-transitions','true')}
        if($ResizePassive){$hostArgs+=@('--resize-passive','true')}
        if($ResizePassiveRight){$hostArgs+=@('--resize-passive-right','true')}
        if($ResizeExhaustion){$hostArgs+=@('--resize-exhaustion','true')}
        if($ResizeManualForeground){
            $hostArgs+=@('--resize-manual-foreground','true')
            Write-Host 'Manual Resize: select the new GPUI-CEF public host from the taskbar or title bar, then leave it unobstructed. Do not move or resize it yourself.'
        }
        if($MenuServicesProbe){$hostArgs+=@('--menu-services-probe','true')}
        if($MenuForegroundApproved){$hostArgs+=@('--menu-foreground-approved','true')}
        if($MenuDismissProbe){$hostArgs+=@('--menu-dismiss-probe','true')}
        if($MenuDevtoolsProbe){$hostArgs+=@('--menu-devtools-probe','true')}
        if($MenuFindProbe){$hostArgs+=@('--menu-find-probe','true')}
        if($MenuSourceProbe){$hostArgs+=@('--menu-source-probe','true')}
        if($MenuEditProbe){$hostArgs+=@('--menu-edit-probe','true')}
        if($MenuSpellingProbe){$hostArgs+=@('--menu-spelling-probe','true')}
        if($MenuStructureProbe){$hostArgs+=@('--menu-structure-probe','true')}
        if($MenuSubmenuProbe){$hostArgs+=@('--menu-submenu-probe','true')}
        if($ExhaustionManualForeground){
            Write-Host 'Manual Exhaustion: when the new GPUI-CEF public host window appears, bring that window to the foreground without moving or resizing it. Keep it unobstructed until it closes. The harness does not activate it.'
        }
        if($VisibilityManualForeground){
            Write-Host 'Manual Visibility: select the new GPUI-CEF public host without moving or resizing it. The test will minimize and restore it once. Leave it unobstructed until it closes.'
        }
        $launch=Invoke-Probe $hostFile $hostArgs $cwd $gate $helperFile $checkWindow
        Write-JsonFile $launch (Join-Path $case 'launch.json')
        [IO.File]::WriteAllText((Join-Path $case 'stdout.log'),$launch.stdout)
        [IO.File]::WriteAllText((Join-Path $case 'stderr.log'),$launch.stderr)
        $passed=$false; $observed=$null
        if($Scenario -eq 'PaintLatency') {
            if(Test-Path -LiteralPath $report){
                $observed=Get-Content -LiteralPath $report -Raw -Encoding UTF8 | ConvertFrom-Json
                $windowPath=Join-Path $case 'paint-window-identity.json'
                if(Test-Path -LiteralPath $windowPath){
                    $window=Get-Content -LiteralPath $windowPath -Raw -Encoding UTF8 | ConvertFrom-Json
                    $passed=Test-PaintLatencyReport $observed $launch $window $selectedMode $selectedBackend $hostFile ([bool]$TimingProfilePeer) ([bool]$TimingWindowPressure)
                    if($passed -and $TimingProfilePeer){$passed=Test-TimingProfilePeer $observed $selectedBackend}
                    if($passed -and $TimingProfileWindows){$passed=Test-TimingWindowPair $observed $window}
                    if($passed -and $TimingMeasureBoth){$passed=Test-BothWindowTiming $observed $selectedMode $selectedBackend}
                    if ($passed -and $TimingReadbackAdapter) {
                        $key = Get-TimingReadbackAdapter $observed $summary.readback_adapter_luid
                        $passed = $null -ne $key
                        if ($passed -and $TimingRendererAdapter) { $passed = Test-TimingRendererAdapter $observed $key }
                        if ($passed) { $summary.readback_adapter_luid = $key }
                    }
                    if($passed -and $TimingWindowPressure){$passed=Test-WindowPressure $observed ([bool]$TimingWindowMenuPressure) ([bool]$TimingWindowEventPressure)}
                    if($passed -and $TimingWindowMenuPressure){$passed=Test-WindowPressureMenu $observed ([bool]$TimingWindowSourcePressure)}
                    if($passed -and $TimingWindowMenuFlushPressure){$passed=Test-WindowPressureFlush $observed}
                    if($passed -and $TimingWindowSourcePressure){$passed=Test-WindowPressureSource $observed}
                    if($passed -and $TimingWindowFindPressure){$passed=Test-WindowPressureFind $observed}
                    if($passed -and $TimingWindowEventPressure){$passed=Test-WindowMixedEvents $observed}
                    if($passed -and $TimingWindowPrintPressure){$passed=Test-WindowPrintPressure $observed}
                    if($TimingExtraFlushFault){$passed=$false}
                }
            }
        } elseif(Test-Path -LiteralPath $report){
            $observed=Get-Content -LiteralPath $report -Raw -Encoding UTF8 | ConvertFrom-Json
            $firstName=switch($Scenario){MenuDefault{'menu-default-window-0.json'} FindView{'find-view-window-0.json'} SourceViewer{'source-viewer-window-0.json'} Dispatch{'window-stage-0.json'} ProfileDispatch{'window-stage-0.json'} Popup{'popup-window-0.json'} Input{'input-window-0.json'} ImeObserve{'ime-observe-window-0.json'} Resize{'resize-window-0.json'} Unfocused{'unfocused-window-0.json'} Exhaustion{'exhaustion-window-0.json'} Visibility{'visibility-window-0.json'} default{'window-first.json'}}
            $lastName=switch($Scenario){MenuDefault{'menu-default-window-3.json'} FindView{'find-view-window-1.json'} SourceViewer{'source-viewer-window-1.json'} Dispatch{'window-stage-5.json'} ProfileDispatch{'window-stage-5.json'} Popup{'popup-window-8.json'} Input{'input-window-5.json'} ImeObserve{'ime-observe-window-1.json'} Resize{'resize-window-5.json'} Unfocused{'unfocused-window-2.json'} Exhaustion{'exhaustion-window-3.json'} Visibility{'visibility-window-3.json'} default{'window-detached.json'}}
            if($MenuDismissProbe){$firstName='menu-dismiss-window-0.json';$lastName='menu-dismiss-window-4.json'}
            $first=if(Test-Path -LiteralPath (Join-Path $case $firstName)){Get-Content -LiteralPath (Join-Path $case $firstName) -Raw -Encoding UTF8 | ConvertFrom-Json}else{$null}
            $detached=if(Test-Path -LiteralPath (Join-Path $case $lastName)){Get-Content -LiteralPath (Join-Path $case $lastName) -Raw -Encoding UTF8 | ConvertFrom-Json}else{$null}
            $expectedBrowsers=if($Scenario -in @('Dispatch','ProfileDispatch','Popup','Input','ImeObserve')){2}else{1}
            $windowChain=if($Scenario -in @('Resize','Unfocused','Exhaustion','Visibility')){$null -ne $first -and $null -ne $detached -and $first.success -eq $true -and $detached.success -eq $true -and $null -ne $first.PSObject.Properties['hwnd'] -and $null -ne $detached.PSObject.Properties['hwnd'] -and $first.hwnd -eq $detached.hwnd}elseif($Scenario -eq 'Popup'){$null -ne $first -and $null -ne $detached -and $first.hwnd -ne $detached.hwnd}elseif($Scenario -in @('Input','ImeObserve')){$null -ne $first -and $null -ne $detached -and @($first.hwnds).Count -eq 2 -and @($first.hwnds|Select-Object -Unique).Count -eq 2 -and @($detached.hwnds).Count -eq 2 -and ((@($first.hwnds|Sort-Object) -join ',') -eq (@($detached.hwnds|Sort-Object) -join ','))}else{$null -ne $first -and $null -ne $detached -and $first.hwnd -eq $detached.hwnd}
            $threadRule=($selectedMode -eq 'Integrated' -and $observed.cef_ui_thread -eq $observed.main_thread -and $observed.manual_pump -gt 0) -or ($selectedMode -eq 'Threaded' -and $observed.cef_ui_thread -gt 0 -and $observed.cef_ui_thread -ne $observed.main_thread -and $observed.manual_pump -eq 0)
            $expectedBackend=if($selectedBackend -eq 'Software'){'Some(SoftwareRequired)'}else{'Some(D3d11Required)'}
            $passed=$null -ne $first -and $null -ne $detached -and $launch.exit_code -eq 0 -and -not $launch.timed_out -and $launch.streams_complete -and $launch.image_verified -and $null -eq $launch.observer_error -and $launch.remaining_processes.Count -eq 0 -and
                $observed.schema -eq 1 -and $observed.kind -eq 'component-host' -and $observed.pid -eq $launch.pid -and $observed.mode -eq $selectedMode -and $observed.backend -eq $selectedBackend -and $observed.observed_backend -eq $expectedBackend -and
                $observed.success -eq $true -and $null -eq $observed.error -and $observed.created -eq $expectedBrowsers -and $observed.closed -eq $expectedBrowsers -and $observed.valid_frames -gt 0 -and $observed.wake_consumed -gt 0 -and $observed.borrow_violations -eq 0 -and
                $null -ne $observed.paint -and $null -ne $observed.released_view -and $observed.paint.paint_calls -gt 0 -and $observed.paint.painted_sequence -gt 0 -and $observed.paint.created_images -gt 0 -and $observed.paint.frame_width -gt 0 -and $observed.paint.frame_height -gt 0 -and
                $observed.host_alive_after_view_drop -eq $true -and $observed.view_release_observed_closing -eq $true -and $observed.duplicate_attachment_refused -eq $true -and
                $observed.main_thread -gt 0 -and $observed.init_thread -eq $observed.main_thread -and $observed.shutdown_thread -eq $observed.main_thread -and $threadRule -and
                $observed.finished -eq $true -and $observed.shutdown_calls -eq 1 -and $observed.native_after -eq 0 -and $observed.pending_tasks -eq 0 -and ($observed.tasks_completed + $observed.post_failures) -eq $observed.tasks_posted -and $observed.tasks_posted -gt 0 -and
                $observed.final_cpu_used -eq 0 -and $observed.released_view.released_images -eq $observed.released_view.created_images -and $observed.pending_requests -eq 0 -and $observed.pending_profiles -eq 0 -and $observed.profile_refs -eq 0 -and $observed.app_retained -eq $false -and $observed.root_lease -eq $false -and
                $first.success -eq $true -and $detached.success -eq $true -and $first.pid -eq $launch.pid -and $detached.pid -eq $launch.pid -and $windowChain
            if($Scenario -eq 'FindView'){
                $passed=$passed -and $observed.scenario -eq 'FindView' -and $observed.find_view_verified -eq $true -and $null -ne $observed.find_view -and $observed.find_view.success -eq $true -and $observed.find_view.phase -eq 4 -and $observed.find_view.first_result -and $observed.find_view.first_footer_renders -gt 0 -and $observed.find_view.automatic_view_release_cleanup -and -not $observed.find_view.find_dispatch_used_after_owner_drop -and $observed.find_view.route_unregistered_after_drain -and $observed.find_view.real_view_owner -and -not $observed.find_view.menu_widget_tested -and -not $observed.find_view.full_find_matrix -and $first.sampled_footer_light_pixels -ge 15 -and -not $first.input_sent -and -not $first.foreground_requested
                if($passed){
                    $findBefore=Get-Content -LiteralPath (Join-Path $case 'find-view-stage-0.json') -Raw -Encoding UTF8 | ConvertFrom-Json
                    $findAfter=Get-Content -LiteralPath (Join-Path $case 'find-view-stage-1.json') -Raw -Encoding UTF8 | ConvertFrom-Json
                    $passed=Test-FindViewIdentity $observed $findBefore $findAfter $first $detached
                }
            }
            if($Scenario -eq 'MenuDefault' -and -not $MenuDismissProbe -and -not $MenuDevtoolsProbe -and -not $MenuFindProbe -and -not $MenuSourceProbe -and -not $MenuEditProbe -and -not $MenuSpellingProbe -and -not $MenuStructureProbe -and -not $MenuSubmenuProbe){
                $d=if($null -ne $observed.PSObject.Properties['menu_default']){$observed.menu_default}else{$null}
                if($null -eq $d){$passed=$false}else{$passed=$passed -and $d.success -and $d.phase -eq 4 -and $d.real_widget_path -and $d.widget_select_all_verified -and -not $d.full_menu_matrix -and -not $d.synthetic_cef_input -and $d.native.opened -eq 1 -and $d.native.published -eq 1 -and $d.native.actions -eq 1 -and $d.native.dismissed -eq 1 -and $d.native.post_failures -eq 0 -and $null -eq $d.native.first_failure -and $d.visual.retired -and $d.visual.pending -eq 0 -and $null -eq $d.visual.first_error}
                if($null -ne $d){$fg=$d.initial_foreground;$passed=$passed -and $fg.approved -eq [bool]$MenuForegroundApproved -and $fg.attempts -in @(0,1);if($MenuForegroundApproved){$passed=$passed -and $fg.after -eq $true -and $fg.policy -eq 'one-ordinary-owned-request'}else{$passed=$passed -and $fg.attempts -eq 0 -and $fg.policy -eq 'passive'};$summary.foreground_requested=$fg.attempts -gt 0}
                for($i=0;$i -lt 4;$i++){
                    $p=Join-Path $case "menu-default-window-$i.json";$w=Join-Path $case "menu-default-observed-$i.json"
                    if(-not(Test-Path $p) -or -not(Test-Path $w)){$passed=$false;continue}
                    $s=Get-Content $p -Raw -Encoding utf8|ConvertFrom-Json;$wm=Get-Content $w -Raw -Encoding utf8|ConvertFrom-Json
                    $count=if($i -lt 2){2}else{0};$trace=$wm.trace
                    $passed=$passed -and $s.success -and $s.stage -eq $i -and $s.pid -eq $launch.pid -and $s.hwnd -eq $first.hwnd -and -not $s.foreground_requested -and $s.input_sent -eq ($i -lt 2) -and $wm.pid -eq $launch.pid -and $wm.stage -eq $i -and $wm.window.hwnd -eq $s.hwnd -and $trace.complete -and $null -eq $trace.error -and @($trace.expected).Count -eq $count -and @($trace.observed).Count -eq $count -and @($trace.raw).Count -eq $count
                    $passed=$passed -and $s.process_identity_verified -and $s.metadata.process_identity.created_filetime -gt 0 -and @($s.button_observations).Count -eq $count
                    if($i -lt 2){$passed=$passed -and $s.button_observations[0].accepted -and $s.button_observations[0].pressed_after -eq $true -and $s.button_observations[1].accepted -and $s.button_observations[1].pressed_after -eq $false -and $null -eq $s.possibly_held}
                    foreach($raw in @($trace.raw)){$passed=$passed -and $raw.accepted -and $raw.raw_extra_info -eq 1128678706 -and $raw.observation_point -eq 'window-proc' -and -not $raw.during_manual_pump}
                    if($i -eq 0){$passed=$passed -and (@($trace.raw.message) -join ',') -eq '516,517'}
                    if($i -eq 1){$passed=$passed -and (@($trace.raw.message) -join ',') -eq '513,514' -and $s.menu_contrast_samples -ge 12}
                    if($i -eq 2){$passed=$passed -and $s.metadata.title -eq 'menu-default-selected:1' -and $s.metadata.native.actions -eq 1 -and $s.metadata.native.dismissed -eq 1}
                    if($i -eq 3){$passed=$passed -and $s.metadata.weak_released -and $s.metadata.browser_closed -and $s.metadata.host_alive}
                }
            }
            if($Scenario -eq 'MenuDefault' -and $MenuSpellingProbe){
                $d=$observed.menu_default
                $passed=$passed -and $null -ne $d -and $d.success -and $d.phase -eq 2 -and $d.spelling_probe -eq $true -and $null -ne $d.spelling_clicked -and [int]$d.spelling_clicked -ge 200 -and [int]$d.spelling_clicked -le 204 -and -not $d.synthetic_cef_input -and $d.native.opened -eq 1 -and $d.native.published -eq 1 -and $d.native.actions -eq 1 -and $d.native.dismissed -eq 1 -and $d.native.post_failures -eq 0 -and $null -eq $d.native.first_failure -and $d.visual.retired -and $null -eq $d.visual.first_error
                if($null -ne $d){$fg=$d.initial_foreground;$passed=$passed -and $fg.approved -eq [bool]$MenuForegroundApproved -and $fg.attempts -in @(0,1);$summary.foreground_requested=$fg.attempts -gt 0}
                for($i=0;$i -lt 4;$i++){
                    $p=Join-Path $case "menu-default-window-$i.json";$w=Join-Path $case "menu-default-observed-$i.json"
                    if(-not(Test-Path $p) -or -not(Test-Path $w)){$passed=$false;continue}
                    $s=Get-Content $p -Raw -Encoding utf8|ConvertFrom-Json;$wm=Get-Content $w -Raw -Encoding utf8|ConvertFrom-Json;$trace=$wm.trace
                    $passed=$passed -and $s.success -and $s.stage -eq $i -and $s.pid -eq $launch.pid -and $s.hwnd -eq $first.hwnd -and $s.input_sent -eq ($i -lt 2) -and $wm.pid -eq $launch.pid -and $wm.stage -eq $i -and $trace.complete -and $null -eq $trace.error
                }
            }
            if($Scenario -eq 'MenuDefault' -and $MenuEditProbe){
                # Two real clicks: SelectAll then the fixed Delete command; the
                # witness is the fixture page's emptied-editable title.
                $d=$observed.menu_default
                $passed=$passed -and $null -ne $d -and $d.success -and $d.phase -eq 4 -and $d.edit_probe -eq $true -and $d.edit_verified -eq $true -and -not $d.dismiss_probe -and -not $d.synthetic_cef_input -and $d.native.opened -eq 2 -and $d.native.published -eq 2 -and $d.native.actions -eq 2 -and $d.native.post_failures -eq 0 -and $null -eq $d.native.first_failure -and $d.visual.retired -and $d.visual.pending -eq 0 -and $null -eq $d.visual.first_error
                if($null -ne $d){$fg=$d.initial_foreground;$passed=$passed -and $fg.approved -eq [bool]$MenuForegroundApproved -and $fg.attempts -in @(0,1);$summary.foreground_requested=$fg.attempts -gt 0}
                for($i=0;$i -lt 6;$i++){
                    $p=Join-Path $case "menu-default-window-$i.json";$w=Join-Path $case "menu-default-observed-$i.json"
                    if(-not(Test-Path $p) -or -not(Test-Path $w)){$passed=$false;continue}
                    $s=Get-Content $p -Raw -Encoding utf8|ConvertFrom-Json;$wm=Get-Content $w -Raw -Encoding utf8|ConvertFrom-Json;$trace=$wm.trace
                    $input=$i -lt 4
                    $passed=$passed -and $s.success -and $s.stage -eq $i -and $s.pid -eq $launch.pid -and $s.hwnd -eq $first.hwnd -and $s.input_sent -eq $input -and $wm.pid -eq $launch.pid -and $wm.stage -eq $i -and $wm.window.hwnd -eq $s.hwnd -and $trace.complete -and $null -eq $trace.error
                    if($input){$passed=$passed -and @($trace.raw).Count -eq 2 -and @($s.button_observations).Count -eq 2 -and $null -eq $s.possibly_held -and $s.process_identity_verified}
                    if($i -in @(0,2)){$passed=$passed -and (@($trace.raw.message) -join ',') -eq '516,517'}
                    if($i -in @(1,3)){$passed=$passed -and (@($trace.raw.message) -join ',') -eq '513,514'}
                }
            }
            if($Scenario -eq 'MenuDefault' -and $MenuSourceProbe){
                # Same four stages as the DevTools row, but the auxiliary is the
                # source viewer and the witness is its own created counter.
                $d=$observed.menu_default
                $passed=$passed -and $null -ne $d -and $d.success -and $d.phase -eq 2 -and $d.source_probe -eq $true -and [int64]$d.source_started -ge 1 -and -not $d.dismiss_probe -and -not $d.synthetic_cef_input -and $d.native.opened -eq 1 -and $d.native.published -eq 1 -and $d.native.actions -eq 1 -and $d.native.dismissed -eq 1 -and $d.native.post_failures -eq 0 -and $null -eq $d.native.first_failure -and $d.visual.retired -and $d.visual.pending -eq 0 -and $null -eq $d.visual.first_error -and $d.visual.last_reply.requested -eq 'Select { command_id: 132 }' -and $d.visual.last_reply.accepted -eq $true
                if($null -ne $d){$fg=$d.initial_foreground;$passed=$passed -and $fg.approved -eq [bool]$MenuForegroundApproved -and $fg.attempts -in @(0,1);$summary.foreground_requested=$fg.attempts -gt 0}
                for($i=0;$i -lt 4;$i++){
                    $p=Join-Path $case "menu-default-window-$i.json";$w=Join-Path $case "menu-default-observed-$i.json"
                    if(-not(Test-Path $p) -or -not(Test-Path $w)){$passed=$false;continue}
                    $s=Get-Content $p -Raw -Encoding utf8|ConvertFrom-Json;$wm=Get-Content $w -Raw -Encoding utf8|ConvertFrom-Json;$trace=$wm.trace
                    $passed=$passed -and $s.success -and $s.stage -eq $i -and $s.pid -eq $launch.pid -and $s.hwnd -eq $first.hwnd -and $s.input_sent -eq ($i -lt 2) -and $wm.pid -eq $launch.pid -and $wm.stage -eq $i -and $wm.window.hwnd -eq $s.hwnd -and $trace.complete -and $null -eq $trace.error
                    if($i -lt 2){$passed=$passed -and @($trace.raw).Count -eq 2 -and @($s.button_observations).Count -eq 2 -and $null -eq $s.possibly_held -and $s.process_identity_verified}
                    if($i -eq 0){$passed=$passed -and (@($trace.raw.message) -join ',') -eq '516,517'}
                    if($i -eq 1){$passed=$passed -and (@($trace.raw.message) -join ',') -eq '513,514'}
                }
            }
            if($Scenario -eq 'MenuDefault' -and $MenuFindProbe){
                # Six real-click stages: open, SelectAll, open again, click Find,
                # teardown, terminal. Find(selection) needs the selection first.
                $d=$observed.menu_default
                $passed=$passed -and $null -ne $d -and $d.success -and $d.phase -eq 4 -and $d.find_probe -eq $true -and [int64]$d.find_started -ge 1 -and -not $d.dismiss_probe -and -not $d.synthetic_cef_input -and $d.native.opened -eq 2 -and $d.native.published -eq 2 -and $d.native.actions -eq 2 -and $d.native.post_failures -eq 0 -and $null -eq $d.native.first_failure -and $d.visual.retired -and $d.visual.pending -eq 0 -and $null -eq $d.visual.first_error
                if($null -ne $d){$fg=$d.initial_foreground;$passed=$passed -and $fg.approved -eq [bool]$MenuForegroundApproved -and $fg.attempts -in @(0,1);$summary.foreground_requested=$fg.attempts -gt 0}
                for($i=0;$i -lt 6;$i++){
                    $p=Join-Path $case "menu-default-window-$i.json";$w=Join-Path $case "menu-default-observed-$i.json"
                    if(-not(Test-Path $p) -or -not(Test-Path $w)){$passed=$false;continue}
                    $s=Get-Content $p -Raw -Encoding utf8|ConvertFrom-Json;$wm=Get-Content $w -Raw -Encoding utf8|ConvertFrom-Json;$trace=$wm.trace
                    $input=$i -lt 4
                    $passed=$passed -and $s.success -and $s.stage -eq $i -and $s.pid -eq $launch.pid -and $s.hwnd -eq $first.hwnd -and $s.input_sent -eq $input -and $wm.pid -eq $launch.pid -and $wm.stage -eq $i -and $wm.window.hwnd -eq $s.hwnd -and $trace.complete -and $null -eq $trace.error
                    if($input){$passed=$passed -and @($trace.raw).Count -eq 2 -and $s.process_identity_verified -and @($s.button_observations).Count -eq 2 -and $null -eq $s.possibly_held}
                    if($i -in @(0,2)){$passed=$passed -and (@($trace.raw.message) -join ',') -eq '516,517'}
                    if($i -in @(1,3)){$passed=$passed -and (@($trace.raw.message) -join ',') -eq '513,514'}
                }
            }
            if($Scenario -eq 'MenuDefault' -and $MenuSubmenuProbe){
                # Real hover into the one-level submenu, then a real click on its
                # child row. The child row only exists in the host model.
                $d=$observed.menu_default
                $passed=$passed -and $null -ne $d -and $d.success -and $d.phase -eq 4 -and $d.structure_probe -eq $true -and $d.structure_selected -eq $true -and $d.host_menu_item -eq 28501 -and (@($d.clicked_rows) -join ',') -eq '28501' -and $d.menu_submenu_from_cef -eq $true -and -not $d.synthetic_cef_input -and $d.real_widget_path -eq $true -and $d.native.opened -eq 1 -and $d.native.actions -eq 1 -and $d.native.dismissed -eq 1 -and $d.native.post_failures -eq 0 -and $null -eq $d.native.first_failure -and $d.visual.retired -and $null -eq $d.visual.first_error -and $d.visual.last_reply.requested -eq 'Select { command_id: 28501 }' -and $d.visual.last_reply.accepted -eq $true
                if($null -ne $d){$fg=$d.initial_foreground;$passed=$passed -and $fg.approved -eq [bool]$MenuForegroundApproved -and $fg.attempts -in @(0,1);$summary.foreground_requested=$fg.attempts -gt 0}
            }
            if($Scenario -eq 'MenuDefault' -and $MenuStructureProbe){
                # Two real-click stages: open the menu, click the host checkable
                # row. CEF's own model has no checkable item, so the row only
                # exists because the host opted into structural items.
                $d=$observed.menu_default
                $passed=$passed -and $null -ne $d -and $d.success -and $d.phase -eq 4 -and $d.structure_probe -eq $true -and $d.structure_selected -eq $true -and $d.host_menu_item -eq 28500 -and (@($d.clicked_rows) -join ',') -eq '28500' -and $d.menu_checked_from_cef -eq $true -and $d.menu_submenu_from_cef -eq $true -and -not $d.synthetic_cef_input -and $d.real_widget_path -eq $true -and $d.native.opened -eq 1 -and $d.native.actions -eq 1 -and $d.native.dismissed -eq 1 -and $d.native.post_failures -eq 0 -and $null -eq $d.native.first_failure -and $d.visual.retired -and $d.visual.pending -eq 0 -and $null -eq $d.visual.first_error -and $d.visual.last_reply.requested -eq 'Select { command_id: 28500 }' -and $d.visual.last_reply.accepted -eq $true
                if($null -ne $d){$fg=$d.initial_foreground;$passed=$passed -and $fg.approved -eq [bool]$MenuForegroundApproved -and $fg.attempts -in @(0,1);$summary.foreground_requested=$fg.attempts -gt 0}
                for($i=0;$i -lt 2;$i++){
                    $p=Join-Path $case "menu-default-window-$i.json";$w=Join-Path $case "menu-default-observed-$i.json"
                    if(-not(Test-Path $p) -or -not(Test-Path $w)){$passed=$false;continue}
                    $s=Get-Content $p -Raw -Encoding utf8|ConvertFrom-Json;$wm=Get-Content $w -Raw -Encoding utf8|ConvertFrom-Json;$trace=$wm.trace
                    $passed=$passed -and $s.success -and $s.stage -eq $i -and $s.pid -eq $launch.pid -and $s.hwnd -eq $first.hwnd -and $s.input_sent -and $wm.pid -eq $launch.pid -and $wm.stage -eq $i -and $wm.window.hwnd -eq $s.hwnd -and $trace.complete -and $null -eq $trace.error -and @($trace.raw).Count -eq 2 -and $s.process_identity_verified -and @($s.button_observations).Count -eq 2 -and $null -eq $s.possibly_held
                    if($i -eq 0){$passed=$passed -and (@($trace.raw.message) -join ',') -eq '516,517'}
                    if($i -eq 1){$passed=$passed -and (@($trace.raw.message) -join ',') -eq '513,514'}
                }
            }
            if($Scenario -eq 'MenuDefault' -and $MenuDevtoolsProbe){
                # Two real-click stages: open the menu, click CefSharp's
                # "Show DevTools (Inspect)" row, then require the browser host's
                # own association answer. No DOM title effect is involved.
                $d=$observed.menu_default
                $passed=$passed -and $null -ne $d -and $d.success -and $d.phase -eq 2 -and $d.devtools_probe -eq $true -and $d.devtools_associated_after_menu -eq $true -and -not $d.dismiss_probe -and -not $d.synthetic_cef_input -and $d.native.opened -eq 1 -and $d.native.published -eq 1 -and $d.native.actions -eq 1 -and $d.native.dismissed -eq 1 -and $d.native.post_failures -eq 0 -and $null -eq $d.native.first_failure -and $d.visual.retired -and $d.visual.pending -eq 0 -and $null -eq $d.visual.first_error -and $d.visual.last_reply.requested -eq 'Select { command_id: 28440 }' -and $d.visual.last_reply.accepted -eq $true
                if($null -ne $d){$fg=$d.initial_foreground;$passed=$passed -and $fg.approved -eq [bool]$MenuForegroundApproved -and $fg.attempts -in @(0,1);$summary.foreground_requested=$fg.attempts -gt 0}
                for($i=0;$i -lt 2;$i++){
                    $p=Join-Path $case "menu-default-window-$i.json";$w=Join-Path $case "menu-default-observed-$i.json"
                    if(-not(Test-Path $p) -or -not(Test-Path $w)){$passed=$false;continue}
                    $s=Get-Content $p -Raw -Encoding utf8|ConvertFrom-Json;$wm=Get-Content $w -Raw -Encoding utf8|ConvertFrom-Json;$trace=$wm.trace
                    $passed=$passed -and $s.success -and $s.stage -eq $i -and $s.pid -eq $launch.pid -and $s.hwnd -eq $first.hwnd -and -not $s.foreground_requested -and $s.input_sent -and $wm.pid -eq $launch.pid -and $wm.stage -eq $i -and $wm.window.hwnd -eq $s.hwnd -and $trace.complete -and $null -eq $trace.error -and @($trace.expected).Count -eq 2 -and @($trace.observed).Count -eq 2 -and @($trace.raw).Count -eq 2 -and $s.process_identity_verified -and @($s.button_observations).Count -eq 2 -and $null -eq $s.possibly_held
                    if($i -eq 0){$passed=$passed -and (@($trace.raw.message) -join ',') -eq '516,517'}
                    if($i -eq 1){$passed=$passed -and (@($trace.raw.message) -join ',') -eq '513,514'}
                }
            }
            if($MenuDismissProbe){
                $d=$observed.menu_default
                $passed=$passed -and $d.success -and $d.phase -eq 5 -and $d.dismiss_probe -and $d.widget_dismiss_verified -and -not $d.widget_select_all_verified -and $d.real_widget_path -and -not $d.full_menu_matrix -and -not $d.synthetic_cef_input -and $d.native.opened -eq 2 -and $d.native.published -eq 2 -and $d.native.cancelled -eq 2 -and $d.native.dismissed -eq 2 -and $d.native.actions -eq 0 -and $d.native.post_failures -eq 0 -and $null -eq $d.native.first_failure -and $d.visual.retired -and $d.visual.pending -eq 0 -and $null -eq $d.visual.first_error
                $fg=$d.initial_foreground;$passed=$passed -and $fg.approved -and $fg.attempts -in @(0,1) -and $fg.after -and $fg.policy -eq 'one-ordinary-owned-request';$summary.foreground_requested=$fg.attempts -gt 0
                for($i=0;$i -lt 5;$i++){
                    $sp=Join-Path $case "menu-dismiss-window-$i.json";$wp=Join-Path $case "menu-default-observed-$i.json"
                    if(-not(Test-Path $sp) -or -not(Test-Path $wp)){$passed=$false;continue}
                    $shot=Get-Content $sp -Raw -Encoding utf8|ConvertFrom-Json;$wm=Get-Content $wp -Raw -Encoding utf8|ConvertFrom-Json;$trace=$wm.trace
                    $count=if($i -eq 1){3}elseif($i -eq 4){0}else{2};$eventCount=if($i -eq 4){0}else{2}
                    $passed=$passed -and $shot.success -and $shot.pid -eq $launch.pid -and $shot.stage -eq $i -and $shot.hwnd -eq $first.hwnd -and $shot.process_identity_verified -and -not $shot.foreground_requested -and $shot.input_sent -eq ($i -lt 4) -and $null -eq $shot.possibly_held -and -not $shot.physical_release_proof -and -not $shot.async_zero_authoritative -and @($shot.events).Count -eq $eventCount -and $wm.pid -eq $launch.pid -and $wm.stage -eq $i -and $wm.window.hwnd -eq $shot.hwnd -and $trace.complete -and $null -eq $trace.error -and @($trace.raw).Count -eq $count -and @($trace.expected).Count -eq $count -and @($trace.observed).Count -eq $count
                    foreach($raw in @($trace.raw)){
                        $passed=$passed -and $raw.accepted -and -not $raw.during_manual_pump
                        if($raw.message -eq 258){$passed=$passed -and $i -eq 1 -and $raw.char_associated -and $raw.wparam -eq 27 -and $raw.observation_point -eq 'removed-queue'}else{$passed=$passed -and $raw.raw_extra_info -eq 1128678706}
                    }
                    if($i -in @(0,2)){$passed=$passed -and (@($trace.raw.message)-join ',') -eq '516,517'}
                    if($i -eq 1){$passed=$passed -and (@($trace.raw.message)-join ',') -eq '256,258,257' -and $shot.menu_contrast_samples -ge 12 -and $null -ne $shot.escape_prefix}
                    if($i -eq 3){$passed=$passed -and (@($trace.raw.message)-join ',') -eq '513,514' -and $shot.menu_contrast_samples -ge 12}
                    if($i -eq 2){$passed=$passed -and $shot.metadata.native.cancelled -eq 1 -and $shot.metadata.native.dismissed -eq 1 -and $shot.metadata.native.actions -eq 0}
                    if($i -eq 4){$passed=$passed -and $shot.metadata.native.cancelled -eq 2 -and $shot.metadata.native.dismissed -eq 2 -and $shot.metadata.weak_released -and $shot.metadata.browser_closed -and $shot.metadata.host_alive}
                }
            }
            if($Scenario -eq 'SourceViewer'){
                $passed=$passed -and $observed.source_viewer_verified -eq $true -and $null -ne $observed.source_viewer -and $observed.source_viewer.success -eq $true -and $observed.source_viewer.phase -eq 3 -and $observed.source_viewer.source_bytes -eq 392 -and ([decimal]$observed.source_viewer.source_hash) -eq 14904330143978622504d -and $observed.source_viewer.real_source_getter -and $observed.source_viewer.real_gpui_viewer -and $observed.source_viewer.automatic_view_release_cleanup -and $observed.source_viewer.route_unregistered_after_drain -and -not $observed.source_viewer.menu_widget_tested -and -not $observed.source_viewer.full_view_source_matrix
                if($passed){
                    $sourceStart=Get-Content -LiteralPath (Join-Path $case 'source-viewer-stage-0.json') -Raw -Encoding UTF8 | ConvertFrom-Json
                    $sourceEnd=Get-Content -LiteralPath (Join-Path $case 'source-viewer-stage-1.json') -Raw -Encoding UTF8 | ConvertFrom-Json
                    $passed=Test-SourceViewerIdentity $observed $sourceStart $sourceEnd $first $detached
                }
            }
            if($MenuServicesProbe){
                $metaPath=Join-Path $case 'menu-services-lifecycle.json'
                $meta=if(Test-Path -LiteralPath $metaPath){Get-Content -Raw -Encoding UTF8 $metaPath|ConvertFrom-Json}else{$null}
                if($null -eq $meta){
                    $passed=$false
                    if($null -eq $summary.error -and $null -ne $launch.observer_error){$summary.error=[string]$launch.observer_error}
                }else{
                $passed=$passed -and $observed.menu_services_probe -and $observed.menu_services_verified -and $meta.schema -eq 1 -and $meta.pid -eq $launch.pid -and $meta.success -and $meta.scope -eq 'real-view-entity-release-only' -and $meta.window_id -gt 0 -and -not $meta.before_retired -and $meta.after_retired -and $meta.in_flight -eq 0 -and $meta.weak_entity_released -and $meta.duplicate_registration_refused -and $meta.delivery_capture_rejected -and $meta.cleanup_receipt_retired -and -not $meta.native_menu_callbacks_tested -and -not $meta.menu_focus_widget_tested -and -not $meta.selection_execution_tested
                }
            }
            if($Scenario -eq 'Popup') {
                $passed=$passed -and $observed.scenario -eq 'Popup' -and $null -ne $observed.popup -and $observed.popup.success -eq $true -and $observed.popup.stages -eq 9
                foreach($stage in 0..8) {
                    $shotPath=Join-Path $case "popup-window-$stage.json"
                    if(-not (Test-Path -LiteralPath $shotPath)){$passed=$false;continue}
                    $shot=Get-Content -LiteralPath $shotPath -Raw -Encoding UTF8 | ConvertFrom-Json
                    $passed=$passed -and $shot.success -eq $true -and $shot.pid -eq $launch.pid
                }
            }
            if($Scenario -eq 'Input') {
                $passed=$passed -and $observed.scenario -eq 'Input' -and $null -ne $observed.input -and $observed.input.success -eq $true -and $observed.input.stages -eq 6
                if($null -ne $observed.input -and $observed.input.success -eq $true) {
                    $peerRelease=$observed.input.peer_release
                    $passed=$passed -and $peerRelease.created_images -gt 0 -and $peerRelease.created_images -eq $peerRelease.released_images -and $observed.input.peer_entity_released -eq $true -and $observed.input.peer_release_observed_closing -eq $true
                }
                if($null -ne $observed.input -and $null -ne $observed.input.PSObject.Properties['native_keyboard']) {
                    $keyboard=$observed.input.native_keyboard
                    $passed=$passed -and $keyboard.routed_down -eq 3 -and $keyboard.routed_up -eq 3 -and $keyboard.refused -eq 0 -and $keyboard.unclaimed -eq 0 -and $keyboard.panics -eq 0 -and $null -eq $keyboard.last_failure -and $keyboard.cleanup_failures -eq 0 -and $keyboard.retired -eq $true -and $keyboard.hook_live -eq $false -and $keyboard.lease_live -eq $false -and $keyboard.properties_live -eq 0
                } else {
                    # Early failures (e.g. ordinary foreground denied before
                    # stages) remain failures; do not mask their actual cause
                    # with a StrictMode missing-optional-evidence exception.
                    $passed=$false
                }
                foreach($stage in 0..5) {
                    $shotPath=Join-Path $case "input-window-$stage.json"
                    if(-not (Test-Path -LiteralPath $shotPath)){$passed=$false;continue}
                    $shot=Get-Content -LiteralPath $shotPath -Raw -Encoding UTF8 | ConvertFrom-Json
                    $passed=$passed -and $shot.success -eq $true -and $shot.pid -eq $launch.pid -and $shot.stage -eq $stage -and @($shot.hwnds).Count -eq 2 -and ((@($shot.hwnds|Sort-Object) -join ',') -eq (@($first.hwnds|Sort-Object) -join ','))
                    $wmPath=Join-Path $case "input-observed-$stage.json"
                    if(-not (Test-Path -LiteralPath $wmPath)){$passed=$false;continue}
                    $wm=Get-Content -LiteralPath $wmPath -Raw -Encoding UTF8|ConvertFrom-Json
                    $passed=$passed -and $wm.schema -eq 1 -and $wm.kind -eq 'component-input-wm' -and $wm.pid -eq $launch.pid -and $wm.stage -eq $stage -and $wm.success -eq $true -and @($wm.windows).Count -eq 2 -and $wm.expected_marker -eq 1128678706
                    if($stage -le 3) {
                        $metaPath=Join-Path $case "input-stage-$stage.json"
                        if(-not (Test-Path -LiteralPath $metaPath)){$passed=$false;continue}
                        $stageMeta=Get-Content -LiteralPath $metaPath -Raw -Encoding UTF8|ConvertFrom-Json
                        $passed=$passed -and $stageMeta.pid -eq $launch.pid -and $stageMeta.stage -eq $stage
                    }
                    if($stage -in @(0,1,3)) {
                        $imePath=Join-Path $case "input-owner-ime-$stage.json"
                        if(-not (Test-Path -LiteralPath $imePath)){$passed=$false;continue}
                        $ime=Get-Content -LiteralPath $imePath -Raw -Encoding UTF8|ConvertFrom-Json
                        $prefixCount=if($stage -eq 1){0}else{2}
                        $passed=$passed -and $ime.schema -eq 1 -and $ime.kind -eq 'input-owner-ime' -and $ime.pid -eq $launch.pid -and $ime.stage -eq $stage -and $ime.raw_count -eq $prefixCount -and $ime.keyboard_seen -eq $false -and $ime.accepts_text -eq $true
                        $passed=$passed -and $ime.request -eq "$($launch.pid):${stage}:$($stageMeta.window.hwnd)"
                        foreach($field in @('hwnd','pid','thread','dpi','left','top','width','height')) {
                            $passed=$passed -and $ime.window.$field -eq $stageMeta.window.$field
                        }
                    }
                    if($stage -eq 2) {
                        $peerPath=Join-Path $case 'input-owner-foreground-2.json'
                        if(-not (Test-Path -LiteralPath $peerPath)){$passed=$false;continue}
                        $peer=Get-Content -LiteralPath $peerPath -Raw -Encoding UTF8|ConvertFrom-Json
                        $passed=$passed -and $peer.schema -eq 1 -and $peer.kind -eq 'input-owner-foreground' -and $peer.pid -eq $launch.pid -and $peer.stage -eq 2 -and $peer.raw_count -eq 0 -and $peer.source_was_foreground -eq $true -and $peer.target_is_foreground -eq $true
                        $passed=$passed -and $peer.request -eq "$($launch.pid):2:$($stageMeta.window.hwnd):$($stageMeta.peer.hwnd)"
                        foreach($field in @('hwnd','pid','thread','dpi','left','top','width','height')) {
                            $passed=$passed -and $peer.source.$field -eq $stageMeta.window.$field -and $peer.target.$field -eq $stageMeta.peer.$field
                        }
                    }
                    foreach($entry in @($wm.windows)) {
                        $passed=$passed -and @($first.hwnds) -contains $entry.hwnd
                        $trace=$entry.trace
                        $passed=$passed -and $trace.complete -eq $true -and $null -eq $trace.error -and @($trace.expected).Count -eq @($trace.observed).Count
                        if($stage -ge 4){$passed=$passed -and @($trace.expected).Count -eq 0}
                        foreach($raw in @($trace.raw|Where-Object {$_.accepted -eq $true})) {
                            $point=if($raw.event.kind -in @('keydown','char','keyup')){'removed-queue'}else{'window-proc'}
                            $passed=$passed -and $raw.observation_point -eq $point
                            if($raw.event.kind -eq 'char'){$passed=$passed -and $raw.char_associated -eq $true}
                            else{$passed=$passed -and $raw.raw_extra_info -eq 1128678706}
                        }
                    }
                }
            }
            if($Scenario -eq 'Exhaustion') {
                $detail=$observed.exhaustion
                $passed=$passed -and $observed.scenario -eq 'Exhaustion' -and $null -ne $detail -and $detail.success -eq $true -and $detail.kind -eq 'recovery-exhaustion-slice' -and $detail.stages -eq 4 -and $detail.exhaustion_verified -eq $true -and $detail.task_11_3_accepted -eq $false
                for($i=0;$i -lt 4;$i++) {
                    $path=Join-Path $case "exhaustion-window-$i.json"
                    if(-not(Test-Path -LiteralPath $path)){$passed=$false;continue}
                    $s=Get-Content -LiteralPath $path -Raw -Encoding UTF8|ConvertFrom-Json
                    $required=@('success','pid','stage','hwnd','owner_thread','scope','task_11_3_accepted','input_sent','foreground_forced','metadata','capture')
                    if(@($required|Where-Object {$null -eq $s.PSObject.Properties[$_]}).Count -gt 0 -or $s.success -ne $true -or $null -eq $s.metadata -or $null -eq $s.capture){$passed=$false;continue}
                    $metaFields=@('fault','recovery','probe','stale','fault_label','quiet_observation_ms','detached','initial_foreground')
                    if(@($metaFields|Where-Object {$null -eq $s.metadata.PSObject.Properties[$_]}).Count -gt 0 -or $null -eq $s.metadata.fault -or $null -eq $s.metadata.recovery -or $null -eq $s.metadata.probe -or $null -eq $s.capture.PSObject.Properties['raw'] -or $null -eq $s.capture.PSObject.Properties['pixel_difference_bbox']){$passed=$false;continue}
                    if($null -eq $s.metadata.fault.PSObject.Properties['count'] -or $null -eq $s.metadata.fault.PSObject.Properties['event_sequence'] -or @(@('status','attempts')|Where-Object {$null -eq $s.metadata.recovery.PSObject.Properties[$_]}).Count -gt 0 -or $null -eq $s.metadata.probe.PSObject.Properties['drop_view']){$passed=$false;continue}
                    $passed=$passed -and $s.pid -eq $launch.pid -and $s.stage -eq $i -and $s.hwnd -eq $first.hwnd -and $s.owner_thread -eq $observed.main_thread -and $s.scope -eq 'recovery-exhaustion-slice' -and $s.task_11_3_accepted -eq $false -and $s.input_sent -eq $false -and $s.foreground_forced -eq $false -and $null -eq $s.capture.pixel_difference_bbox -and $s.metadata.stale -eq ($i -eq 1)
                    $fg=$s.metadata.initial_foreground
                    if($null -eq $fg -or @(@('approved','attempts','before','after','policy')|Where-Object {$null -eq $fg.PSObject.Properties[$_]}).Count -gt 0){$passed=$false;continue}
                    $passed=$passed -and $fg.approved -eq [bool]$InitialForegroundApproved -and $fg.attempts -in @(0,1)
                    if($ExhaustionManualForeground){
                        $passed=$passed -and $fg.approved -eq $false -and $fg.attempts -eq 0 -and $fg.before -is [bool] -and $fg.after -is [bool] -and $fg.after -eq $true -and $fg.policy -ceq 'manual-owned-observation'
                        $passed=$passed -and (Test-ManualOwnedStageFiles $case $s $i $launch.pid)
                    }elseif($InitialForegroundApproved){$passed=$passed -and $fg.after -eq $true -and $fg.policy -eq 'one-ordinary-owned-request'}else{$passed=$passed -and $fg.attempts -eq 0 -and $null -eq $fg.before -and $null -eq $fg.after -and $fg.policy -eq 'passive'}
                    if($i -gt 0){$passed=$passed -and $s.metadata.fault.count -eq 1 -and $s.metadata.fault.event_sequence -gt 0}
                    if($i -eq 1){$passed=$passed -and $s.metadata.recovery.status -eq 'Exhausted' -and $s.metadata.recovery.attempts -eq 8 -and $s.metadata.quiet_observation_ms -ge 250 -and $s.metadata.fault_label -eq 'STALE: resize recovery exhausted'}
                    if($i -eq 2){$passed=$passed -and $s.metadata.recovery.status -eq 'Recovered' -and $s.metadata.probe.drop_view -eq $false -and $null -eq $s.metadata.fault_label}
                    if($i -eq 3){$passed=$passed -and $s.metadata.detached -eq $true -and $null -eq $s.capture.raw}
                }
            }
            if($Scenario -eq 'Unfocused') {
                $detail=$observed.unfocused
                $passed=$passed -and $observed.scenario -eq 'Unfocused' -and $null -ne $detail -and $detail.kind -eq 'unfocused-animation-slice' -and $detail.success -eq $true -and $detail.stages -eq 3 -and $detail.unfocused_animation_verified -eq $true -and $detail.minimize_verified -eq $false -and $detail.task_11_3_accepted -eq $false -and $detail.keyboard_or_pointer_sent -eq $false
                for($i=0;$i -lt 3;$i++) {
                    $path=Join-Path $case "unfocused-window-$i.json"
                    if(-not(Test-Path -LiteralPath $path)){$passed=$false;continue}
                    $s=Get-Content -LiteralPath $path -Raw -Encoding UTF8|ConvertFrom-Json
                    if($s.success -ne $true -or $null -eq $s.PSObject.Properties['metadata'] -or $null -eq $s.PSObject.Properties['capture']){$passed=$false;continue}
                    $passed=$passed -and $s.pid -eq $launch.pid -and $s.stage -eq $i -and $s.scope -eq 'unfocused-animation-slice' -and $s.hwnd -eq $first.hwnd -and $s.owner_thread -eq $observed.main_thread -and $s.task_11_3_accepted -eq $false -and $s.keyboard_or_pointer_sent -eq $false -and $s.foreground_forced -eq $false -and $s.metadata.visible -eq $true -and $s.metadata.active -eq ($i -eq 0) -and $null -eq $s.capture.pixel_difference_bbox -and @($s.hwnds).Count -eq 2 -and @($s.hwnds|Select-Object -Unique).Count -eq 2
                    if($i -gt 0){$passed=$passed -and $s.foreground_hwnd -eq $s.hwnds[1] -and @($s.metadata.samples).Count -eq 3 -and @($s.sample_artifacts).Count -eq 3}
                    if($i -eq 2){$passed=$passed -and $null -eq $s.capture.raw -and $s.metadata.detached -eq $true}
                }
            }
            if($Scenario -eq 'Visibility') {
                $detail=if($null -ne $observed.PSObject.Properties['visibility']){$observed.visibility}else{$null}
                if($null -eq $detail -or @(@('kind','success','stages','task_11_3_accepted')|Where-Object {$null -eq $detail.PSObject.Properties[$_]}).Count -gt 0){$passed=$false}else{
                    $passed=$passed -and $observed.scenario -eq 'Visibility' -and $detail.kind -eq 'host-owned-visibility-slice' -and $detail.success -eq $true -and $detail.stages -eq 4 -and $detail.task_11_3_accepted -eq $false
                }
                for($i=0;$i -lt 4;$i++) {
                    $path=Join-Path $case "visibility-window-$i.json"
                    if(-not(Test-Path -LiteralPath $path)){$passed=$false;continue}
                    $s=Get-Content $path -Raw -Encoding UTF8|ConvertFrom-Json
                    $required=@('success','pid','stage','hwnd','owner_thread','scope','task_11_3_accepted','input_sent','foreground_forced','observer_resized','metadata','capture')
                    if(@($required|Where-Object {$null -eq $s.PSObject.Properties[$_]}).Count -gt 0 -or $s.success -ne $true){$passed=$false;continue}
                    $passed=$passed -and $s.pid -eq $launch.pid -and $s.stage -eq $i -and $s.hwnd -eq $first.hwnd -and $s.owner_thread -eq $observed.main_thread -and $s.scope -eq 'host-owned-visibility-slice' -and $s.task_11_3_accepted -eq $false -and $s.input_sent -eq $false -and $s.foreground_forced -eq $false -and $s.observer_resized -eq $false
                    if($null -eq $s.metadata -or $null -eq $s.metadata.PSObject.Properties['bridge']){$passed=$false;continue}
                    if($VisibilityManualForeground -and $i -eq 0){$passed=$passed -and (Test-ManualOwnedStageFiles $case $s $i $launch.pid -Family visibility)}
                    $b=$s.metadata.bridge
                    if($null -eq $b -or @(@('phase','minimize_requests','restore_requests','quiet_ms','quiet_ms_frozen')|Where-Object {$null -eq $b.PSObject.Properties[$_]}).Count -gt 0){$passed=$false;continue}
                    $passed=$passed -and $b.phase -eq @('Ready','Hidden','SourceRecovered','Retired')[$i] -and $b.minimize_requests -eq [int]($i -gt 0) -and $b.restore_requests -eq [int]($i -ge 2)
                    $passed=$passed -and $b.quiet_ms_frozen -eq ($i -ge 2)
                    if($i -ge 2){$passed=$passed -and $b.quiet_ms -ge 250}
                    if($i -eq 1){
                        if($null -eq $s.PSObject.Properties['hidden_identity'] -or $null -eq $s.hidden_identity -or $null -eq $s.hidden_identity.PSObject.Properties['is_iconic']){$passed=$false;continue}
                        $passed=$passed -and $null -eq $s.capture -and $s.hidden_identity.is_iconic -eq $true -and $b.quiet_ms -ge 250
                    }else{
                        if($null -eq $s.capture -or $null -eq $s.capture.PSObject.Properties['pixel_difference_bbox']){$passed=$false;continue}
                        $passed=$passed -and $null -eq $s.capture.pixel_difference_bbox
                    }
                }
            }
            if($Scenario -eq 'Resize') {
                $detail=$observed.resize
                $passed=$passed -and $observed.scenario -eq 'Resize' -and $null -ne $detail -and $detail.kind -eq 'window-layout-resize-slice' -and $detail.success -eq $true -and $detail.stages -eq 6 -and $detail.task_11_3_accepted -eq $false
                if($ResizePassive){$passed=$passed -and $detail.passive_noactivation_resize -is [bool] -and $detail.passive_noactivation_resize}
                if($ResizePassiveRight){$passed=$passed -and $detail.passive_right_initial_placement -is [bool] -and $detail.passive_right_initial_placement}
                $priorGeneration=0L; $priorSequence=0L; $resizeStages=@()
                for($i=0; $i -lt 6; $i++) {
                    $path=Join-Path $case "resize-window-$i.json"
                    if(-not (Test-Path -LiteralPath $path)){$passed=$false;continue}
                    $s=Get-Content -LiteralPath $path -Raw -Encoding UTF8|ConvertFrom-Json
                    # Early observer failures intentionally have no capture/metadata.
                    # Keep that original failure, not a StrictMode property exception.
                    if($s.success -ne $true -or $null -eq $s.PSObject.Properties['metadata'] -or $null -eq $s.PSObject.Properties['capture']){$passed=$false;continue}
                    if (-not (Test-ResizeStageFiles $case $s $i)) { Write-Host "Resize stage $i artifact bytes did not match its receipt"; $passed=$false; continue }
                    if($ResizeManualForeground -and $i -eq 0){$passed=$passed -and (Test-ManualOwnedStageFiles $case $s 0 $launch.pid -Family resize)}
                    if($ResizePassive){$passed=$passed -and $s.passive_unfocused_pre -is [bool] -and $s.passive_unfocused_pre -and $s.passive_unfocused_post -is [bool] -and $s.passive_unfocused_post}
                    $passed=$passed -and $s.success -eq $true -and $s.pid -eq $launch.pid -and $s.stage -eq $i -and $s.hwnd -eq $first.hwnd -and $s.owner_thread -eq $observed.main_thread -and $s.scope -eq 'window-layout-resize-slice' -and $s.task_11_3_accepted -eq $false -and $s.input_sent -eq $false -and $s.foreground_forced -eq $false -and $null -eq $s.capture.pixel_difference_bbox
                    if($i -gt 0){$passed=$passed -and $s.actual_client_size_changed -eq ($i -lt 5)}
                    if($i -lt 5){
                        $raw=$s.metadata.view_raw
                        if($null -eq $raw -or $null -eq $raw.PSObject.Properties['generation'] -or $null -eq $raw.PSObject.Properties['sequence']){$passed=$false;continue}
                        $passed=$passed -and $null -ne $s.capture.raw -and $raw.generation -gt $priorGeneration -and $raw.sequence -gt $priorSequence
                        $priorGeneration=$raw.generation;$priorSequence=$raw.sequence
                    }else{$passed=$passed -and $null -eq $s.capture.raw -and $s.metadata.detached -eq $true}
                    $resizeStages += $s
                }
                if($passed -and $ResizeVerifiedTransitions -and $selectedBackend -ceq 'D3d11'){
                    $passed=Test-ResizeVerifiedTransitions $observed $resizeStages
                }
            }
            if($Scenario -eq 'ImeObserve') {
                $detail=$observed.input
                $passed=$passed -and $observed.scenario -eq 'ImeObserve' -and $null -ne $detail
                if($null -ne $detail -and $detail.success -eq $true) {
                    $keyboard=$detail.native_keyboard
                    # Both owned windows now compose the fixed text; the pointer-only
                    # probe counts below still have to hold for the same run.
                    $passed=$passed -and $detail.kind -eq 'ime-compose-two-window' -and $detail.stages -eq 4 -and $detail.task_11_2_accepted -eq $false -and $detail.native_chinese_verified -eq $true -and $detail.keyboard_input_sent -eq $true
                    # Six marked keys were sent to each of the two owned windows.
                    $passed=$passed -and $keyboard.routed_down -eq 12 -and $keyboard.routed_up -eq 12 -and $keyboard.orphan_up -eq 0
                    # Compose may commit before the peer ever allocates an image;
                    # balance, not a nonzero count, is what retirement requires.
                    $passed=$passed -and $detail.peer_release.created_images -eq $detail.peer_release.released_images -and $detail.peer_entity_released -eq $true -and $detail.peer_release_observed_closing -eq $true
                    $passed=$passed -and $keyboard.unclaimed -eq 0 -and $keyboard.host_shortcuts -eq 0 -and $keyboard.orphan_up -eq 0 -and $keyboard.system_text_skipped -eq 0 -and $keyboard.refused -eq 0 -and $keyboard.panics -eq 0 -and $keyboard.cleanup_failures -eq 0 -and $null -eq $keyboard.last_failure -and $keyboard.retired -eq $true -and $keyboard.hook_live -eq $false -and $keyboard.lease_live -eq $false -and $keyboard.properties_live -eq 0
                } else {$passed=$false}
                foreach($stage in 0..3) {
                    $paths=@("ime-observe-window-$stage.json","ime-observe-stage-$stage.json","ime-observe-owner-$stage.json","ime-observe-observed-$stage.json")
                    if(@($paths|Where-Object {-not (Test-Path -LiteralPath (Join-Path $case $_))}).Count -gt 0){$passed=$false;continue}
                    $shot=Get-Content (Join-Path $case $paths[0]) -Raw -Encoding UTF8|ConvertFrom-Json
                    $meta=Get-Content (Join-Path $case $paths[1]) -Raw -Encoding UTF8|ConvertFrom-Json
                    $owner=Get-Content (Join-Path $case $paths[2]) -Raw -Encoding UTF8|ConvertFrom-Json
                    $wm=Get-Content (Join-Path $case $paths[3]) -Raw -Encoding UTF8|ConvertFrom-Json
                    if($stage -lt 2) {
                        $passed=$passed -and $shot.success -eq $true -and $shot.pid -eq $launch.pid -and $shot.stage -eq $stage -and $shot.scope -eq 'ime-environment-only' -and $shot.task_11_2_accepted -eq $false -and $shot.native_chinese_verified -eq $false -and $shot.keyboard_input_sent -eq $false -and $shot.mode_changed -eq $false
                    } else {
                        $passed=$passed -and $shot.success -eq $true -and $shot.pid -eq $launch.pid -and $shot.stage -eq $stage -and $shot.scope -eq 'ime-compose-two-window' -and $shot.task_11_2_accepted -eq $false -and $shot.keyboard_input_sent -eq $true -and $shot.mode_changed -eq $false
                    }
                    $passed=$passed -and $meta.pid -eq $launch.pid -and $meta.stage -eq $stage -and $owner.schema -eq 1 -and $owner.kind -eq 'ime-environment-owner' -and $owner.pid -eq $launch.pid -and $owner.stage -eq $stage -and $owner.request -eq "$($launch.pid):${stage}:$($meta.window.hwnd)"
                    if($stage -lt 2) {
                        $passed=$passed -and $owner.raw_count -eq 2 -and $owner.keyboard_seen -eq $false -and $owner.accepts_text -eq $true -and $owner.foreground -eq $true -and $owner.keyboard_input_sent -eq $false -and $owner.mode_changed -eq $false
                    } else {
                        # Real marked keys: 2 pointer + 6 down/up pairs.
                        $passed=$passed -and $owner.raw_count -eq 14 -and $owner.keyboard_seen -eq $true -and $owner.accepts_text -eq $true -and $owner.foreground -eq $true -and $owner.mode_changed -eq $false
                        # 3619451238 exceeds Int32; the DOM hash is a u32.
                        $n=2; [long]$h=3619451238
                        $fields=@{}
                        foreach($part in ([string]$owner.dom_title).Split('|')) { $kv=$part.Split('=',2); if($kv.Count -eq 2){$fields[$kv[0]]=[long]$kv[1]} }
                        $passed=$passed -and $fields['n'] -eq $n -and $fields['h'] -eq $h -and $fields['cs'] -eq 1 -and $fields['ce'] -eq 1 -and $fields['kd'] -eq 6 -and $fields['ku'] -eq 6
                        $cb=$owner.component_callbacks
                        $passed=$passed -and $cb.key_down_entered -eq 6 -and $cb.replace_text_entered -eq 1 -and $cb.key_down_rejected -eq 0 -and $cb.key_up_rejected -eq 0 -and $cb.replace_text_rejected -eq 0 -and $cb.replace_and_mark_text_rejected -eq 0 -and $null -eq $cb.last_rejection
                    }
                    foreach($field in @('hwnd','pid','thread','dpi','left','top','width','height')){$passed=$passed -and $owner.window.$field -eq $meta.window.$field}
                    $passed=$passed -and $wm.schema -eq 1 -and $wm.kind -eq 'ime-environment-wm' -and $wm.pid -eq $launch.pid -and $wm.stage -eq $stage -and $wm.success -eq $true -and $wm.expected_marker -eq 1128678706 -and $wm.keyboard_input_sent -eq $false -and $wm.task_11_2_accepted -eq $false -and @($wm.windows).Count -eq 2
                    $passed=$passed -and @($wm.windows.hwnd|Select-Object -Unique).Count -eq 2 -and ((@($wm.windows.hwnd|Sort-Object) -join ',') -eq (@($first.hwnds|Sort-Object) -join ','))
                    if($stage -ge 2) {
                        # Compose observes the real IME traffic instead of a
                        # predicted sequence: the target records every marked
                        # message, the idle peer must record none.
                        foreach($entry in @($wm.windows)) {
                            $trace=$entry.trace
                            $expected=if($entry.hwnd -eq $meta.window.hwnd){14}else{0}
                            $passed=$passed -and @($first.hwnds) -contains $entry.hwnd -and $trace.complete -eq $true -and $null -eq $trace.error
                            $passed=$passed -and @($trace.raw).Count -eq $expected -and @($trace.expected).Count -eq 0
                            if($expected -gt 0) {
                                $downs=@($trace.raw|Where-Object {$_.message -eq 256})
                                $ups=@($trace.raw|Where-Object {$_.message -eq 257})
                                $passed=$passed -and $downs.Count -eq 6 -and $ups.Count -eq 6
                                $passed=$passed -and @($downs|Where-Object {$_.raw_extra_info -ne 1128678706}).Count -eq 0
                                $passed=$passed -and @($ups|Where-Object {$_.raw_extra_info -ne 1128678706}).Count -eq 0
                                $chars=@($trace.raw|Where-Object {$_.message -eq 258})
                                $passed=$passed -and $chars.Count -eq 0
                            }
                        }
                        continue
                    }
                    foreach($entry in @($wm.windows)) {
                        $count=if($entry.hwnd -eq $meta.window.hwnd){2}else{0}
                        $trace=$entry.trace
                        $passed=$passed -and @($first.hwnds) -contains $entry.hwnd -and $trace.complete -eq $true -and $null -eq $trace.error -and @($trace.expected).Count -eq $count -and @($trace.observed).Count -eq $count -and @($trace.raw).Count -eq $count
                        $x=[int][Math]::Floor(90*[double]$meta.scale_factor+0.5);$y=[int][Math]::Floor(94*[double]$meta.scale_factor+0.5)
                        for($index=0;$index -lt $count;$index++) {
                            if(@($trace.raw).Count -ne $count -or @($trace.expected).Count -ne $count -or @($trace.observed).Count -ne $count){$passed=$false;break}
                            $kind=if($index -eq 0){'down'}else{'up'}
                            $raw=$trace.raw[$index]
                            $message=if($index -eq 0){513}else{514}
                            $passed=$passed -and $raw.message -eq $message -and $raw.accepted -eq $true -and $raw.observation_point -eq 'window-proc' -and $raw.raw_extra_info -eq 1128678706
                            foreach($event in @($trace.expected[$index],$trace.observed[$index],$raw.event)){$passed=$passed -and $event.kind -eq $kind -and $event.x -eq $x -and $event.y -eq $y}
                        }
                    }
                }
            }
            if($Scenario -in @('Dispatch','ProfileDispatch')) {
                $passed=$passed -and $observed.scenario -eq $Scenario -and $observed.dispatch.success -eq $true -and $observed.dispatch.stages -eq 6
                foreach($stage in 0..5) {
                    $shotPath=Join-Path $case "window-stage-$stage.json"
                    if(-not (Test-Path -LiteralPath $shotPath)){$passed=$false;continue}
                    $shot=Get-Content -LiteralPath $shotPath -Raw -Encoding UTF8 | ConvertFrom-Json
                    $passed=$passed -and $shot.success -eq $true -and $shot.pid -eq $launch.pid -and $shot.hwnd -eq $first.hwnd
                }
            }
            if($Scenario -eq 'ProfileDispatch' -and $passed) {
                $p=$observed.dispatch.profiles
                $passed=$null -ne $p -and $p.is_same -eq $false -and $p.is_sharing -eq $false -and
                    $p.native_browser_context_queried -eq $false -and $p.storage_tested -eq $false -and
                    $p.a.storage -ceq 'Memory' -and $p.b.storage -ceq 'Persistent' -and
                    $p.a.ready_after_views -eq $true -and $p.b.ready_after_views -eq $true -and
                    ($p.a.browser -join ',') -ceq ($first.browser_ids.a -join ',') -and
                    ($p.b.browser -join ',') -ceq ($first.browser_ids.b -join ',') -and
                    @($p.a.configured_profile).Count -eq 2 -and @($p.b.configured_profile).Count -eq 2 -and
                    $p.a.configured_profile[0] -eq $p.a.browser[0] -and $p.b.configured_profile[0] -eq $p.b.browser[0] -and
                    $p.a.configured_profile[1] -eq 0 -and $p.b.configured_profile[1] -gt 0
            }
        }
        $summary.cases+=@{mode=$selectedMode;backend=$selectedBackend;pid=$launch.pid;passed=$passed;exit_code=$launch.exit_code;report=$report;observed=$observed}
        Write-Host "ComponentHost/$selectedMode/$selectedBackend pid=$($launch.pid) exit=$($launch.exit_code) passed=$passed"
        if($Scenario -eq 'MenuDefault' -and -not $passed){throw 'MenuDefault slice failed; preserve first failure and stop'}
        if($Scenario -eq 'ImeObserve' -and -not $passed){throw 'IME observation failed; stop before another process or pointer gesture'}
        if($Scenario -eq 'Resize' -and -not $passed){throw 'Resize slice failed; stop before another process or window change'}
        if($Scenario -eq 'Unfocused' -and -not $passed){throw 'Unfocused slice failed; stop before another process or owned foreground request'}
        if($Scenario -eq 'Exhaustion' -and -not $passed){throw 'Exhaustion slice failed; preserve fault/observer evidence and stop'}
        if($Scenario -eq 'Visibility' -and -not $passed){throw 'Visibility slice failed; preserve original evidence and stop without compensating window operation'}
        if($Scenario -eq 'ProfileDispatch' -and -not $passed){throw 'Passive ProfileDispatch failed; preserve evidence without forcing foreground or reattempting capture'}
    }}
    $summary.success=$summary.cases.Count -gt 0 -and @($summary.cases | Where-Object {-not $_.passed}).Count -eq 0
    if ($Scenario -in @('SourceViewer','FindView')) {
        $expected = @(foreach ($m in $modes) { foreach ($b in $backends) { "$m/$b" } })
        $actual = @($summary.cases | ForEach-Object { "$($_.mode)/$($_.backend)" })
        $summary.success = $summary.success -and $expected.Count -eq $summary.expected_cases -and
            ($expected -join ',') -ceq ($actual -join ',')
    }
    if($Scenario -eq 'MenuDefault'){$summary.success=$summary.success -and $summary.cases.Count -eq 1}
    if($Scenario -in @('ProfileDispatch','PaintLatency')) {
        $expected=@(foreach($m in $modes){foreach($b in $backends){ "$m/$b" }})
        $actual=@($summary.cases | ForEach-Object { "$($_.mode)/$($_.backend)" })
        $summary.expected_cases=$expected.Count
        $summary.success=$summary.success -and $expected.Count -gt 0 -and ($expected -join ',') -ceq ($actual -join ',')
        if($Scenario -eq 'PaintLatency' -and $summary.success){
            $summary.success=@($summary.cases | ForEach-Object { $_.observed.paint_timing.scale } | Select-Object -Unique).Count -eq 1
        }
    }
    if($Scenario -eq 'ImeObserve'){$summary.expected_cases=Get-ObservationCaseCount $modes $backends;$summary.success=$summary.success -and $summary.cases.Count -eq $summary.expected_cases}
    if($Scenario -in @('Resize','Unfocused','Exhaustion','Visibility')){$summary.success=$summary.success -and $summary.cases.Count -eq $summary.expected_cases}
    if ($ResizePassiveMatrix -and $summary.success) {
        $summary.resize_matrix_dpi = Get-PassiveResizeMatrixDpi $summary.cases
        if ($null -eq $summary.resize_matrix_dpi) { throw 'Passive Resize matrix did not preserve its four cases and one actual DPI' }
        $ids = Get-Content -LiteralPath (Join-Path $run 'artifact-identity.json') -Raw -Encoding UTF8 | ConvertFrom-Json
        if ((Get-PocFileSha256 $hostFile) -cne $ids.host_sha256 -or (Get-PocFileSha256 $helperFile) -cne $ids.helper_sha256) {
            throw 'Passive Resize matrix staging bytes changed'
        }
    }
} catch {$summary.error=$_.Exception.Message; $summary.success=$false; Write-Host "ComponentHost failed: $($summary.error)"}
finally{
    $env:CEF_PATH=$oldCef; $env:CARGO_TARGET_DIR=$oldTarget
    Write-JsonFile $summary (Join-Path $run 'result.json')
    Write-Host "Evidence: $run"
}
if(-not $summary.success){exit 1}
