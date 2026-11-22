#Requires -Version 7.0
# G4 passive startup/teardown and opt-in profile restart; no desktop input or menus.
[CmdletBinding()]
param(
    [string]$CefPath,
    [string]$TargetDir,
    [switch]$ProfileRestart,
    [switch]$PrintConfirmationProbe,
    [switch]$PrintNavigationProbe,
    [ValidateSet('Integrated','Threaded')][string]$PrintConfirmationMode = 'Threaded',
    [switch]$Release,
    [ValidateRange(45,180)][int]$TimeoutSeconds = 90
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($PrintConfirmationProbe -and ($ProfileRestart -or $Release)) { throw 'PrintConfirmationProbe requires the standalone passive dev host without restart' }
if (-not $PrintConfirmationProbe -and $PSBoundParameters.ContainsKey('PrintConfirmationMode')) { throw 'PrintConfirmationMode requires PrintConfirmationProbe' }
if ($PrintNavigationProbe -and -not $PrintConfirmationProbe) { throw 'PrintNavigationProbe requires PrintConfirmationProbe' }
if ($Release -and -not $ProfileRestart) { throw 'Release requires ProfileRestart in the distribution harness' }
. (Join-Path $PSScriptRoot 'common.ps1')
. (Join-Path $PSScriptRoot 'component-process-observer.ps1')
. (Join-Path $PSScriptRoot 'component-probe-launcher.ps1')
. (Join-Path $PSScriptRoot 'component-distribution-restart.ps1')

function Write-Report($Value, [string]$Path) {
    [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
}

function Confirm-DistributionReleaseArtifact($Artifact, [string]$Name, [string]$ProfileDir) {
    $p = $Artifact.profile
    $expected = [IO.Path]::GetFullPath((Join-Path $ProfileDir "$Name.exe"))
    if ($p.opt_level -isnot [string] -or $p.opt_level -cne '3' -or
        $p.debug_assertions -isnot [bool] -or $p.debug_assertions -or
        $p.test -isnot [bool] -or $p.test -or
        [IO.Path]::GetFullPath($Artifact.executable) -ine $expected) {
        throw "Invalid Cargo release artifact for $Name"
    }
}

function Build-DistributionArtifact([string]$Name, [string[]]$CargoArgs, [string[]]$RustcArgs = @()) {
    $lines = @(& cargo @CargoArgs --message-format=json-render-diagnostics @RustcArgs 2>&1 |
        Tee-Object -FilePath (Join-Path $run "$Name-build.log") | ForEach-Object { Write-Host $_; $_ })
    if ($LASTEXITCODE -ne 0) { throw "Build $Name failed: $LASTEXITCODE" }
    $matches = @($lines | Where-Object { $_ -is [string] -and $_.StartsWith('{') } | ForEach-Object { $_ | ConvertFrom-Json } |
        Where-Object { $_.reason -eq 'compiler-artifact' -and $_.target.name -eq $Name -and $null -ne $_.executable })
    if ($matches.Count -ne 1) { throw "Expected one actual Cargo executable for $Name" }
    return $matches[0]
}

function Test-WithinDirectory([string]$Path, [string]$Root) {
    return $Path.Equals($Root, [StringComparison]::OrdinalIgnoreCase) -or
        $Path.StartsWith($Root.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)
}

function Find-OwnedDistributionWindow([int]$OwnedPid, [string]$ExpectedTitle) {
    if (-not ('DistWinEnum.U' -as [type])) {
        Add-Type -Namespace DistWinEnum -Name U -MemberDefinition @'
public delegate bool EnumWindowsProc(System.IntPtr hWnd, System.IntPtr lParam);
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, System.IntPtr lp);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(System.IntPtr h, out uint pid);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(System.IntPtr h, System.Text.StringBuilder s, int max);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(System.IntPtr h);
[DllImport("user32.dll")] public static extern System.IntPtr GetWindow(System.IntPtr h, uint cmd);
'@
    }
    $targetPid = [uint32]$OwnedPid
    $found = [System.Collections.Generic.List[int64]]::new()
    $cb = [DistWinEnum.U+EnumWindowsProc]{
        param($h, $lp)
        [uint32]$wp = 0
        [void][DistWinEnum.U]::GetWindowThreadProcessId($h, [ref]$wp)
        if ($wp -eq $targetPid -and [DistWinEnum.U]::IsWindowVisible($h) -and [DistWinEnum.U]::GetWindow($h, 4) -eq [IntPtr]::Zero) {
            $tb = New-Object System.Text.StringBuilder 256
            [void][DistWinEnum.U]::GetWindowTextW($h, $tb, 256)
            if ($tb.ToString() -ceq $ExpectedTitle) {
                $found.Add($h.ToInt64())
            }
        }
        return $true
    }
    [void][DistWinEnum.U]::EnumWindows($cb, [IntPtr]::Zero)
    if ($found.Count -eq 1) { return $found[0] }
    return 0L
}

function Get-OwnedWindow([int]$OwnedPid, [string]$Executable) {
    $p = Get-Process -Id $OwnedPid -ErrorAction Stop
    try {
        $hwnd = Find-OwnedDistributionWindow $OwnedPid 'GPUI-CEF public host'
        if ($p.HasExited -or $hwnd -le 0 -or $p.Path -ine $Executable) {
            throw 'Expected live staged host and original GPUI window'
        }
        return @{ pid = $OwnedPid; hwnd = $hwnd; image = $p.Path; creation_filetime = $p.StartTime.ToUniversalTime().ToFileTimeUtc() }
    } finally { $p.Dispose() }
}

function Confirm-EmbedLifecycle($Probe, $Launch, $Windows, [string]$Mode, [string]$Backend) {
    $expected = "Some(${Backend}Required)"
    $threadOk = if ($Mode -ceq 'Integrated') {
        $Probe.cef_ui_thread -eq $Probe.main_thread -and $Probe.manual_pump -gt 0
    } else { $Probe.cef_ui_thread -ne $Probe.main_thread -and $Probe.manual_pump -eq 0 }
    return $Launch.exit_code -eq 0 -and -not $Launch.timed_out -and $Launch.streams_complete -and
        $Launch.live_images_verified -and $null -eq $Launch.observer_error -and @($Launch.remaining_processes).Count -eq 0 -and
        $Windows.before.pid -eq $Launch.pid -and $Windows.after.pid -eq $Launch.pid -and $Windows.before.hwnd -eq $Windows.after.hwnd -and
        $Windows.before.creation_filetime -gt 0 -and $Windows.before.creation_filetime -eq $Windows.after.creation_filetime -and
        $Probe.schema -eq 1 -and $Probe.kind -ceq 'component-host' -and $Probe.pid -eq $Launch.pid -and
        $Probe.scenario -ceq 'Embed' -and $Probe.passive_distribution -and $Probe.mode -ceq $Mode -and $Probe.backend -ceq $Backend -and
        $Probe.observed_backend -ceq $expected -and $threadOk -and $Probe.cef_ui_thread -gt 0 -and $Probe.main_thread -gt 0 -and
        $Probe.init_thread -eq $Probe.main_thread -and $Probe.shutdown_thread -eq $Probe.main_thread -and
        $Probe.success -and $null -eq $Probe.error -and $Probe.created -eq 1 -and $Probe.closed -eq 1 -and
        $Probe.valid_frames -gt 0 -and $Probe.wake_consumed -gt 0 -and $Probe.borrow_violations -eq 0 -and
        $null -ne $Probe.paint -and $Probe.paint.paint_calls -gt 0 -and $Probe.paint.painted_sequence -gt 0 -and
        $Probe.paint.frame_width -gt 0 -and $Probe.paint.frame_height -gt 0 -and
        $null -ne $Probe.released_view -and $Probe.released_view.created_images -gt 0 -and
        $Probe.released_view.created_images -eq $Probe.released_view.released_images -and
        $Probe.duplicate_attachment_refused -and $Probe.host_alive_after_view_drop -and $Probe.view_release_observed_closing -and
        $Probe.finished -and $Probe.shutdown_calls -eq 1 -and $Probe.native_after -eq 0 -and
        $Probe.tasks_posted -gt 0 -and $Probe.tasks_completed -eq $Probe.tasks_posted -and $Probe.post_failures -eq 0 -and
        $Probe.pending_tasks -eq 0 -and $Probe.pending_requests -eq 0 -and $Probe.pending_profiles -eq 0 -and
        $Probe.profile_refs -eq 0 -and $Probe.final_cpu_used -eq 0 -and -not $Probe.app_retained -and -not $Probe.root_lease
}

function Invoke-DistributionEmbedCase([string]$Case, [string]$Mode, [string]$Backend, [string[]]$ExtraArgs = @(), [scriptblock]$BeforeDetach = $null) {
    $cwd = Join-Path $Case 'unrelated cwd'; $gate = Join-Path $Case 'observer.release'; $report = Join-Path $Case 'probe.json'
    New-Item -ItemType Directory -Path $Case,$cwd | Out-Null
    $windowReport = Join-Path $Case 'window-identity.json'
    $observeLifecycle = {
        param([int]$OwnedPid)
        $before = Get-OwnedWindow $OwnedPid $hostFile
        if ($null -ne $BeforeDetach) { & $BeforeDetach $OwnedPid }
        $detach = [IO.Path]::ChangeExtension($gate,'detach')
        [IO.File]::WriteAllText($detach + '.pending',[string]$OwnedPid)
        [IO.File]::Move($detach + '.pending',$detach)
        $detached = [IO.Path]::ChangeExtension($gate,'detached')
        $watch = [Diagnostics.Stopwatch]::StartNew()
        while (-not [IO.File]::Exists($detached)) {
            if ($watch.Elapsed.TotalSeconds -ge 8) { throw 'Owned View detach handshake timeout' }
            Start-Sleep -Milliseconds 25
        }
        $detachedText = $null
        for ($attempt = 0; $attempt -lt 10; $attempt++) {
            try { $detachedText = [IO.File]::ReadAllText($detached).Trim(); break }
            catch [IO.IOException] { if ($attempt -eq 9) { throw }; Start-Sleep -Milliseconds 25 }
        }
        if ($detachedText -cne [string]$OwnedPid) { throw 'View detach PID mismatch' }
        $after = Get-OwnedWindow $OwnedPid $hostFile
        Write-Report @{ before = $before; after = $after; desktop_pixels_observed = $false } $windowReport
    }
    $arguments = @('--mode',$Mode,'--backend',$Backend,'--report',$report,'--helper',$helperFile,'--observer-gate',$gate,'--scenario','Embed','--passive-distribution','true') + $ExtraArgs
    $launch = Invoke-Probe $hostFile $arguments $cwd $gate $helperFile $observeLifecycle (Join-Path $Case 'new user dirs')
    Write-Report $launch (Join-Path $Case 'launch.json')
    $record = @{ mode = $Mode; backend = $Backend; pid = $launch.pid; exit_code = $launch.exit_code; directory = $Case; passed = $false }
    if ((Test-Path -LiteralPath $report) -and (Test-Path -LiteralPath $windowReport)) {
        $probe = Get-Content -LiteralPath $report -Raw -Encoding UTF8 | ConvertFrom-Json
        $windows = Get-Content -LiteralPath $windowReport -Raw -Encoding UTF8 | ConvertFrom-Json
        $record.passed = Confirm-EmbedLifecycle $probe $launch $windows $Mode $Backend
    }
    return $record
}

function Test-PrintPanelLifecycle($Present, $Final, [int]$OwnedPid) {
    if ($null -eq $Present -or $null -eq $Final -or $Present.pid -ne $OwnedPid -or $Present.window_id -le 0 -or
        @($Present.operation).Count -ne 3 -or ($Present.operation -join ',') -cne '1,1,1') { return $false }
    foreach ($flag in @($Present.wrong_binding_refused,$Final.wrong_binding_refused,$Final.original_browser_closed,$Final.original_view_released,
        $Final.stale_panel_rejected,$Final.same_panel_at_invalidation,$Final.panel_entity_released,$Final.print_operation_reaped)) {
        if ($flag -isnot [bool] -or -not $flag) { return $false }
    }
    foreach ($flag in @($Present.submission_enabled,$Final.submission_enabled,$Final.physical_input,$Final.user_confirmation_observed)) {
        if ($flag -isnot [bool] -or $flag) { return $false }
    }
    return $Present.pending -eq 1 -and $Present.owner_paints -gt 0 -and
        ($Final.operation -join ',') -ceq ($Present.operation -join ',') -and $Final.window_id -eq $Present.window_id -and
        $Final.owner_paints -eq $Present.owner_paints -and ($Final.pending_by_cut -join ',') -ceq '1,1,0' -and $Final.outcome -ceq 'Cancelled'
}

function Test-PrintNavigationRevocation($Reload) {
    if ($null -eq $Reload) { return $false }
    $s = @($Reload.main_load_sequence)
    $wrongMenu = $Reload.PSObject.Properties['wrong_menu_refused']
    if ($null -eq $wrongMenu) { return $false }
    foreach ($flag in @($Reload.original_view_live_at_reap,$Reload.wrong_epoch_and_replay_refused,$wrongMenu.Value)) {
        if ($flag -isnot [bool] -or -not $flag) { return $false }
    }
    foreach ($value in $s + @($Reload.command,$Reload.paint_at_revocation,$Reload.paint_after_revocation)) {
        if ($value -isnot [int] -and $value -isnot [long]) { return $false }
    }
    return $s.Count -eq 4 -and 0 -lt $s[0] -and $s[0] -lt $s[1] -and $s[1] -lt $s[2] -and $s[2] -lt $s[3] -and
        $Reload.command -gt 0 -and $Reload.paint_at_revocation -gt 0 -and $Reload.paint_at_revocation -eq $Reload.paint_after_revocation
}

function Invoke-DistributionPrintPanel($Summary, [string]$Outside, [string]$Mode) {
    $case = Join-Path $Outside 'print-confirmation'
    $presentPath = Join-Path $case 'print-confirmation-present.json'
    $beforeDetach = {
        param([int]$OwnedPid)
        $r = Get-Content -LiteralPath $presentPath -Raw -Encoding UTF8 | ConvertFrom-Json
        if ($r.pid -ne $OwnedPid -or $r.owner_paints -le 0 -or $r.pending -ne 1 -or $r.submission_enabled) {
            throw 'Original print panel not painted or not held before detach'
        }
    }
    $extra = @('--print-confirmation-probe','true')
    if ($PrintNavigationProbe) { $extra += @('--print-navigation-probe','true') }
    $record = Invoke-DistributionEmbedCase $case $Mode 'Software' $extra $beforeDetach
    if ($record.passed) {
        $present = Get-Content -LiteralPath $presentPath -Raw -Encoding UTF8 | ConvertFrom-Json
        $probe = Get-Content -LiteralPath (Join-Path $case 'probe.json') -Raw -Encoding UTF8 | ConvertFrom-Json
        $record.passed = Test-PrintPanelLifecycle $present $probe.print_confirmation $record.pid
        if ($record.passed) {
            $reload = $probe.print_confirmation.PSObject.Properties['reload']
            $record.passed = if ($PrintNavigationProbe) {
                $null -ne $reload -and (Test-PrintNavigationRevocation $reload.Value)
            } else { $null -eq $reload }
        }
    }
    $Summary.cases += $record
    Write-Host "PrintConfirmation/$Mode/Software pid=$($record.pid) exit=$($record.exit_code) passed=$($record.passed)"
    $Summary.success = $record.passed
}

function Invoke-DistributionColdEmbed($Summary, [string]$Outside) {
    foreach ($mode in @('Integrated','Threaded')) { foreach ($backend in @('Software','D3d11')) {
        $record = Invoke-DistributionEmbedCase (Join-Path $Outside "$mode-$backend") $mode $backend
        $Summary.cases += $record
        Write-Host "Distribution/$mode/$backend pid=$($record.pid) exit=$($record.exit_code) passed=$($record.passed)"
        if (-not $record.passed) { throw 'Cold distribution cell failed; preserve the original evidence' }
    }}
    $case = Join-Path $Outside 'missing helper path'
    $cwd = Join-Path $case 'unrelated cwd'; $gate = Join-Path $case 'observer.release'; $report = Join-Path $case 'probe.json'
    New-Item -ItemType Directory -Path $case,$cwd | Out-Null
    $missing = Join-Path $bundle 'deliberately missing worker.exe'
    if (Test-Path -LiteralPath $missing) { throw 'Expected nonexistent helper path is present' }
    # Do not mutate the valid release. The explicit bad path must not fall back to its present helper.
    $arguments = @('--mode','Integrated','--backend','Software','--report',$report,'--helper',$missing,'--observer-gate',$gate,'--scenario','Embed','--passive-distribution','true')
    $launch = Invoke-Probe $hostFile $arguments $cwd $gate $helperFile $null (Join-Path $case 'new user dirs')
    Write-Report $launch (Join-Path $case 'launch.json')
    $Summary.negative = @{ pid = $launch.pid; exit_code = $launch.exit_code; directory = $case; helper = $missing; passed = $false }
    if (Test-Path -LiteralPath $report) {
        $probe = Get-Content -LiteralPath $report -Raw -Encoding UTF8 | ConvertFrom-Json
        $Summary.negative.passed = $launch.exit_code -eq 1 -and $launch.streams_complete -and -not $launch.timed_out -and
            -not $launch.live_images_verified -and $null -eq $launch.observer_error -and @($launch.children).Count -eq 0 -and
            @($launch.remaining_processes).Count -eq 0 -and -not [IO.File]::Exists([IO.Path]::ChangeExtension($gate,'ready')) -and
            $probe.pid -eq $launch.pid -and $probe.passive_distribution -and -not $probe.success -and -not $probe.finished -and
            $probe.created -eq 0 -and $probe.closed -eq 0 -and $probe.error -clike 'start: Deployment(MissingFile {*' -and
            $probe.error.Contains('deliberately missing worker.exe') -and -not [IO.Directory]::Exists((Join-Path $case 'cache'))
    }
    Write-Host "Distribution/MissingHelper pid=$($launch.pid) exit=$($launch.exit_code) passed=$($Summary.negative.passed)"
    $keys = @($Summary.cases | ForEach-Object { "$($_.mode)/$($_.backend)" })
    $Summary.success = $Summary.negative.passed -and ($keys -join ',') -ceq 'Integrated/Software,Integrated/D3d11,Threaded/Software,Threaded/D3d11'
}

$token = [guid]::NewGuid().ToString('N')
$run = Resolve-ValidationDir 'distribution' $token
$outside = Resolve-PocTempDir "component-release-$token"
$bundle = Join-Path $outside '发行 public bundle with spaces'
$profile = if ($Release) { 'release' } else { 'debug' }
$summary = @{ schema = 1; stage = 'DistributionEmbed'; bundle = $bundle; outside_root = $outside; expected_cases = 4; cases = @(); negative = $null;
    success = $false; error = $null; profile_restart = $false; menu_matrix = $false; desktop_pixels_observed = $false; physical_input = $false;
    cargo_profile = $profile; dev_artifacts = (-not $Release); clean_windows_account = $false; task_14_3_accepted = $false }
if ($ProfileRestart) { $summary.stage = 'DistributionProfileRestart'; $summary.expected_cases = 16; $summary.fixture = $null }
if ($PrintConfirmationProbe) {
    $summary.stage = if ($PrintNavigationProbe) { 'PrintNavigationRevocation' } else { 'PrintConfirmationPanel' }
    $summary.expected_cases = 1
    $summary.print_job_submitted = $false; $summary.task_12_1_accepted = $false
}
$oldCef = $env:CEF_PATH; $oldTarget = $env:CARGO_TARGET_DIR
New-Item -ItemType Directory -Force -Path $run | Out-Null
try {
    if (Test-WithinDirectory (Resolve-PocTempDir) $PocRoot) { throw 'Temporary staging must be outside the repository' }
    $cef = Resolve-CefPath $CefPath
    $env:CEF_PATH = $cef
    $env:CARGO_TARGET_DIR = Resolve-TargetDir $TargetDir
    $inputs = Join-Path $outside 'actual Cargo inputs'
    $prior = Join-Path $run 'prior-artifacts'
    New-Item -ItemType Directory -Force -Path $outside,$inputs,$prior | Out-Null
    $profileDir = Join-Path $env:CARGO_TARGET_DIR "$PocTarget/$profile"
    foreach ($name in @('component-host.exe','rust-cef-helper.exe')) {
        $old = Join-Path $profileDir $name
        if (Test-Path -LiteralPath $old -PathType Leaf) { Copy-Item -LiteralPath $old -Destination $prior }
    }
    Push-Location $PocRoot
    try {
        $helperArgs = @('build','--locked','--target',$PocTarget,'-p','rust-cef-helper','--bin','rust-cef-helper')
        $hostArgs = @('--locked','--manifest-path','validation/component-host/Cargo.toml','--target',$PocTarget,'--bin','component-host')
        if ($Release) {
            $helperBuild = Build-DistributionArtifact 'rust-cef-helper' ($helperArgs + '--release')
            $hostBuild = Build-DistributionArtifact 'component-host' (@('build') + $hostArgs + '--release')
            Confirm-DistributionReleaseArtifact $helperBuild 'rust-cef-helper' $profileDir
            Confirm-DistributionReleaseArtifact $hostBuild 'component-host' $profileDir
        } else {
            $helperBuild = Build-DistributionArtifact 'rust-cef-helper' $helperArgs
            # This host links GPUI; apply the known PDB workaround only to the dev host.
            $linkArgs = @('--','-C','link-arg=/DEBUG:NONE')
            if ($PrintConfirmationProbe) {
                $hostArgs += @('--features','validation')
            } else {
                $linkArgs += @('-C',('incremental=' + (Join-Path $outside 'host-incremental')))
            }
            $hostBuild = Build-DistributionArtifact 'component-host' (@('rustc') + $hostArgs) $linkArgs
        }
    } finally { Pop-Location }
    $hostArtifact = [IO.Path]::GetFullPath($hostBuild.executable)
    $helper = [IO.Path]::GetFullPath($helperBuild.executable)
    $hostName = 'component release viewer.exe'; $helperName = 'component release worker.exe'
    $hostInput = Join-Path $inputs $hostName; $helperInput = Join-Path $inputs $helperName
    Copy-Item -LiteralPath $hostArtifact -Destination $hostInput
    Copy-Item -LiteralPath $helper -Destination $helperInput
    $version = if ($Release) { 'public-distribution-release-restart' } elseif ($PrintConfirmationProbe) { 'public-print-confirmation-probe' } else { 'public-distribution-embed' }
    & (Join-Path $PSScriptRoot 'package-component.ps1') -HostExecutable $hostInput -HelperExecutable $helperInput -CefPath $cef -OutputDirectory $bundle -HostBuildVersion $version
    $hostFile = Join-Path $bundle $hostName; $helperFile = Join-Path $bundle $helperName
    $workspaceIdentity = [GPUI.Cef.Build.DirectoryIdentity]::Resolve($PocRoot)
    if (Test-WithinDirectory ([GPUI.Cef.Build.DirectoryIdentity]::Resolve($outside)) $workspaceIdentity) { throw 'Physical output aliases the repository' }
    $identity = @{ host_artifact = $hostArtifact; helper_artifact = $helper; host = $hostFile; helper = $helperFile;
        host_profile = $hostBuild.profile; helper_profile = $helperBuild.profile;
        host_sha256 = Get-PocFileSha256 $hostArtifact; helper_sha256 = Get-PocFileSha256 $helper }
    if ((Get-PocFileSha256 $hostFile) -cne $identity.host_sha256 -or (Get-PocFileSha256 $helperFile) -cne $identity.helper_sha256) {
        throw 'Staged executables differ from Cargo artifacts'
    }
    Write-Report $identity (Join-Path $run 'artifact-identity.json')

    if ($ProfileRestart) {
        Invoke-DistributionProfileRestart $summary $run $outside
        $summary.profile_restart = $true
        $summary.success = $true
    } elseif ($PrintConfirmationProbe) {
        Invoke-DistributionPrintPanel $summary $outside $PrintConfirmationMode
    } else {
        Invoke-DistributionColdEmbed $summary $outside
    }
} catch {
    $summary.error = $_.ToString()
    Write-Host "Distribution failed: $($summary.error)"
} finally {
    $env:CEF_PATH = $oldCef; $env:CARGO_TARGET_DIR = $oldTarget
    Write-Report $summary (Join-Path $run 'result.json')
    Write-Host "Evidence: $run"
}
if (-not $summary.success) { exit 1 }


