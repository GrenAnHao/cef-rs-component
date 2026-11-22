[CmdletBinding()]
param(
    [ValidateSet('Core', 'Native', 'Profiles', 'Navigation', 'Policy', 'Renderer', 'All')][string]$Stage = 'Native',
    [ValidateSet('Integrated', 'Threaded', 'All')][string]$Mode = 'All',
    [ValidateSet('Software', 'D3d11', 'All')][string]$Backend = 'Software',
    [ValidateSet('Smoke', 'Frames', 'Multiple', 'Creating', 'Loading', 'PendingRequest', 'InFlight', 'CloseTimeout', 'CreatePostRejected', 'DeviceCreateFailure', 'CaptureFailureRecovery', 'CaptureTiming', 'RetiredFrameReplay', 'CommandPressureMenuClose', 'EventPressureMenuClose', 'PressureProfileFlushDrain', 'LateCreateClose', 'Menu', 'MenuDeferred', 'MenuBridge', 'MenuSource', 'MenuSourceBudget', 'SourceVisitorDrain', 'SourceProfileFlushDrain', 'SourceFindProfileDrain', 'TitleSourceFindProfileDrain', 'ManagedPressureSourceFindDrain', 'ManagedPressureSourceFindDrainAux', 'AuxDevTools', 'MenuDevTools', 'ProfileFlushDrain', 'MenuFind', 'MenuFindIsolation', 'MenuSpelling', 'MenuEditHistory', 'DesktopUx', 'JsBridgeSchemes', 'HostCapabilities', 'OsrParity', 'NativeChildHwnd', 'PrintDrainFence', 'Faults', 'All')][string]$Scenario = 'All',
    [ValidateSet('Native', 'Gpui', 'All')][string]$HostKind = 'All',
    [string]$CefPath,
    [string]$TargetDir,
    [ValidateRange(1, 900)][int]$TimeoutSeconds = 120,
    [ValidateSet('Memory', 'Persistent', 'All')][string]$Storage = 'All',
    [switch]$InteractiveInputApproved,
    [string]$SpellingDictionaryFixture,
    [string]$SpellingDictionaryLicense,
    [switch]$StageBundleInTemp
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($StageBundleInTemp -and ($Stage -ne 'Native' -or $Scenario -notin @('PressureProfileFlushDrain','SourceProfileFlushDrain','SourceFindProfileDrain','TitleSourceFindProfileDrain','ManagedPressureSourceFindDrain','ManagedPressureSourceFindDrainAux','AuxDevTools'))) {
    throw 'Temporary bundle staging is limited to explicit native joint flush fixtures'
}
. (Join-Path $PSScriptRoot 'common.ps1')

function Write-JsonFile($Value, [string]$Path) {
    [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 12), (New-Object Text.UTF8Encoding($false)))
}
if ($Stage -in @('Core', 'All')) {
    if ($HostKind -ne 'All' -or $Scenario -ne 'All' -or $Storage -ne 'All' -or $InteractiveInputApproved -or
        $SpellingDictionaryFixture -or $SpellingDictionaryLicense) {
        throw "$Stage does not accept native scenario/host filters, storage filters or interactive fixtures"
    }
    . (Join-Path $PSScriptRoot 'component-test-stages.ps1')
    if ($Stage -eq 'Core') {
        if ($PSBoundParameters.ContainsKey('Mode') -or $PSBoundParameters.ContainsKey('Backend') -or $CefPath) {
            throw 'Core does not accept mode, backend or CEF options'
        }
        if (Invoke-ComponentCore $TargetDir $TimeoutSeconds) { exit 0 }
        exit 1
    }
    if ($Mode -ne 'All' -or $Backend -ne 'All') { throw 'Stage All requires Mode All and Backend All' }
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'Stage All requires Windows x64 MSVC; only Stage Core is supported on other platforms' }
    if (Invoke-ComponentAll $CefPath $TargetDir $TimeoutSeconds) { exit 0 }
    exit 1
}
. (Join-Path $PSScriptRoot 'component-process-observer.ps1')
function Test-AcceleratedEarlyClose($Observed, [string]$Scenario) {
    $property = $Observed.PSObject.Properties['early_close_backend']
    if ($null -eq $property -or $null -eq $property.Value) { return $false }
    $early = $property.Value
    if (@($early.backend_browser).Count -ne 2 -or @($early.target_browser).Count -ne 2 -or
        $null -eq $early.capture -or $null -eq $early.retired_capture) { return $false }
    $cap = $early.capture
    $final = $early.retired_capture
    $backendId = $early.backend_browser -join ','
    $targetId = $early.target_browser -join ','
    $closed = @($early.closed_events)
    $backendClosed = @($closed | Where-Object { ($_[0] -join ',') -ceq $backendId -and $_[1] -gt 0 }).Count
    $targetClosed = @($closed | Where-Object { ($_[0] -join ',') -ceq $targetId -and $_[1] -gt 0 }).Count
    $cancel = $early.cancelled_creation
    $targetDone = ($null -eq $cancel -and $targetClosed -eq 1 -and $Observed.cancelled_creates -eq 0) -or
        ($Scenario -eq 'Creating' -and $null -ne $cancel -and ($cancel[0] -join ',') -ceq $targetId -and $cancel[1] -gt 0 -and $targetClosed -eq 0 -and $Observed.cancelled_creates -eq 1)
    $boundary = ($Scenario -eq 'Creating' -and $early.target_before_drain -ceq 'Creating') -or
        ($Scenario -eq 'Loading' -and $early.target_before_drain -ceq 'Alive' -and $Observed.loading_request_observed)
    return $early.backend_browser[0] -gt 0 -and $early.backend_browser[1] -gt 0 -and
        $early.target_browser[0] -eq $early.backend_browser[0] -and $early.target_browser[1] -gt $early.backend_browser[1] -and
        $boundary -and $targetDone -and $backendClosed -eq 1 -and $closed.Count -eq ($backendClosed + $targetClosed) -and
        $closed.Count -eq $Observed.created -and ($Observed.created + $Observed.cancelled_creates) -eq 2 -and
        $early.observer_released -and $early.backend_source_closed -and $early.target_source_closed -and
        $early.frame_generation -gt 0 -and $early.frame_sequence -gt 0 -and $Observed.valid_frame_browsers -gt 0 -and
        $Observed.first_frame_width -eq 320 -and $Observed.first_frame_height -eq 240 -and $Observed.task_post_failures -eq 0 -and
        $cap.observed_backend -ceq 'D3d11' -and $cap.requested_backend -ceq 'D3d11' -and $cap.owner_thread -eq $Observed.cef_ui_thread_id -and
        $cap.native_resources_live -and $cap.d3d11_captures -gt 0 -and $cap.software_captures -eq 0 -and
        $cap.maps -eq $cap.unmaps -and $cap.maps -ge $cap.d3d11_captures -and $final.retired -and -not $final.abandoned -and
        -not $final.native_resources_live -and $final.staging_bytes -eq 0 -and $final.live_imports -eq 0 -and
        $final.maps -eq $final.unmaps -and $final.failures -eq 0 -and
        $final.retirement_thread -eq $cap.owner_thread -and $final.owner_thread -eq $cap.owner_thread -and
        $final.d3d11_captures -ge $cap.d3d11_captures -and $final.software_captures -eq 0
}

function Test-LatencySummary($Samples, $Stats) {
    $ordered = @($Samples | Sort-Object)
    $n = $ordered.Count
    if ($n -eq 0) { return $null -eq $Stats }
    return $null -ne $Stats -and $Stats.samples -eq $n -and
        $Stats.p50_ns -eq $ordered[[int][Math]::Ceiling($n * 0.50) - 1] -and
        $Stats.p95_ns -eq $ordered[[int][Math]::Ceiling($n * 0.95) - 1] -and
        $Stats.max_ns -eq $ordered[-1] -and $ordered[0] -ge 0 -and $ordered[-1] -gt 0
}

function Test-CaptureTiming($Observed) {
    $property = $Observed.PSObject.Properties['capture_timing']
    if ($null -eq $property -or $null -eq $property.Value) { return $false }
    $w = $property.Value
    $rows = @($w.copy_samples)
    $frames = @($w.published_sequences)
    if ($rows.Count -ne 32 -or $frames.Count -ne 32 -or $w.invalidates_applied -ne 32 -or
        @($w.browser).Count -ne 2 -or $w.browser[0] -le 0 -or $w.browser[1] -le 0 -or
        $w.generation -le 0 -or $w.baseline_sequence -le 0 -or $null -eq $w.retired_capture) { return $false }
    $previous = $w.baseline_sequence
    for ($i=0; $i -lt 32; $i++) {
        $row = $rows[$i]
        if ($row.sequence -ne $frames[$i] -or $row.sequence -le $previous -or
            ($row.browser -join ',') -cne ($w.browser -join ',') -or $row.generation -ne $w.generation -or
            $row.width -ne 320 -or $row.height -ne 240 -or $row.ui_thread -ne $Observed.cef_ui_thread_id) { return $false }
        $previous = $row.sequence
    }
    $pumps = @($w.manual_pump_samples_ns)
    $pumpOk = if ($Observed.requested_mode -ceq 'Integrated') {
        $w.manual_pump_calls -gt 0 -and $w.manual_pump_calls -le $Observed.manual_pump_count -and
            $pumps.Count -eq [Math]::Min($w.manual_pump_calls, 256)
    } else {
        $Observed.requested_mode -ceq 'Threaded' -and $w.manual_pump_calls -eq 0 -and $pumps.Count -eq 0 -and $Observed.manual_pump_count -eq 0
    }
    $a = $w.baseline_capture
    $b = $w.retired_capture
    $captureOk = if ($Observed.requested_backend -ceq 'D3d11') {
        $a.d3d11_captures -gt 0 -and $a.software_captures -eq 0 -and $b.software_captures -eq 0 -and
            $b.d3d11_captures -ge ($a.d3d11_captures + 32) -and $b.maps -ge ($a.maps + 32) -and $b.maps -eq $b.unmaps
    } else {
        $Observed.requested_backend -ceq 'Software' -and $a.software_captures -gt 0 -and
            $b.software_captures -ge ($a.software_captures + 32) -and $a.d3d11_captures -eq 0 -and
            $b.d3d11_captures -eq 0 -and $b.maps -eq 0 -and $b.unmaps -eq 0
    }
    return $pumpOk -and (Test-LatencySummary $pumps $w.manual_pump_ns) -and
        (Test-LatencySummary @($rows | ForEach-Object { $_.copy_ns }) $w.copy_ns) -and
        $w.observer_released -and $w.rearm_refused -and $w.source_closed -and $w.final_cpu_bytes -ceq 0 -and
        $w.peak_cpu_bytes -ge 307200 -and $w.peak_cpu_bytes -le $w.cpu_limit_bytes -and $w.cpu_limit_bytes -eq 268435456 -and
        $captureOk -and $a.observed_backend -ceq $Observed.requested_backend -and $b.observed_backend -ceq $Observed.requested_backend -and
        $a.failures -eq 0 -and $b.failures -eq 0 -and $b.retired -and -not $b.abandoned -and -not $b.native_resources_live -and
        $b.live_imports -eq 0 -and $b.staging_bytes -eq 0 -and $b.retirement_thread -eq $b.owner_thread -and
        $b.owner_thread -eq $Observed.cef_ui_thread_id -and $Observed.task_post_failures -eq 0
}

function Test-ProfileFlushDrain($Observed) {
    $property = $Observed.PSObject.Properties['profile_flush_drain']
    if ($null -eq $property -or $null -eq $property.Value) { return $false }
    $w = $property.Value
    $events = @($w.closed_events)
    if (@($w.profile).Count -ne 2 -or @($w.default_profile).Count -ne 2 -or @($w.first).Count -ne 2 -or
        @($w.survivor).Count -ne 2 -or $events.Count -ne 2 -or @($w.release_task).Count -ne 2) { return $false }
    $runtime = $w.profile[0]
    return $runtime -gt 0 -and $w.profile[1] -gt 0 -and $w.default_profile[0] -eq $runtime -and $w.default_profile[1] -eq 0 -and
        $w.distinct_native_contexts -and $w.first[0] -eq $runtime -and $w.first[1] -gt 0 -and $w.survivor[0] -eq $runtime -and
        $w.survivor[1] -gt $w.first[1] -and $w.sequence -gt 0 -and $w.callback_thread -eq $Observed.cef_ui_thread_id -and
        $w.source_closed_after_first -and $w.profile_ready_after_first -and $w.pending_after_first -eq 1 -and
        $w.accepted_before_release -eq 1 -and $w.completed_before_release -eq 0 -and
        $w.survivor_frame_before -gt 0 -and $w.survivor_frame_after -gt $w.survivor_frame_before -and
        ($events[0][0] -join ',') -ceq ($w.first -join ',') -and ($events[1][0] -join ',') -ceq ($w.survivor -join ',') -and
        $events[0][1] -gt 0 -and $events[1][1] -gt $events[0][1] -and $w.poll_only_flush -and $w.finish_only_flush -and
        $w.shutdown_before_release -ceq 0 -and $w.new_flush_refused_while_draining -and $w.wrong_profile_refused -and $w.wrong_sequence_refused -and
        $w.release_task[0] -eq $runtime -and $w.release_task[1] -gt 0 -and -not $w.marker_faulted -and $w.original_ticket_completed -and
        $w.final_accepted -eq 1 -and $w.final_completed -eq 1 -and $w.final_pending -eq 0 -and
        $w.final_pending_profiles -eq 0 -and $w.final_profile_refs -eq 0 -and $w.root_lease_released -and
        $w.final_cpu_bytes -ceq 0 -and $Observed.task_post_failures -eq 0 -and
        $Observed.first_frame_width -eq 320 -and $Observed.first_frame_height -eq 240 -and $Observed.frames -ge 3
}

function Get-ExecutableArtifact($Messages, [string]$Name) {
    $matches = @($Messages | Where-Object {
        $_.reason -eq 'compiler-artifact' -and $_.target.name -eq $Name -and $null -ne $_.executable
    })
    if ($matches.Count -ne 1) { throw "Expected one actual Cargo executable artifact for $Name; got $($matches.Count)" }
    return [IO.Path]::GetFullPath($matches[0].executable)
}
. (Join-Path $PSScriptRoot 'component-probe-launcher.ps1')
. (Join-Path $PSScriptRoot 'component-spelling-dictionary-fixture.ps1')
if ($SpellingDictionaryFixture -or $SpellingDictionaryLicense) {
    if ($Scenario -ne 'MenuSpelling' -or $Stage -ne 'Native' -or $HostKind -ne 'Native' -or $Backend -ne 'Software' -or -not $SpellingDictionaryFixture -or -not $SpellingDictionaryLicense) { throw 'Dictionary fixture requires explicit MenuSpelling Native/Software plus dictionary and license' }
}

if ($Scenario -in @('Menu','MenuDeferred','MenuBridge','MenuSource','MenuSourceBudget','SourceVisitorDrain','MenuFind','MenuSpelling') -and ($Stage -ne 'Native' -or $HostKind -ne 'Native' -or $Backend -ne 'Software')) { throw 'Menu requires explicit Stage Native, HostKind Native and Backend Software; this windowless callback fixture is not a UI/4-backend acceptance run' }

if ($Scenario -eq 'PrintDrainFence' -and ($Stage -ne 'Native' -or $HostKind -ne 'Native' -or $Mode -ne 'All' -or $Backend -ne 'Software' -or $InteractiveInputApproved)) {
    throw 'PrintDrainFence requires Native host, Mode All, Software without desktop input or printing'
}

function Test-PrintDrainFence($Probe) {
    if ($null -eq $Probe -or $null -eq $Probe.PSObject.Properties['print_fence'] -or $null -eq $Probe.print_fence) { return $false }
    $proof = $Probe.print_fence
    $rejected = $proof.PSObject.Properties['wrong_owner_and_replay_refused']
    if ($null -eq $rejected -or @($proof.both_shutdown_gates_blocked).Count -ne 2 -or @($proof.ids).Count -ne 2) { return $false }
    foreach ($flag in @($proof.metadata_only,$proof.a_closed_source,$proof.b_closed_source,$proof.b_owned_live_at_a_close,
        $proof.cancelled_original_a_only,$rejected.Value,$proof.ready_after_explicit_retirement) + $proof.both_shutdown_gates_blocked) {
        if ($flag -isnot [bool] -or -not $flag) { return $false }
    }
    foreach ($value in @($proof.a_close_sequence) + $proof.ids[0] + $proof.ids[1] + $proof.pending_by_cut) {
        if ($value -isnot [int] -and $value -isnot [long]) { return $false }
    }
    return ($proof.ids[0] -join ',') -ceq '1,1,1' -and ($proof.ids[1] -join ',') -ceq '1,2,2' -and
        $proof.a_close_sequence -gt 0 -and ($proof.pending_by_cut -join ',') -ceq '2,2,2,1,1,1,0' -and
        @($proof.cleanup_errors).Count -eq 0 -and $Probe.task_post_failures -eq 0
}

if ($Scenario -eq 'MenuEditHistory' -and ($Stage -ne 'Native' -or $HostKind -ne 'Native' -or $Mode -ne 'All' -or $Backend -ne 'All' -or $InteractiveInputApproved)) {
    throw 'MenuEditHistory requires Stage Native, HostKind Native, Mode All and Backend All without desktop input'
}

function Test-DesktopUx($Probe) {
    if ($null -eq $Probe -or $null -eq $Probe.PSObject.Properties['desktop_ux'] -or $null -eq $Probe.desktop_ux) { return $false }
    $u = $Probe.desktop_ux
    return $u.cursor_pointing_hand -and
        ($u.tooltip_text -ceq 'Desktop UX Tooltip') -and
        $u.status_observed -and
        ($u.loading_progress_max -gt 0) -and
        $u.console_warn_observed -and
        ($u.zoom_event_milli -eq 1500) -and
        ($u.zoom_snapshot_milli -eq 1500) -and
        ($u.muted_event -eq $true) -and
        $u.muted_snapshot -and
        ($u.shortcut_id -eq 1) -and
        ($u.passthrough_key_title -ceq 'desktop-ux:key-a') -and
        (-not $u.page_saw_shortcut_key) -and
        ($u.pulled_ux_event_count -ge 5)
}

function Test-JsBridgeSchemes($Probe) {
    if ($null -eq $Probe -or $null -eq $Probe.PSObject.Properties['js_bridge_schemes'] -or $null -eq $Probe.js_bridge_schemes) { return $false }
    $j = $Probe.js_bridge_schemes
    return $j.app_page_ready -and
        $j.authorized_bridge_frozen -and
        $j.unauthorized_bridge_absent -and
        $j.same_origin_fetch_ok -and
        ($j.interceptor_status -eq 201) -and
        $j.interceptor_body_ok -and
        ($j.interceptor_header_hits -ge 1) -and
        ($j.interceptor_disallowed_calls -eq 0) -and
        $j.external_fetch_blocked -and
        $j.page_to_host_msg_ok -and
        $j.host_to_page_ack_ok -and
        $j.execute_js_ok -and
        $j.eval_json_ok -and
        $j.eval_exception_ok -and
        $j.eval_unsupported_rejected -and
        $j.unlisted_scheme_nav_denied -and
        $j.eval_nav_cancelled -and
        $j.eval_close_cancelled -and
        $j.dir_file_ok -and
        ($j.dir_traversal_status -eq 404) -and
        ($j.dir_junction_status -eq 404) -and
        ($j.pending_evals_after_close -eq 0)
}

function Test-HostCapabilities($Probe) {
    if ($null -eq $Probe -or $null -eq $Probe.PSObject.Properties['host_capabilities'] -or $null -eq $Probe.host_capabilities) { return $false }
    $h = $Probe.host_capabilities
    return $h.default_deny_js_dialog_denied -and
        $h.default_deny_popup_denied -and
        $h.default_deny_download_denied -and
        $h.default_deny_cdp_rejected -and
        $h.pixel_verified -and
        $h.js_dialog_resolved_once -and
        $h.js_dialog_duplicate_rejected -and
        $h.popup_captured_and_blocked -and
        $h.download_forbidden_dest_rejected -and
        $h.download_confirmed_bytes_ok -and
        $h.auth_callback_handled -and
        $h.auth_secrets_redacted -and
        $h.auth_cache_cleared -and
        $h.cookie_set_visited_deleted_ok -and
        $h.cookie_filter_invoked -and
        $h.cookie_zero_visit_ok -and
        $h.cookie_secrets_redacted -and
        $h.pref_allow_ok -and
        $h.pref_blocked_rejected -and
        $h.pdf_exported_header_ok -and
        $h.cdp_method_eval_42_ok -and
        $h.force_close_download_cancelled -and
        ($h.pending_host_reqs_after_close -eq 0) -and
        ($h.cdp_attached_before_shutdown -ge 1) -and
        ($h.cdp_dropped_on_ui_before_shutdown -eq $h.cdp_attached_before_shutdown) -and
        ($h.cdp_active_after_shutdown -eq 0)
}

function Test-OsrParity($Probe) {
    if ($null -eq $Probe -or $null -eq $Probe.PSObject.Properties['osr_parity'] -or $null -eq $Probe.osr_parity) { return $false }
    $o = $Probe.osr_parity
    return $o.default_deny_outbound_drag_refused -and
        $o.optin_outbound_drag_started -and
        $o.optin_outbound_drag_payload_redacted -and
        $o.optin_outbound_drag_finished_no_deadlock -and
        $o.optin_outbound_drag_stale_rejected -and
        $o.a11y_tree_captured -and
        ($o.a11y_node_count -ge 2) -and
        $o.a11y_debug_redacted -and
        $o.a11y_cleared_on_disable -and
        $o.url_request_profile_cookie_ok -and
        $o.url_request_frame_post_ok -and
        $o.url_request_auth_default_denied -and
        $o.url_request_auth_optin_ok -and
        $o.url_request_debug_redacted -and
        $o.url_request_untrusted_redirect_denied -and
        ($o.url_request_disallowed_hits -eq 0) -and
        $o.url_request_explicit_cancel_ok -and
        $o.url_request_shutdown_drain_ok -and
        $o.touch_events_dispatched_ok -and
        $o.touch_active_cancelled_on_reset -and
        ($o.pending_url_requests_after_shutdown -eq 0)
}

function Test-NativeChildHwnd($Probe) {
    if ($null -eq $Probe -or $null -eq $Probe.PSObject.Properties['native_child_hwnd'] -or $null -eq $Probe.native_child_hwnd) { return $false }
    $n = $Probe.native_child_hwnd
    return $n.invalid_parent_rejected -and
        $n.invalid_bounds_rejected -and
        $n.child_is_window -and
        $n.child_parent_matches -and
        $n.noactivate_stripped -and
        $n.surface_mode_matches -and
        $n.zero_osr_frames_on_native -and
        $n.osr_commands_rejected -and
        $n.bounds_tracked_ok -and
        $n.hide_zero_size_ok -and
        $n.restore_bounds_ok -and
        $n.notify_parent_moved_ok -and
        $n.focus_commands_ok -and
        $n.policy_denied_ok -and
        $n.eval_script_ok -and
        $n.print_to_pdf_ok -and
        $n.devtools_associated_open_ok -and
        $n.devtools_closed_dissociated_ok -and
        $n.url_request_ok -and
        $n.graceful_close_hwnd_gone -and
        $n.parent_destroyed_first_safe -and
        $n.coexistence_osr_frame_advanced
}

function Test-EditHistory($Report) {
    if ($null -eq $Report.PSObject.Properties['edit_history'] -or $null -eq $Report.edit_history) { return $false }
    $q = $Report.edit_history
    $s = $q.status; $f = $q.fixture
    if (@($q.browser).Count -ne 2 -or $q.browser[0] -le 0 -or $q.browser[1] -le 0 -or @($q.menus).Count -ne 3 -or
        ($q.renderer_witnesses -join ',') -cne 'edit-history:1:insertText:1,edit-history:2:historyUndo:0,edit-history:3:historyRedo:1' -or
        $s.opened -ne 3 -or $s.published -ne 3 -or $s.cancelled -ne 3 -or $s.native_dismissed -ne 3 -or $s.actions_submitted -ne 2 -or
        $s.post_failures -ne 0 -or $null -ne $s.first_failure -or -not $s.retired -or $f.published -ne 3 -or $f.terminal -ne 3 -or
        $f.outstanding -ne 0 -or ($f.terminal_ids -join ',') -cne '1,2,3' -or $null -ne $f.first_error -or $Report.task_post_failures -ne 0) { return $false }
    $flags = @('False,False','True,False','False,True')
    $refused = @('110,111','111','110')
    $actions = @($null,110,111)
    for ($i=0; $i -lt 3; $i++) {
        $m = $q.menus[$i]
        if ($m.request -ne ($i+1) -or ($m.enabled -join ',') -cne $flags[$i] -or $m.separators -ne 2 -or
            ($m.refused -join ',') -cne $refused[$i] -or $m.action -cne $actions[$i] -or $m.frame_id_bytes -le 0 -or
            $m.frame_id_bytes -ne $f.frame_id_bytes -or ($m.anchor -join ',') -cne '20,20' -or
            $m.stale_refused -isnot [bool] -or $m.stale_refused -ne ($i -gt 0) -or $m.terminal -isnot [bool] -or -not $m.terminal) { return $false }
    }
    return $true
}

if ($Stage -eq 'Policy') {
    if ($HostKind -ne 'All' -or $Scenario -ne 'All') { throw 'Policy does not accept Native scenario or host filters' }
    & (Join-Path $PSScriptRoot 'test-component-policy.ps1') -Mode $Mode -Storage $Storage -Backend $Backend -CefPath $CefPath -TargetDir $TargetDir -TimeoutSeconds $TimeoutSeconds -InteractiveInputApproved:$InteractiveInputApproved
    exit $LASTEXITCODE
}
if ($Storage -ne 'All' -or $InteractiveInputApproved) { throw 'Storage and InteractiveInputApproved apply only to Stage Policy' }

if ($Stage -eq 'Renderer') {
    if ($HostKind -ne 'All' -or $Scenario -ne 'All') { throw 'Renderer does not accept Native scenario or host filters' }
    & (Join-Path $PSScriptRoot 'test-component-renderer.ps1') -Mode $Mode -Backend $Backend -CefPath $CefPath -TargetDir $TargetDir -TimeoutSeconds $TimeoutSeconds
    exit $LASTEXITCODE
}

if ($Stage -eq 'Navigation') {
    if ($HostKind -ne 'All' -or $Scenario -ne 'All') { throw 'Navigation does not accept Native scenario or host filters' }
    & (Join-Path $PSScriptRoot 'test-component-navigation.ps1') -Mode $Mode -Backend $Backend -CefPath $CefPath -TargetDir $TargetDir -TimeoutSeconds $TimeoutSeconds
    exit $LASTEXITCODE
}

if ($Stage -eq 'Profiles') {
    if ($HostKind -ne 'All' -or $Scenario -ne 'All') { throw 'Profiles does not accept Native scenario or host filters' }
    & (Join-Path $PSScriptRoot 'test-component-profiles.ps1') -Mode $Mode -Backend $Backend -CefPath $CefPath -TargetDir $TargetDir -TimeoutSeconds $TimeoutSeconds
    exit $LASTEXITCODE
}

if ($Scenario -in @('CreatePostRejected', 'LateCreateClose', 'CaptureFailureRecovery', 'RetiredFrameReplay') -and ($Stage -ne 'Native' -or $HostKind -ne 'Native')) {
    throw "$Scenario requires Stage Native and HostKind Native"
}
if ($Scenario -in @('CaptureTiming','ProfileFlushDrain','MenuFindIsolation') -and ($Stage -ne 'Native' -or $HostKind -ne 'Native' -or $Mode -ne 'All' -or $Backend -ne 'All')) {
    throw "$Scenario requires explicit Stage Native, HostKind Native, Mode All and Backend All"
}
if ($Scenario -eq 'DeviceCreateFailure' -and ($Stage -ne 'Native' -or $HostKind -ne 'Native' -or $Backend -ne 'D3d11')) {
    throw 'DeviceCreateFailure requires Stage Native, HostKind Native and Backend D3d11'
}
if ($Scenario -in @('AuxDevTools','MenuDevTools') -and ($Stage -ne 'Native' -or $HostKind -ne 'Native' -or $Backend -eq 'All')) {
    throw 'DevTools scenarios require Stage Native, HostKind Native and one Required backend'
}
if ($Scenario -in @('PressureProfileFlushDrain','ManagedPressureSourceFindDrain','ManagedPressureSourceFindDrainAux') -and ($Stage -ne 'Native' -or $HostKind -ne 'Native' -or $Mode -ne 'All' -or $Backend -eq 'All')) {
    throw "$Scenario requires Stage Native, HostKind Native, Mode All and one Required backend"
}
if ($Scenario -in @('SourceProfileFlushDrain','SourceFindProfileDrain','TitleSourceFindProfileDrain') -and ($Stage -ne 'Native' -or $HostKind -ne 'Native' -or $Mode -ne 'All' -or $Backend -ne 'Software')) {
    throw "$Scenario requires Stage Native, HostKind Native, Mode All and Backend Software"
}
if ($Scenario -in @('CommandPressureMenuClose','EventPressureMenuClose') -and ($Stage -ne 'Native' -or $HostKind -ne 'Native' -or $Backend -ne 'Software')) {
    throw "$Scenario requires Stage Native, HostKind Native and Backend Software"
}
if ($Scenario -eq 'Frames' -and $HostKind -ne 'Native') { throw 'Frames requires explicit HostKind Native; the final comparator requires all four cases, so a partial diagnostic run cannot pass acceptance' }

$faultMatrix = $Scenario -eq 'Faults'
if ($faultMatrix -and ($Stage -ne 'Native' -or $HostKind -ne 'Native' -or $Mode -ne 'All' -or $Backend -ne 'All')) {
    throw 'Faults requires explicit Stage Native, HostKind Native, Mode All and Backend All'
}

function Test-JointFindDrain($Report) {
    $d=$Report.joint_find_drain; $s=$Report.source_visitor_drain; $f=$Report.profile_flush_drain
    if ($null -eq $d) { return $false }
    $b=$s.survivor -join ','; $before="$b,1"; $pending="$b,2"
    $owner=$d.find_owner; $menu=$owner.menu
    return ($d.pending_before_first_close -join ',') -ceq $pending -and
        ($d.pending_after_first_close -join ',') -ceq $pending -and ($d.cancelled_after_survivor_close -join ',') -ceq $pending -and
        ($d.source_operation_through_first_close -join ',') -ceq ($s.operation -join ',') -and
        ($d.flush_marker_through_first_close -join ',') -ceq (($f.profile -join ',') + ',' + $f.sequence) -and
        ($d.counts_after_survivor -join ',') -ceq '2,2,0' -and @($d.closed_events).Count -eq 2 -and
        ($d.closed_events[0][0] -join ',') -ceq ($s.operation[0..1] -join ',') -and
        ($d.closed_events[1][0] -join ',') -ceq $b -and $d.closed_events[0][1] -eq $s.closed_events[0][1] -and
        $d.closed_events[1][1] -eq $s.closed_events[1][1] -and
        @($owner.started).Count -eq 2 -and ($owner.started[0] -join ',') -ceq $before -and ($owner.started[1] -join ',') -ceq $pending -and
        @($owner.finished).Count -eq 2 -and ($owner.finished[0][0] -join ',') -ceq $before -and $owner.finished[0][1] -ceq 'Some(Result)' -and
        ($owner.finished[1][0] -join ',') -ceq $pending -and $owner.finished[1][1] -ceq 'Some(Cancelled)' -and $owner.all_receipts_retired -and
        @($owner.native_finals).Count -eq 1 -and ($owner.native_finals[0][0] -join ',') -ceq $before -and
        $owner.native_finals[0][1] -ge 0 -and $owner.native_finals[0][2] -eq 1 -and $owner.native_finals[0][3] -in @(0,1) -and
        $owner.native_finals[0][4] -eq $Report.cef_ui_thread_id -and
        ($owner.query_bytes -join ',') -ceq '21,21' -and ($owner.frame_id_bytes -join ',') -ceq '34,34' -and
        $menu.published -eq 3 -and $menu.terminal -eq 3 -and $menu.outstanding -eq 0 -and $null -eq $menu.first_error -and
        ($menu.selection_bytes -join ',') -ceq '0,21,21' -and @($menu.terminal_ids).Count -eq 3 -and
        $menu.terminal_ids[0] -gt 0 -and $menu.terminal_ids[1] -gt $menu.terminal_ids[0] -and $menu.terminal_ids[2] -gt $menu.terminal_ids[1]
}

function Test-JointTitlePressure($Report) {
    $event=$Report.event_pressure; $joint=$Report.joint_find_drain; $source=$Report.source_visitor_drain
    if ($null -eq $event -or $null -eq $joint -or $null -eq $source) { return $false }
    return ($event.browser -join ',') -ceq ($source.operation[0..1] -join ',') -and
        $event.queued -eq 256 -and $event.ordinary_depth_at_first_close -eq 256 -and $event.rejected -eq 1 -and
        $event.last_rejected_sequence -gt 0 -and $event.first_close_sequence -gt $event.last_rejected_sequence -and
        $event.accepted_sequences_ordered -and $event.retained_titles_ordered -and
        $event.resynced_title -eq 257 -and $joint.title_at_joint_cut -eq $event.resynced_title -and
        $event.first_close_sequence -eq $joint.closed_events[0][1]
}

function Test-ManagedPressureDrain($Report) {
    $d=$Report.managed_pressure_drain; $source=$Report.source_visitor_drain; $find=$Report.joint_find_drain; $flush=$Report.profile_flush_drain
    if ($null -eq $d -or $null -eq $source -or $null -eq $find -or $null -eq $flush) { return $false }
    $p=$d.command; $cut=$p.held_managed_at_close; $m=$d.menu_delivery; $owner=$d.menu_status
    if ($null -eq $cut) { return $false }
    $w=$p.PSObject.Properties['managed_menu_cancelled_by_close']
    if ($null -eq $w -or $null -eq $w.Value) { return $false }
    $cancel=$w.Value
    return ($cancel.task -join ',') -ceq ($p.close_task -join ',') -and
        ($cancel.menu -join ',') -ceq ($cut.menu -join ',') -and
        $p.thread -eq $Report.cef_ui_thread_id -and $null -eq $p.error -and $p.first_sequence -gt 0 -and
        $p.accepted -eq 256 -and $p.dispatched -eq 256 -and $p.pending_at_close -eq 256 -and
        $p.overflow_refused -and $p.new_command_refused_after_intent -and $p.menu_pending_before -eq 1 -and $p.menu_pending_at_close -eq 1 -and
        ($p.held_cookie_flush_at_close -join ',') -ceq (($flush.profile -join ',') + ',' + $flush.sequence) -and
        @($p.close_task).Count -eq 2 -and $p.close_task[0] -eq $flush.first[0] -and $p.close_task[0] -eq $flush.release_task[0] -and
        $p.close_task[1] -gt 0 -and $p.close_task[1] -lt $flush.release_task[1] -and
        @($cut.menu).Count -eq 3 -and ($cut.menu[0..1] -join ',') -ceq ($flush.first -join ',') -and
        ($cut.source -join ',') -ceq ($source.operation -join ',') -and ($cut.find -join ',') -ceq ($find.pending_before_first_close -join ',') -and
        $cut.title -eq $find.title_at_joint_cut -and $cut.title -eq 257 -and
        $m.published -eq 2 -and $m.terminal -eq 2 -and $m.outstanding -eq 0 -and $null -eq $m.first_error -and
        @($m.terminal_ids).Count -eq 2 -and $m.terminal_ids[0] -gt 0 -and $m.terminal_ids[0] -lt $cut.menu[2] -and $m.terminal_ids[1] -eq $cut.menu[2] -and
        $owner.retired -and $owner.opened -eq 2 -and $owner.published -eq 2 -and $owner.cancelled -eq 2 -and
        $owner.actions_submitted -eq 1 -and $owner.post_failures -eq 0 -and $null -eq $owner.first_failure
}

function Test-LateManagedMenuReply($Report) {
    $d=$Report.managed_pressure_drain
    if ($null -eq $d) { return $false }
    $property=$d.PSObject.Properties['late_reply']
    if ($null -eq $property -or $null -eq $property.Value) { return $false }
    $late=$property.Value
    $closed=$Report.joint_find_drain.closed_events[0]
    return ($late.menu -join ',') -ceq ($d.command.held_managed_at_close.menu -join ',') -and
        @($late.owner_closed).Count -eq 2 -and ($late.owner_closed[0] -join ',') -ceq ($closed[0] -join ',') -and
        $late.owner_closed[1] -eq $closed[1] -and $late.owner_closed[1] -gt 0 -and $late.command_id -eq 117 -and
        $late.select_error -ceq 'InvalidRequest' -and $late.cancel_error -ceq 'InvalidRequest' -and
        @($late.tasks_posted).Count -eq 2 -and $late.tasks_posted[0] -gt 0 -and $late.tasks_posted[0] -eq $late.tasks_posted[1] -and
        @($late.native_calls).Count -eq 2 -and $late.native_calls[0] -gt 0 -and $late.native_calls[0] -eq $late.native_calls[1]
}

function Test-AcceleratedPressureCapture($Pressure, [long]$UiThread) {
    $w = $Pressure.PSObject.Properties['accelerated_capture']
    if ($null -eq $w -or $null -eq $w.Value -or $UiThread -le 0) { return $false }
    $d = $w.Value
    foreach ($r in @($d.first_retired,$d.survivor_live_after_invalidate,$d.survivor_retired)) {
        if ($null -eq $r -or $r.requested_backend -cne 'D3d11' -or $r.observed_backend -cne 'D3d11' -or
            $r.owner_thread -ne $UiThread -or $r.abandoned -or $r.failures -ne 0 -or $r.live_imports -ne 0 -or
            $r.software_captures -ne 0 -or $r.d3d11_captures -le 0 -or $r.maps -ne $r.unmaps -or
            $r.maps -lt $r.d3d11_captures -or $r.copies -lt $r.d3d11_captures -or $r.opens -lt $r.d3d11_captures) { return $false }
    }
    foreach ($r in @($d.first_retired,$d.survivor_retired)) {
        if (-not $r.retired -or $r.native_resources_live -or $r.staging_bytes -ne 0 -or $r.retirement_thread -ne $UiThread) { return $false }
    }
    $live = $d.survivor_live_after_invalidate
    return -not $live.retired -and $live.native_resources_live -and $live.staging_bytes -ge (320 * 240 * 4) -and
        $d.survivor_maps_before_invalidate -gt 0 -and $live.maps -gt $d.survivor_maps_before_invalidate -and $d.survivor_retired.maps -ge $live.maps
}

$run = New-PocDirectory (Resolve-ValidationDir 'native' ([guid]::NewGuid().ToString('N')))
$summary = @{ schema = 1; stage = $Stage; cases = @(); error = $null; success = $false }
if ($Scenario -eq 'MenuEditHistory') {
    $summary.validation_scope = 'original-unmodified-cef-editor-menu'
    $summary.expected_cases = 4
    $summary.os_input_sent = $false
    $summary.task_12_1_accepted = $false
    $summary.task_12_2_accepted = $false
}
if ($Scenario -eq 'PrintDrainFence') {
    $summary.validation_scope = 'real-runtime-close-metadata-print-confirmation-barrier'
    $summary.expected_cases = 2
    $summary.print_job_submitted = $false
    $summary.task_13_2_accepted = $false
}
if ($faultMatrix) {
    $commonFaults = @('LateCreateClose', 'CreatePostRejected', 'CaptureFailureRecovery', 'CloseTimeout', 'RetiredFrameReplay')
    $faultScenarios = @{ Software = $commonFaults; D3d11 = $commonFaults + @('DeviceCreateFailure') }
    $summary.fault_plan = @(foreach ($threadMode in @('Integrated', 'Threaded')) {
        foreach ($renderer in @('Software', 'D3d11')) {
            foreach ($fault in $faultScenarios[$renderer]) {
                @{ mode = $threadMode; backend = $renderer; scenario = $fault }
            }
        }
    })
    $summary.non_applicable = @(@{ scenario = 'DeviceCreateFailure'; backend = 'Software'; reason = 'SoftwareRequired does not construct a D3d11 device; CaptureFailureRecovery exercises its required capture path' })
}
$oldTarget = $env:CARGO_TARGET_DIR
$oldCef = $env:CEF_PATH
try {
    $cef = Resolve-CefPath $CefPath
    $env:CEF_PATH = $cef
    $env:CARGO_TARGET_DIR = Resolve-TargetDir $TargetDir
    Push-Location $PocRoot
    try {
        # Build diagnostics remain visible; JSON is used only to select real paths.
        $ErrorActionPreference = 'Continue'
        $build = @(& cargo build --locked --target $PocTarget -p rust-cef-runtime -p rust-cef-helper --features rust-cef-runtime/test-support --bin runtime-probe --bin rust-cef-helper --message-format=json-render-diagnostics 2>&1 | Tee-Object -FilePath (Join-Path $run 'build.log') | ForEach-Object { Write-Host $_; $_ })
        $buildExit = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        if ($buildExit -ne 0) { throw "Cargo build failed: $buildExit" }
    } finally { $ErrorActionPreference = 'Stop'; Pop-Location }
    $messages = @()
    foreach ($line in $build) {
        if ($line -is [string] -and $line.StartsWith('{')) { $messages += ($line | ConvertFrom-Json) }
    }
    $hostArtifact = Get-ExecutableArtifact $messages 'runtime-probe'
    $helperArtifact = Get-ExecutableArtifact $messages 'rust-cef-helper'
    $bundleRoot = $run
    if ($StageBundleInTemp) {
        $bundleRoot = Resolve-PocTempDir ('component-pressure-' + (Split-Path -Leaf $run))
        New-Item -ItemType Directory -Path $bundleRoot -ErrorAction Stop | Out-Null
    }
    $bundle = Join-Path $bundleRoot ([string][char]0x53d1 + [char]0x884c + ' Native bundle with spaces')
    if ($StageBundleInTemp) { $summary.bundle_path = $bundle }
    & (Join-Path $PSScriptRoot 'package-component.ps1') -HostExecutable $hostArtifact -HelperExecutable $helperArtifact -CefPath $cef -OutputDirectory $bundle -HostBuildVersion 'runtime-probe-test-support'
    if ($SpellingDictionaryFixture) { Add-SpellingDictionaryFixture -Dictionary $SpellingDictionaryFixture -License $SpellingDictionaryLicense -Bundle $bundle -Run $run }
    $hostFile = Join-Path $bundle ([IO.Path]::GetFileName($hostArtifact))
    $helperFile = Join-Path $bundle ([IO.Path]::GetFileName($helperArtifact))
    Write-JsonFile @{ host_artifact = $hostArtifact; helper_artifact = $helperArtifact; host_sha256 = (Get-FileHash -LiteralPath $hostFile).Hash; helper_sha256 = (Get-FileHash -LiteralPath $helperFile).Hash } (Join-Path $run 'artifact-identity.json')
    $modes = if ($Mode -eq 'All') { @('Integrated', 'Threaded') } else { @($Mode) }
    $backends = if ($Backend -eq 'All') { @('Software', 'D3d11') } else { @($Backend) }
    $scenarios = if ($Scenario -eq 'All') { @('Smoke', 'Multiple', 'Creating', 'Loading', 'PendingRequest', 'InFlight', 'CloseTimeout') } else { @($Scenario) }
    $hosts = @()
    if ($HostKind -ne 'Gpui') { $hosts += @{ kind = 'native'; executable = $hostFile; helper = $helperFile; scenarios = $scenarios } }
    if ($HostKind -ne 'Native') {
        Push-Location $PocRoot
        try {
            $ErrorActionPreference = 'Continue'
            $gpuiBuild = @(& cargo build --manifest-path validation/gpui-runtime-probe/Cargo.toml --locked --target $PocTarget --message-format=json-render-diagnostics 2>&1 | Tee-Object -FilePath (Join-Path $run 'gpui-build.log') | ForEach-Object { Write-Host $_; $_ })
            $gpuiExit = $LASTEXITCODE
            $ErrorActionPreference = 'Stop'
            if ($gpuiExit -ne 0) { throw "GPUI validation host build failed: $gpuiExit" }
        } finally { $ErrorActionPreference = 'Stop'; Pop-Location }
        $gpuiMessages = @()
        foreach ($line in $gpuiBuild) { if ($line -is [string] -and $line.StartsWith('{')) { $gpuiMessages += ($line | ConvertFrom-Json) } }
        $gpuiArtifact = Get-ExecutableArtifact $gpuiMessages 'gpui-runtime-probe'
        $gpuiBundle = Join-Path $run ([string][char]0x53d1 + [char]0x884c + ' GPUI bundle with spaces')
        & (Join-Path $PSScriptRoot 'package-component.ps1') -HostExecutable $gpuiArtifact -HelperExecutable $helperArtifact -CefPath $cef -OutputDirectory $gpuiBundle -HostBuildVersion 'gpui-runtime-probe-test-support'
        $gpuiHost = Join-Path $gpuiBundle ([IO.Path]::GetFileName($gpuiArtifact))
        $gpuiHelper = Join-Path $gpuiBundle ([IO.Path]::GetFileName($helperArtifact))
        Write-JsonFile @{ host_artifact = $gpuiArtifact; helper_artifact = $helperArtifact; host_sha256 = (Get-FileHash -LiteralPath $gpuiHost).Hash; helper_sha256 = (Get-FileHash -LiteralPath $gpuiHelper).Hash } (Join-Path $run 'gpui-artifact-identity.json')
        $hosts += @{ kind = 'gpui'; executable = $gpuiHost; helper = $gpuiHelper; scenarios = @('Smoke') }
    }
    foreach ($hostSpec in $hosts) {
      $hostFile = $hostSpec.executable
      $helperFile = $hostSpec.helper
      foreach ($selectedMode in $modes) {
        foreach ($selectedBackend in $backends) {
          $selectedScenarios = if ($faultMatrix) { $faultScenarios[$selectedBackend] } else { $hostSpec.scenarios }
          foreach ($selectedScenario in $selectedScenarios) {
            $case = Join-Path $run "$($hostSpec.kind)-$selectedMode-$selectedBackend-$selectedScenario"
            $cwd = Join-Path $case 'unrelated cwd'
            New-Item -ItemType Directory -Path $cwd -Force | Out-Null
            $reportPath = Join-Path $case 'probe.json'
            $gate = Join-Path $case 'observer.release'
            $launch = Invoke-Probe $hostFile @('--mode', $selectedMode, '--backend', $selectedBackend, '--report', $reportPath, '--helper', $helperFile, '--observer-gate', $gate, '--scenario', $selectedScenario) $cwd $gate $helperFile
            Write-JsonFile @{ children = $launch.children; image_verified = $launch.image_verified; live_images_verified = $launch.live_images_verified; observer_error = $launch.observer_error; remaining_processes = $launch.remaining_processes; observation_samples = $launch.observation_samples; residual_observations = $launch.residual_observations } (Join-Path $case 'child-images.json')
            [IO.File]::WriteAllText((Join-Path $case 'stdout.log'), $launch.stdout)
            [IO.File]::WriteAllText((Join-Path $case 'stderr.log'), $launch.stderr)
            $observed = $null
            $passed = $false
            if (Test-Path -LiteralPath $reportPath) {
                $observed = Get-Content -LiteralPath $reportPath -Raw -Encoding UTF8 | ConvertFrom-Json
                $threadRule = ($selectedMode -eq 'Integrated' -and $observed.cef_ui_thread_id -eq $observed.main_thread_id) -or
                    ($selectedMode -eq 'Threaded' -and $observed.cef_ui_thread_id -ne $observed.main_thread_id -and $observed.manual_pump_count -eq 0)
                $needsFrame = $selectedScenario -notin @('Creating', 'Loading')
                $earlyD3d11 = -not $needsFrame -and $selectedBackend -eq 'D3d11'
                $expectedBrowsers = switch ($selectedScenario) { 'Multiple' {3} 'RetiredFrameReplay' {2} 'CommandPressureMenuClose' {2} 'EventPressureMenuClose' {2} 'PressureProfileFlushDrain' {2} 'SourceVisitorDrain' {2} 'SourceProfileFlushDrain' {2} 'SourceFindProfileDrain' {2} 'TitleSourceFindProfileDrain' {2} 'ManagedPressureSourceFindDrain' {2} 'ManagedPressureSourceFindDrainAux' {2} 'ProfileFlushDrain' {2} 'MenuFindIsolation' {2} 'PrintDrainFence' {2} 'JsBridgeSchemes' {2} 'HostCapabilities' {2} 'OsrParity' {2} default {1} }
                $backendValid = $observed.actual_backend -eq $selectedBackend -or (-not $needsFrame -and $selectedBackend -eq 'Software' -and $null -eq $observed.actual_backend)
                $scenarioValid = if ($earlyD3d11) { Test-AcceleratedEarlyClose $observed $selectedScenario } elseif ($selectedScenario -eq 'Creating') { ($observed.created + $observed.cancelled_creates) -eq 1 } elseif ($selectedScenario -eq 'Loading') { $observed.created -eq 1 -and $observed.loading_request_observed } elseif ($selectedScenario -eq 'NativeChildHwnd') {
                    $observed.created -eq 3 -and $observed.valid_frame_browsers -eq 1 -and $observed.frames -gt 0 -and $observed.first_frame_width -gt 0 -and $observed.first_frame_height -gt 0 -and $observed.task_post_failures -eq 0 -and (Test-NativeChildHwnd $observed)
                } else {
                    $observed.created -eq $expectedBrowsers -and $observed.valid_frame_browsers -eq $expectedBrowsers -and $observed.frames -gt 0 -and $observed.first_frame_width -gt 0 -and $observed.first_frame_height -gt 0
                }
                $scenarioValid = $scenarioValid -and ($selectedScenario -ne 'PendingRequest' -or $observed.cancelled_requests -eq 1) -and ($selectedScenario -ne 'InFlight' -or ($observed.in_flight_peak -gt 0 -and $observed.pending_tasks_at_shutdown -gt 0)) -and ($selectedScenario -ne 'CloseTimeout' -or $observed.timeout_blocked_observed)
                if ($selectedScenario -eq 'CaptureTiming') {
                    $scenarioValid = $scenarioValid -and (Test-CaptureTiming $observed)
                }
                if ($selectedScenario -in @('ProfileFlushDrain','PressureProfileFlushDrain','SourceProfileFlushDrain','SourceFindProfileDrain','TitleSourceFindProfileDrain','ManagedPressureSourceFindDrain')) {
                    $scenarioValid = $scenarioValid -and (Test-ProfileFlushDrain $observed)
                }
                if ($selectedScenario -eq 'CloseTimeout') {
                    $witness = $observed.close_timeout
                    $scenarioValid = $scenarioValid -and $null -ne $witness -and
                        @($witness.browser).Count -eq 2 -and $witness.browser[0] -gt 0 -and $witness.browser[1] -gt 0 -and
                        @($witness.poll_open_browsers).Count -eq 1 -and @($witness.finish_open_browsers).Count -eq 1 -and
                        ($witness.poll_open_browsers[0] -join ',') -ceq ($witness.browser -join ',') -and
                        ($witness.finish_open_browsers[0] -join ',') -ceq ($witness.browser -join ',') -and
                        $witness.browser_closing_before_release -and $witness.closed_events_before_release -ceq 0 -and
                        $witness.shutdown_calls_before_release -ceq 0
                }
                if ($selectedScenario -eq 'LateCreateClose') {
                    $witness = $observed.late_create_close
                    $scenarioValid = $observed.created -eq 2 -and $observed.valid_frame_browsers -eq 1 -and
                        $observed.frames -gt 0 -and $observed.first_frame_width -eq 320 -and $observed.first_frame_height -eq 240 -and
                        $observed.task_post_failures -eq 0 -and $null -ne $witness -and
                        $witness.callback_after_accepted_close_intent -and $witness.frame_source_closed -and
                        $witness.frames_before_recovery -eq 0 -and @($witness.late_browser).Count -eq 2 -and
                        @($witness.recovered_browser).Count -eq 2 -and $witness.recovered_browser[0] -eq $witness.late_browser[0] -and
                        $witness.recovered_browser[1] -gt $witness.late_browser[1]
                }
                if ($selectedScenario -in @('CommandPressureMenuClose','EventPressureMenuClose','PressureProfileFlushDrain')) {
                    $q = $observed.command_pressure_menu_close
                    $m = $observed.menu_probe
                    $scenarioValid = $scenarioValid -and $null -ne $q -and $null -ne $m -and $observed.task_post_failures -eq 0
                    if ($scenarioValid) {
                        $p = $q.pressure
                        $scenarioValid = $p.thread -eq $observed.cef_ui_thread_id -and $p.accepted -eq 256 -and $p.dispatched -eq 256 -and
                            $p.pending_at_close -eq 256 -and $p.first_sequence -gt 0 -and $null -eq $p.error -and
                            $p.menu_pending_before -eq 1 -and $p.menu_pending_at_close -eq 1 -and
                            $p.overflow_refused -and @($p.close_task).Count -eq 2 -and
                            $p.close_task[0] -eq $q.closed_browser[0] -and $p.close_task[1] -gt 0 -and
                            $p.new_command_refused_after_intent -and
                            @($q.closed_events).Count -eq 2 -and
                            ($q.closed_events[0][0] -join ',') -ceq ($q.closed_browser -join ',') -and
                            ($q.closed_events[1][0] -join ',') -ceq ($q.survivor_browser -join ',') -and
                            $q.closed_events[0][1] -gt 0 -and $q.closed_events[1][1] -gt $q.closed_events[0][1] -and
                            @($q.closed_browser).Count -eq 2 -and @($q.survivor_browser).Count -eq 2 -and
                            $q.closed_browser[0] -eq $q.survivor_browser[0] -and $q.survivor_browser[1] -gt $q.closed_browser[1] -and
                            $q.first_source_closed -and $q.frame_before -gt 0 -and $q.frame_after -gt $q.frame_before -and
                            $m.deferred -and $m.retired -and $m.cef_ui_thread -eq $p.thread -and $m.pending_continuations -eq 0 -and
                            $m.copies -eq 1 -and $m.cancel_calls -eq 1 -and $m.cancel_returns -eq 1 -and $m.dismissed_during_cancel -eq 1 -and
                            ($m.dismissal_work -join ',') -eq '1' -and @($m.witnesses).Count -eq 1 -and
                            ($m.witnesses[0].browser -join ',') -ceq ($q.closed_browser -join ',') -and $null -eq $m.error
                        if ($scenarioValid -and $selectedScenario -in @('EventPressureMenuClose','PressureProfileFlushDrain')) {
                            $e = $observed.event_pressure
                            $scenarioValid = $null -ne $e -and ($e.browser -join ',') -ceq ($q.closed_browser -join ',') -and
                                $e.queued -eq 256 -and $e.rejected -eq 1 -and $e.last_rejected_sequence -gt 0 -and
                                $e.accepted_sequences_ordered -and $e.retained_titles_ordered -and $e.resynced_title -eq 257 -and
                                $e.ordinary_depth_at_first_close -eq 256 -and $e.first_close_sequence -gt $e.last_rejected_sequence -and
                                $e.first_close_sequence -eq $q.closed_events[0][1]
                        }
                        if ($scenarioValid -and $selectedScenario -eq 'PressureProfileFlushDrain') {
                            $f = $observed.profile_flush_drain
                            $originalFlush = ($f.profile -join ',') + ',' + $f.sequence
                            $scenarioValid = ($p.held_cookie_flush_at_close -join ',') -ceq $originalFlush -and
                                ($f.first -join ',') -ceq ($q.closed_browser -join ',') -and
                                ($f.survivor -join ',') -ceq ($q.survivor_browser -join ',') -and
                                $f.survivor_frame_before -eq $q.frame_before -and $f.survivor_frame_after -eq $q.frame_after -and
                                $f.closed_events[0][1] -eq $q.closed_events[0][1] -and $f.closed_events[1][1] -eq $q.closed_events[1][1] -and
                                $p.close_task[0] -eq $f.release_task[0] -and $p.close_task[1] -lt $f.release_task[1]
                            if ($scenarioValid -and $selectedBackend -eq 'D3d11') {
                                $scenarioValid = Test-AcceleratedPressureCapture $q $observed.cef_ui_thread_id
                            }
                        }
                    }
                }
                if ($selectedScenario -eq 'RetiredFrameReplay') {
                    $q = $observed.retired_frame_replay
                    $scenarioValid = $scenarioValid -and $null -ne $q -and $observed.task_post_failures -eq 0 -and
                        @($q.old_browser).Count -eq 2 -and @($q.live_browser).Count -eq 2 -and
                        $q.old_browser[0] -gt 0 -and $q.old_browser[0] -eq $q.live_browser[0] -and
                        $q.old_browser[1] -gt 0 -and $q.old_browser[1] -lt $q.live_browser[1] -and
                        $q.closed_error -ceq 'Closed' -and $q.live_error -ceq 'WrongIdentity' -and
                        $q.old_source_closed_empty -and $q.live_frame_unchanged -and
                        $q.generation -gt 0 -and $q.retired_sequence -gt 0 -and $q.live_sequence_before -gt 0 -and
                        $q.live_sequence_after_repaint -gt $q.live_sequence_before -and
                        $q.old_cpu_bytes_before_drop -eq 307200 -and $q.old_cpu_bytes_after_drop -eq 0
                }
                if ($selectedScenario -eq 'CaptureFailureRecovery') {
                    $witness = $observed.capture_failure_recovery
                    if ($null -eq $witness) { $scenarioValid = $false } else {
                        $q = $witness.callback
                        $wantDelta = if ($selectedBackend -eq 'D3d11') { 1 } else { 0 }
                        $kind = if ($selectedBackend -eq 'D3d11') { 'IncompleteFrame' } else { 'InvalidDimensions' }
                        $d = $witness.retired_capture
                        $scenarioValid = $scenarioValid -and $null -ne $d -and
                            $witness.closed_source -and $null -ne $witness.final_cpu_bytes -and $witness.final_cpu_bytes -eq 0 -and
                            $d.failures -eq 1 -and $d.last_failure -ceq $kind -and $null -eq $d.last_failure_code -and
                            $d.retired -and -not $d.abandoned -and -not $d.native_resources_live -and
                            $d.owner_thread -eq $q.thread -and $d.retirement_thread -eq $q.thread -and
                            $d.requested_backend -ceq $selectedBackend -and $d.observed_backend -ceq $selectedBackend -and
                            $d.staging_bytes -eq 0 -and $d.live_imports -eq 0 -and $d.maps -eq $d.unmaps -and
                            $observed.task_post_failures -eq 0 -and
                            $q.backend -ceq $selectedBackend -and $q.thread -eq $observed.cef_ui_thread_id -and
                            $q.expected_error_returned -and $q.same_frame -and $q.live_imports_after -eq 0 -and
                            $q.budget_used[0] -gt 0 -and $q.budget_used[0] -eq $q.budget_used[1] -and
                            $witness.event_sequence -gt 0 -and $q.good_frame[0] -eq $q.generation_after -and
                            $witness.recovered_frame[0] -eq $q.good_frame[0] -and
                            $witness.recovered_frame[1] -gt $q.good_frame[1] -and
                            @($q.native_io_before).Count -eq 4 -and @($q.native_io_after).Count -eq 4
                        if ($scenarioValid) {
                            for ($i = 0; $i -lt 4; $i++) {
                                $scenarioValid = $scenarioValid -and ($q.native_io_after[$i] - $q.native_io_before[$i]) -eq $wantDelta
                            }
                        }
                    }
                }
                if ($selectedScenario -eq 'DeviceCreateFailure') {
                    $witness = $observed.device_create_failure
                    $scenarioValid = $scenarioValid -and $null -ne $witness -and
                        $observed.task_post_failures -eq 0 -and $witness.failed_event_sequence -gt 0 -and
                        $witness.rejected_frame_source_closed -and @($witness.rejected_browser).Count -eq 2 -and
                        @($witness.recovered_browser).Count -eq 2 -and $witness.recovered_browser[0] -eq $witness.rejected_browser[0] -and
                        $witness.recovered_browser[1] -gt $witness.rejected_browser[1] -and
                        $witness.capture.requested_backend -eq 'D3d11' -and $null -eq $witness.capture.observed_backend -and
                        $witness.capture.software_captures -eq 0 -and $witness.capture.d3d11_captures -eq 0 -and
                        $witness.capture.failures -eq 1 -and $witness.capture.last_failure -eq 'DeviceCreation' -and
                        $null -eq $witness.capture.last_failure_code -and $witness.capture.retired -and
                        -not $witness.capture.abandoned -and -not $witness.capture.native_resources_live -and
                        $witness.capture.owner_thread -eq $observed.cef_ui_thread_id -and
                        $witness.capture.retirement_thread -eq $observed.cef_ui_thread_id
                }
                if ($selectedScenario -eq 'CreatePostRejected') {
                    $witness = $observed.create_post_rejection
                    $scenarioValid = $scenarioValid -and $null -ne $witness -and
                        $observed.task_post_failures -eq 1 -and $witness.failed_event_sequence -gt 0 -and
                        $witness.rejected_frame_source_closed -and @($witness.rejected_browser).Count -eq 2 -and
                        @($witness.recovered_browser).Count -eq 2 -and
                        $witness.recovered_browser[0] -eq $witness.rejected_browser[0] -and
                        $witness.recovered_browser[1] -gt $witness.rejected_browser[1]
                }
                if($selectedScenario -eq 'MenuSpelling'){
                    $q=$observed.spelling
                    if($null -eq $q){$scenarioValid=$false}else{$s=$q.status;$f=$q.fixture;$scenarioValid=$scenarioValid -and $q.metadata_owner_only -and $q.activation_input_submitted -and @($q.activation_witness).Count -eq 2 -and $q.activation_witness[0] -and $q.activation_witness[1] -and $q.phase -eq 3 -and $q.word_bytes -eq 9 -and $q.selection_bytes -eq 9 -and $q.suggestion_count -gt 0 -and $q.frame_id_bytes -gt 0 -and $q.replacement_bytes -gt 0 -and $q.expected_hash -eq $q.observed_hash -and $q.original_input_event_count -eq 2 -and $q.effect_observed -and $q.menu_terminal_separate -and $q.dictionary_disabled_rejected -and $q.menu_retired -and $q.menu_failure -eq 'missing required host capability for menu command 206' -and $s.native_issued -eq 1 -and $s.replacement_issued -eq 1 -and $s.dictionary_issued -eq 0 -and -not $s.effect_verified -and $s.rejected -eq 0 -and $null -eq $s.first_failure -and $f.published -eq 1 -and $f.terminal -eq 1 -and $f.outstanding -eq 0 -and $null -eq $f.first_error}
                }
                if ($selectedScenario -eq 'PrintDrainFence') {
                    $scenarioValid = $scenarioValid -and (Test-PrintDrainFence $observed)
                }
                if ($selectedScenario -eq 'MenuEditHistory') {
                    $scenarioValid = $scenarioValid -and (Test-EditHistory $observed)
                }
                if ($selectedScenario -eq 'DesktopUx') {
                    $scenarioValid = $scenarioValid -and (Test-DesktopUx $observed)
                }
                if ($selectedScenario -eq 'JsBridgeSchemes') {
                    $scenarioValid = $scenarioValid -and (Test-JsBridgeSchemes $observed)
                }
                if ($selectedScenario -eq 'HostCapabilities') {
                    $scenarioValid = $scenarioValid -and (Test-HostCapabilities $observed)
                }
                if ($selectedScenario -eq 'OsrParity') {
                    $scenarioValid = $scenarioValid -and (Test-OsrParity $observed)
                }
                if($selectedScenario -eq 'MenuFind'){
                    $q=$observed.find
                    if($null -eq $q){$scenarioValid=$false}else{
                        $scenarioValid=$scenarioValid -and $q.metadata_owner_only -and $q.phase -eq 4 -and $q.started -eq 2 -and $q.finished -eq 2 -and $q.final_pending -eq 0 -and -not $q.faulted -and $null -eq $q.first_failure -and $null -eq $q.sink_error -and ($q.query_bytes -join ',') -eq '21,21' -and ($q.frame_id_bytes -join ',') -eq '34,34' -and ($q.local_sequences -join ',') -eq '1,2' -and ($q.matches -join ',') -eq '1' -and @($q.cef_identifiers).Count -eq 1 -and $q.cef_identifiers[0] -ge 0 -and @($q.active_ordinals).Count -eq 1 -and $q.active_ordinals[0] -ge 0 -and $q.active_ordinals[0] -le 1 -and @($q.callback_threads).Count -eq 1 -and $q.callback_threads[0] -eq $observed.cef_ui_thread_id -and ($q.outcomes -join ',') -eq 'Some(Result),Some(Cancelled)' -and $q.menu_terminal_separate -and $q.repeat_pending_before_close -and $q.stop_return_not_drain -and $q.operations_drained_after_close
                    }
                }
                if ($selectedScenario -eq 'MenuFindIsolation') {
                    $w = $observed.find_isolation
                    $scenarioValid = $scenarioValid -and $null -ne $w -and $observed.task_post_failures -eq 0
                    if ($scenarioValid) {
                        $a = $w.original -join ','; $b = $w.survivor -join ','
                        $a1 = "$a,1"; $a2 = "$a,2"; $b3 = "$b,3"
                        $f = $w.first; $p = $w.peer; $closes = @($w.closed_events)
                        $scenarioValid = $w.metadata_owner_only -and @($w.original).Count -eq 2 -and @($w.survivor).Count -eq 2 -and
                            $w.original[0] -eq $w.survivor[0] -and $w.original[1] -gt 0 -and $w.original[1] -lt $w.survivor[1] -and
                            ($w.pending_before_peer -join ',') -ceq $a2 -and ($w.pending_after_peer -join ',') -ceq $a2 -and
                            ($w.counts_at_first_close -join ',') -ceq '3,3,0' -and ($w.final_counts -join ',') -ceq '3,3,0' -and
                            $w.stop_return_not_drain -and $w.source_closed -and $w.survivor_result_unchanged -and $w.survivor_generation -gt 0 -and
                            $w.survivor_frame_before -gt 0 -and $w.survivor_frame_after -gt $w.survivor_frame_before -and
                            $closes.Count -eq 2 -and ($closes[0][0] -join ',') -ceq $a -and ($closes[1][0] -join ',') -ceq $b -and $closes[0][1] -gt 0 -and $closes[1][1] -gt $closes[0][1] -and
                            @($f.native_finals).Count -eq 1 -and @($p.native_finals).Count -eq 1 -and
                            ($f.native_finals[0][0] -join ',') -ceq $a1 -and ($p.native_finals[0][0] -join ',') -ceq $b3 -and
                            $f.native_finals[0][1] -ge 0 -and $p.native_finals[0][1] -gt $f.native_finals[0][1] -and
                            $f.native_finals[0][2] -eq 1 -and $p.native_finals[0][2] -eq 1 -and $f.native_finals[0][4] -eq $observed.cef_ui_thread_id -and $p.native_finals[0][4] -eq $observed.cef_ui_thread_id -and
                            @($f.finished).Count -eq 2 -and @($p.finished).Count -eq 1 -and
                            ($f.finished[0][0] -join ',') -ceq $a1 -and $f.finished[0][1] -ceq 'Some(Result)' -and
                            ($f.finished[1][0] -join ',') -ceq $a2 -and $f.finished[1][1] -ceq 'Some(Cancelled)' -and
                            ($p.finished[0][0] -join ',') -ceq $b3 -and $p.finished[0][1] -ceq 'Some(Result)' -and
                            $f.all_receipts_retired -and $p.all_receipts_retired -and
                            $f.menu.published -eq 3 -and $f.menu.terminal -eq 3 -and $f.menu.outstanding -eq 0 -and $null -eq $f.menu.first_error -and
                            $p.menu.published -eq 2 -and $p.menu.terminal -eq 2 -and $p.menu.outstanding -eq 0 -and $null -eq $p.menu.first_error
                    }
                }
                if ($selectedScenario -in @('SourceVisitorDrain','SourceProfileFlushDrain','SourceFindProfileDrain','TitleSourceFindProfileDrain','ManagedPressureSourceFindDrain')) {
                    $w = $observed.source_visitor_drain
                    $q = $observed.source_view
                    $scenarioValid = $scenarioValid -and $null -ne $w -and $null -ne $q -and $observed.task_post_failures -eq 0
                    if ($scenarioValid) {
                        $scenarioValid = @($w.operation).Count -eq 3 -and @($w.survivor).Count -eq 2 -and
                            $w.operation[0] -eq $w.survivor[0] -and $w.operation[1] -gt 0 -and $w.operation[1] -lt $w.survivor[1] -and
                            $w.operation[2] -gt 0 -and $w.source_closed -and $w.callback_received_before_release -and
                            $w.native_live_after_first_close -eq 1 -and $w.host_live_after_first_close -eq 0 -and
                            $w.survivor_frame_before -gt 0 -and $w.survivor_frame_after -gt $w.survivor_frame_before -and
                            $null -ne $w.shutdown_calls_before_release -and $w.shutdown_calls_before_release -eq 0 -and
                            $w.explicit_release_posted -and $w.held_reference_gone -and
                            @($w.closed_events).Count -eq 2 -and
                            ($w.closed_events[0][0] -join ',') -ceq ($w.operation[0..1] -join ',') -and
                            ($w.closed_events[1][0] -join ',') -ceq ($w.survivor -join ',') -and
                            $w.closed_events[0][1] -gt 0 -and $w.closed_events[1][1] -gt $w.closed_events[0][1] -and
                            $q.phase -eq 3 -and $q.metadata_owner_only -and -not $q.budget_case -and -not $q.faulted -and
                            $q.started -eq 1 -and $q.finished -eq 1 -and $q.publications -eq 1 -and $q.close_requests -eq 1 -and $q.close_acks -eq 1 -and
                            $q.final_pending -eq 0 -and $q.final_native_live -eq 0 -and $q.final_host_live -eq 0 -and
                            $q.barrier_after_menu_terminal -and $null -eq $q.first_failure -and $null -eq $q.sink_error -and
                            @($q.operation_ids).Count -eq 1 -and ($q.operation_ids[0] -join ',') -ceq ($w.operation -join ',') -and
                            @($q.source_bytes).Count -eq 1 -and $q.source_bytes[0] -gt 21 -and $q.source_bytes[0] -le 8192 -and
                            ($q.outcomes -join ',') -ceq 'Some(Cancelled)'
                        if ($scenarioValid -and $selectedScenario -in @('SourceProfileFlushDrain','SourceFindProfileDrain','TitleSourceFindProfileDrain','ManagedPressureSourceFindDrain')) {
                            $f = $observed.profile_flush_drain
                            $originalFlush = ($f.profile -join ',') + ',' + $f.sequence
                            $scenarioValid = -not $w.blocked_only_by_source -and $w.source_retired_before_flush_release -and
                                ($w.flush_marker_at_joint_barrier -join ',') -ceq $originalFlush -and
                                ($f.first -join ',') -ceq ($w.operation[0..1] -join ',') -and
                                ($f.survivor -join ',') -ceq ($w.survivor -join ',') -and
                                $f.survivor_frame_before -eq $w.survivor_frame_before -and $f.survivor_frame_after -eq $w.survivor_frame_after -and
                                $f.closed_events[0][1] -eq $w.closed_events[0][1] -and $f.closed_events[1][1] -eq $w.closed_events[1][1]
                        } elseif ($scenarioValid) {
                            $scenarioValid = $w.blocked_only_by_source
                        }
                        if ($scenarioValid -and $selectedScenario -in @('SourceFindProfileDrain','TitleSourceFindProfileDrain','ManagedPressureSourceFindDrain')) {
                            $scenarioValid = Test-JointFindDrain $observed
                            if ($scenarioValid -and $selectedScenario -in @('TitleSourceFindProfileDrain','ManagedPressureSourceFindDrain')) {
                                $scenarioValid = Test-JointTitlePressure $observed
                            }
                            if ($scenarioValid -and $selectedScenario -eq 'ManagedPressureSourceFindDrain') {
                                $scenarioValid = (Test-ManagedPressureDrain $observed) -and (Test-LateManagedMenuReply $observed)
                                if ($scenarioValid -and $selectedBackend -eq 'D3d11') {
                                    $scenarioValid = Test-AcceleratedPressureCapture $w $observed.cef_ui_thread_id
                                }
                            }
                        }
                    }
                }
                if($selectedScenario -in @('MenuSource','MenuSourceBudget')){
                    $q=$observed.source_view
                    if($null -eq $q){$scenarioValid=$false}else{
                        $scenarioValid=$scenarioValid -and $q.metadata_owner_only -and -not $q.faulted -and $null -eq $q.sink_error -and $q.final_pending -eq 0 -and $q.final_native_live -eq 0 -and $q.final_host_live -eq 0
                        if($selectedScenario -eq 'MenuSourceBudget'){$scenarioValid=$scenarioValid -and $q.budget_case -and $q.budget_rejected -and $q.started -eq 1 -and $q.finished -eq 1 -and $q.publications -eq 0 -and $q.first_failure -eq 'Budget' -and (@($q.outcomes) -join ',') -eq 'Some(Failed)'}
                        else{$scenarioValid=$scenarioValid -and -not $q.budget_case -and $q.started -eq 2 -and $q.finished -eq 2 -and $q.publications -eq 2 -and $q.close_requests -eq 1 -and $q.close_acks -eq 1 -and $null -eq $q.first_failure -and $q.barrier_after_menu_terminal -and $q.viewer_pending_at_shutdown -and @($q.source_bytes).Count -eq 2 -and @($q.source_bytes|Where-Object {$_ -le 21 -or $_ -gt 8192}).Count -eq 0 -and (@($q.frame_id_bytes) -join ',') -eq '34,34' -and (@($q.outcomes) -join ',') -eq 'Some(Closed),Some(Cancelled)'}
                    }
                }
                if($selectedScenario -eq 'MenuBridge'){
                    $b=$observed.menu_bridge
                    if($null -eq $b){$scenarioValid=$false}else{
                    $s=$b.status;$f=$b.fixture
                    $scenarioValid=$scenarioValid -and ( $b.selection_observed -and $b.stale_selection_rejected -and $b.old_id_rejected -and $b.native_navigation_cancelled -and $b.missing_managed_capability_cancelled -and $b.binding_metadata_only -and $b.pending_after_close -eq 0 -and
                        $s.retired -and $s.opened -eq 3 -and $s.published -eq 3 -and $s.cancelled -eq 4 -and $s.actions_submitted -eq 1 -and $s.native_dismissed -eq 4 -and $s.post_failures -eq 0 -and $s.invalidated -ge 1 -and $s.first_failure -eq 'missing required host capability for menu command 131' -and
                        $null -eq $f.first_error -and $f.published -eq 3 -and $f.terminal -eq 3 -and $f.outstanding -eq 0 -and (@($f.terminal_ids) -join ',') -eq '1,2,3' -and $f.frame_id_bytes -gt 0 -and @($f.selection_bytes).Count -eq 3 -and $f.selection_bytes[0] -eq 0 -and $f.selection_bytes[1] -ge 21)
                    }
                }
                if ($selectedScenario -in @('Menu','MenuDeferred')) {
                    $m=$observed.menu_probe
                    $menuCount=if($selectedScenario -eq 'MenuDeferred'){3}else{2}
                    $scenarioValid=$scenarioValid -and $null -ne $m -and $null -eq $m.error -and $m.retired -and -not $m.enabled -and
                        $m.input_rounds -eq $menuCount -and $m.cef_mouse_events -eq ($menuCount*2) -and $m.fixture_builds -eq $menuCount -and $m.captures -eq $menuCount -and $m.copies -eq $menuCount -and
                        $m.cancel_calls -eq $menuCount -and $m.cancel_returns -eq $menuCount -and $m.dismissed -eq $menuCount -and $m.dismissed_during_cancel -eq $menuCount -and $m.uncorrelated_dismissed -eq 0 -and
                        $null -eq $m.answering -and $m.cef_ui_thread -eq $observed.cef_ui_thread_id -and @($m.witnesses).Count -eq $menuCount
                    if($selectedScenario -eq 'MenuDeferred'){
                        $scenarioValid=$scenarioValid -and $m.deferred -and $m.pending_continuations -eq 0 -and
                            (@($m.dismissal_work) -join ',') -eq '1,1,1' -and (@($observed.menu_pending_witnesses) -join ',') -eq '1,1,1' -and
                            $observed.menu_retired_lease_with_pending -and $observed.menu_explicit_cancel_posts -eq 2
                    }
                    for($i=0;$i -lt @($m.witnesses).Count;$i++){
                        $w=$m.witnesses[$i]
                        $scenarioValid=$scenarioValid -and $w.sequence -eq ($i+1) -and $w.anchor[0] -eq 20 -and $w.anchor[1] -eq 20 -and $w.frame_id_bytes -gt 0 -and $w.browser[0] -gt 0 -and $w.browser[1] -gt 0 -and $w.native_browser_id -gt 0 -and $w.context_total_bytes -le 8192 -and $w.suggestions -le 5 -and
                            $w.capture_thread -eq $m.cef_ui_thread -and $w.root_rows -eq 6 -and $w.child_rows -eq 3 -and $w.fixture_faithful -and $w.binding_metadata_only
                    }
                }
                $passed = $launch.exit_code -eq 0 -and -not $launch.timed_out -and $launch.streams_complete -and ($launch.image_verified -or (-not $needsFrame -and -not $earlyD3d11)) -and $launch.remaining_processes.Count -eq 0 -and $null -eq $launch.observer_error -and
                    $observed.schema -eq 1 -and $observed.pid -eq $launch.pid -and $observed.exit_code -eq 0 -and $null -eq $observed.error -and
                    $observed.requested_mode -eq $selectedMode -and $observed.requested_backend -eq $selectedBackend -and
                    $observed.actual_mode -eq $selectedMode -and $backendValid -and $observed.scenario -eq $selectedScenario -and
                    $observed.main_thread_id -gt 0 -and $observed.cef_ui_thread_id -gt 0 -and $threadRule -and
                    $observed.init_thread_id -eq $observed.main_thread_id -and $observed.shutdown_thread_id -eq $observed.main_thread_id -and $observed.app_reference_retained -eq $false -and
                    $observed.tasks_posted -gt 0 -and $observed.pending_tasks -eq 0 -and ($observed.tasks_completed + $observed.task_post_failures) -eq $observed.tasks_posted -and
                    $observed.closed -eq $observed.created -and $scenarioValid -and $observed.cef_shutdown_calls -eq 1 -and $observed.pending_requests -eq 0 -and
                    $observed.native_calls -gt 0 -and $observed.native_calls_after_shutdown -eq 0 -and $observed.post_shutdown_rejections -gt 0
                if ($hostSpec.kind -eq 'gpui') {
                    $passed = $passed -and $observed.kind -eq 'gpui' -and $observed.notify_count -gt 0 -and $observed.ffi_to_ui -gt 0 -and $observed.borrow_violation -eq 0 -and ($selectedMode -ne 'Threaded' -or $observed.frame_notifications_consumed -gt 0)
                }
            }
            $summary.cases += @{ kind = $hostSpec.kind; mode = $selectedMode; backend = $selectedBackend; scenario = $selectedScenario; pid = $launch.pid; exit_code = $launch.exit_code; timed_out = $launch.timed_out; passed = $passed; renderer_image_verified = $launch.image_verified; report = $reportPath; observed = $observed }
            Write-Host "$($hostSpec.kind)/$selectedMode/$selectedBackend/$selectedScenario pid=$($launch.pid) exit=$($launch.exit_code) passed=$passed"
          }
        }
      }
    }
    $summary.success = $summary.cases.Count -gt 0 -and @($summary.cases | Where-Object { -not $_.passed }).Count -eq 0
    if ($faultMatrix) {
        $planned = @($summary.fault_plan | ForEach-Object { "$($_.mode)/$($_.backend)/$($_.scenario)" })
        $actual = @($summary.cases | ForEach-Object { "$($_.mode)/$($_.backend)/$($_.scenario)" })
        if ($planned.Count -ne 22 -or $actual.Count -ne $planned.Count -or
            ($actual -join ',') -cne ($planned -join ',')) {
            throw 'Faults matrix did not execute all 22 planned cells exactly once'
        }
    }
    if ($Scenario -in @('CaptureTiming','ProfileFlushDrain','MenuFindIsolation','MenuEditHistory','JsBridgeSchemes','HostCapabilities','OsrParity','NativeChildHwnd')) {
        $keys = @($summary.cases | ForEach-Object { "$($_.kind)/$($_.mode)/$($_.backend)/$($_.scenario)" })
        $expected = "native/Integrated/Software/$Scenario,native/Integrated/D3d11/$Scenario,native/Threaded/Software/$Scenario,native/Threaded/D3d11/$Scenario"
        if (($keys -join ',') -cne $expected) { throw "$Scenario did not run all four required cells" }
    }
    if ($Scenario -in @('PressureProfileFlushDrain','SourceProfileFlushDrain','SourceFindProfileDrain','TitleSourceFindProfileDrain','ManagedPressureSourceFindDrain','PrintDrainFence')) {
        $keys = @($summary.cases | ForEach-Object { "$($_.kind)/$($_.mode)/$($_.backend)/$($_.scenario)" })
        $required = "native/Integrated/$Backend/$Scenario,native/Threaded/$Backend/$Scenario"
        if (($keys -join ',') -cne $required) {
            throw "$Scenario did not run both requested $Backend cells"
        }
    }
    if ($Scenario -eq 'Frames') {
        & python (Join-Path $PSScriptRoot 'compare-component-frames.py') --run-dir $run
        if ($LASTEXITCODE -ne 0) { throw "Native frame image comparison failed: $LASTEXITCODE" }
    }
} catch { $summary.success = $false; $summary.error = $_.Exception.Message; Write-Host "Native validation failed: $($summary.error)" }
finally {
    $env:CARGO_TARGET_DIR = $oldTarget
    $env:CEF_PATH = $oldCef
    Write-JsonFile $summary (Join-Path $run 'result.json')
    Write-Host "Evidence: $run"
}
if (-not $summary.success) { exit 1 }
