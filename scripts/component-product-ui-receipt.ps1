# Pure receipt predicates for the two-phase passive product-UI matrix.
# Never launches a host, reads only already-written JSON, and never turns a
# missing sample, mixed DPI or mismatched scenario into a pass.

function Get-ProductUiCount($Value, [long]$Minimum = 0) {
    return ($Value -is [int] -or $Value -is [long]) -and $Value -ge $Minimum
}
function Test-ProductUiScale($Value) {
    if ($Value -isnot [single] -and $Value -isnot [double]) { return $false }
    $v = [double]$Value
    return [double]::IsFinite($v) -and $v -gt 0
}
function Get-ProductUiDistribution($Value) {
    if ($null -eq $Value -or -not (Get-ProductUiCount $Value.samples 1)) { return $null }
    $n = $Value.samples
    foreach ($k in @('p50_ns','p95_ns','max_ns')) {
        if (-not (Get-ProductUiCount $Value.$k)) { return $null }
    }
    if ($Value.p50_ns -gt $Value.p95_ns -or $Value.p95_ns -gt $Value.max_ns) { return $null }
    return [pscustomobject]@{samples=$n;p50_ns=$Value.p50_ns;p95_ns=$Value.p95_ns;max_ns=$Value.max_ns}
}
function Test-ProductUiSeries($Rows) {
    return $null -ne $Rows -and @($Rows).Count -eq 32
}
function Get-ProductUiCell($d, $t, [string]$want, $Pids) {
    # One already-written cell pair; returns a compact cell or $null.
    if ("$($d.mode)/$($d.backend)" -cne $want -or "$($t.mode)/$($t.backend)" -cne $want) { return $null }
    foreach ($c in @($d, $t)) {
        if ($c.passed -isnot [bool] -or -not $c.passed -or -not (Get-ProductUiCount $c.exit_code) -or
            $c.exit_code -ne 0 -or -not (Get-ProductUiCount $c.pid 1) -or -not $Pids.Add($c.pid)) { return $null }
    }
    $o = $d.observed; $a = $t.observed
    if ($null -eq $o -or $null -eq $a) { return $null }
    if ($o.pid -ne $d.pid -or $o.mode -cne $d.mode -or $o.backend -cne $d.backend -or
        $o.observed_backend -cne "Some($($d.backend)Required)" -or
        -not (Get-ProductUiCount $o.created 2) -or -not (Get-ProductUiCount $o.valid_frames 1) -or
        $null -eq $o.paint -or -not (Get-ProductUiCount $o.paint.paint_calls 1) -or
        -not (Test-ProductUiScale $o.paint.scale_factor)) { return $null }
    if ($a.pid -ne $t.pid -or $a.mode -cne $t.mode -or $a.backend -cne $t.backend -or
        $a.observed_backend -cne "Some($($t.backend)Required)" -or
        -not (Get-ProductUiCount $a.tasks_completed 1) -or $a.tasks_completed -ne $a.tasks_posted -or
        -not (Test-ProductUiScale $a.paint_timing.scale) -or
        $null -ne $a.paint_timing_partial -or $null -ne $a.paint_timing_peer_partial) { return $null }
    $pair = $a.paint_timing_peer.window_pair; $peer = $a.paint_timing_peer.two_live_windows_b_timed_after_a
    if ($null -eq $pair -or $null -eq $peer) { return $null }
    if (-not (Get-ProductUiCount $pair.a_dpi 1) -or $pair.a_dpi -ne $pair.b_dpi -or
        $pair.a_hwnd -le 0 -or $pair.b_hwnd -le 0 -or $pair.a_hwnd -eq $pair.b_hwnd -or
        $pair.a_gpui -eq $pair.b_gpui -or -not $pair.a_hwnd_gone -or -not $pair.original_b_hwnd_still_owned) { return $null }
    if ([math]::Abs([double]$pair.a_dpi / 96.0 - [double]$a.paint_timing.scale) -gt 0.001) { return $null }
    if ([math]::Abs([double]$o.paint.scale_factor - [double]$a.paint_timing.scale) -gt 0.001) { return $null }
    $aRows = $a.paint_timing.rows; $bRows = $peer.rows
    if (-not (Test-ProductUiSeries $aRows) -or -not (Test-ProductUiSeries $bRows)) { return $null }
    foreach ($rows in @($aRows, $bRows)) {
        $prevSeq = 0; $prevPaint = [long]0; $ok = $true
        foreach ($r in $rows) {
            if (-not (Get-ProductUiCount $r.sequence 1) -or $r.sequence -le $prevSeq -or
                -not (Get-ProductUiCount $r.copy_ns 1) -or -not (Get-ProductUiCount $r.copy_to_paint_ns 1) -or
                -not (Get-ProductUiCount $r.paint_completed_ns 1) -or
                $r.copy_to_paint_ns -ne ($r.paint_completed_ns - $r.copy_completed_ns) -or
                $r.paint_completed_ns -le $prevPaint) { $ok = $false; break }
            $prevSeq = $r.sequence; $prevPaint = $r.paint_completed_ns
        }
        if (-not $ok) { return $null }
    }
    $stats = @()
    foreach ($lane in @($a.paint_timing, $peer)) {
        $copy = Get-ProductUiDistribution $lane.copy_ns
        $submit = Get-ProductUiDistribution $lane.copy_to_paint_ns
        if ($null -eq $copy -or $null -eq $submit) { return $null }
        $pump = if ($null -eq $lane.pump_ns) { $null } else { Get-ProductUiDistribution $lane.pump_ns }
        if ($lane.pump_calls -gt 0 -and $null -eq $pump) { return $null }
        $stats += [pscustomobject]@{pump=$pump;copy=$copy;submit=$submit;pump_calls=$lane.pump_calls}
    }
    if ($stats.Count -ne 2) { return $null }
    return [pscustomobject]@{
        mode = $d.mode; backend = $d.backend; dispatch_pid = $d.pid; timing_pid = $t.pid
        browsers = $o.created; paint_calls = $o.paint.paint_calls; tasks = $a.tasks_completed
        dpi = $pair.a_dpi; a_copy = $stats[0].copy; a_submit = $stats[0].submit
        b_copy = $stats[1].copy; b_submit = $stats[1].submit
        a_pump = $stats[0].pump; b_pump = $stats[1].pump
    }
}

function Test-ProductUiReceipts($Dispatch, $Timing) {
    if ($null -eq $Dispatch -or $null -eq $Timing) { return $null }
    if ($Dispatch.schema -ne 1 -or $Dispatch.stage -cne 'ComponentHost' -or $Dispatch.scenario -cne 'ProfileDispatch' -or
        $Dispatch.success -isnot [bool] -or -not $Dispatch.success -or $null -ne $Dispatch.error -or
        $Dispatch.validation_scope -cne 'passive-product-ui-multiple-profiles' -or
        -not (Get-ProductUiCount $Dispatch.expected_cases 1) -or $Dispatch.expected_cases -ne 4 -or
        @($Dispatch.cases).Count -ne 4) { return $null }
    if ($Timing.schema -ne 1 -or $Timing.stage -cne 'ComponentHost' -or $Timing.scenario -cne 'PaintLatency' -or
        $Timing.success -isnot [bool] -or -not $Timing.success -or $null -ne $Timing.error -or
        $Timing.validation_scope -cne 'passive-two-window-separate-copy-to-gpui-paint-measurements' -or
        -not (Get-ProductUiCount $Timing.expected_cases 1) -or $Timing.expected_cases -ne 4 -or
        @($Timing.cases).Count -ne 4) { return $null }
    $plan = @('Integrated/Software', 'Integrated/D3d11', 'Threaded/Software', 'Threaded/D3d11')
    $pids = [Collections.Generic.HashSet[int64]]::new()
    $cells = @(); $scales = @()
    for ($i = 0; $i -lt 4; $i++) {
        $cell = Get-ProductUiCell $Dispatch.cases[$i] $Timing.cases[$i] $plan[$i] $pids
        if ($null -eq $cell) { return $null }
        $cells += $cell; $scales += [double]$cell.dpi
    }
    if ($pids.Count -ne 8 -or @($scales | Select-Object -Unique).Count -ne 1) { return $null }
    return [pscustomobject]@{ scale = ([double]$scales[0] / 96.0); cells = $cells }
}
function Test-ProductUiBundle($Manifest) {
    # CEF/binding identity of one already-staged bundle; no file reading here.
    if ($null -eq $Manifest) { return $false }
    if ($Manifest.target -cne 'x86_64-pc-windows-msvc' -or $Manifest.cef_version -cne '156.0.31' -or
        $Manifest.binding_revision -cne '7b6bf84e76e919790bd61df077e29dc4132eb5e2') { return $false }
    $paths = @($Manifest.runtime | ForEach-Object { $_.path.ToLowerInvariant() })
    return $paths.Count -eq 234 -and @($paths | Select-Object -Unique).Count -eq 234
}
