#Requires -Version 7.0
# Standalone, passive root consumer smoke. Do not switch the legacy demo runner.
[CmdletBinding()]
param([string]$CefPath, [string]$TargetDir, [ValidateRange(45,180)][int]$TimeoutSeconds = 90, [switch]$Release, [switch]$ReleasePathGuards, [switch]$MenuCapabilities, [switch]$MenuProbe, [int]$MenuTarget = 0)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($ReleasePathGuards -and -not $Release) { throw 'ReleasePathGuards requires actual Cargo -Release artifacts' }
. (Join-Path $PSScriptRoot 'common.ps1')
. (Join-Path $PSScriptRoot 'component-process-observer.ps1')
. (Join-Path $PSScriptRoot 'component-probe-launcher.ps1')
function Initialize-DemoInput {
    if (-not ('DemoInput.U' -as [type])) {
        Add-Type -Namespace DemoInput -Name U -MemberDefinition @'
public delegate bool EnumWindowsProc(System.IntPtr hWnd, System.IntPtr lParam);
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, System.IntPtr lp);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(System.IntPtr h, out uint pid);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(System.IntPtr h, System.Text.StringBuilder s, int max);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(System.IntPtr h, System.Text.StringBuilder s, int max);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(System.IntPtr h);
[DllImport("user32.dll")] public static extern System.IntPtr GetWindow(System.IntPtr h, uint cmd);
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(System.IntPtr h);
[DllImport("user32.dll")] public static extern System.IntPtr GetForegroundWindow();
[DllImport("user32.dll")] public static extern bool GetWindowRect(System.IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool GetClientRect(System.IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool ClientToScreen(System.IntPtr h, ref POINT p);
[DllImport("user32.dll")] public static extern bool SetCursorPos(int X, int Y);
[DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
[DllImport("user32.dll")] public static extern System.IntPtr WindowFromPoint(POINT p);
[DllImport("user32.dll")] public static extern System.IntPtr GetAncestor(System.IntPtr h, uint flags);
[DllImport("user32.dll")] public static extern short GetAsyncKeyState(int vKey);
[DllImport("user32.dll", SetLastError=true)] public static extern uint SendInput(uint n, INPUT[] i, int cb);
[DllImport("user32.dll")] public static extern System.IntPtr SetThreadDpiAwarenessContext(System.IntPtr ctx);
[DllImport("user32.dll")] public static extern uint GetDpiForWindow(System.IntPtr h);
[DllImport("user32.dll")] public static extern System.IntPtr GetThreadDesktop(uint tid);
[DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
[DllImport("user32.dll", SetLastError=true)] public static extern bool GetUserObjectInformationW(System.IntPtr h, int idx, out bool val, int len, out int needed);
public struct RECT { public int L,T,R,B; }
public struct POINT { public int X,Y; }
[StructLayout(LayoutKind.Sequential)] public struct MOUSEINPUT { public int dx,dy; public uint mouseData,flags,time; public System.IntPtr extra; }
[StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public MOUSEINPUT mi; }
public static uint SendMouse(int dx, int dy, uint flags) {
    INPUT[] inp = new INPUT[1];
    inp[0].type = 0;
    inp[0].mi.dx = dx;
    inp[0].mi.dy = dy;
    inp[0].mi.flags = flags;
    return SendInput(1, inp, System.Runtime.InteropServices.Marshal.SizeOf(typeof(INPUT)));
}
'@
    }
    # Per-Monitor-V2 DPI awareness so GetClientRect/ClientToScreen/SetCursorPos use physical pixels.
    [void][DemoInput.U]::SetThreadDpiAwarenessContext([IntPtr](-4))
}

function Find-OwnedDemoWindow([int]$OwnedPid, [string]$ExpectedTitle) {
    Initialize-DemoInput
    $targetPid = [uint32]$OwnedPid
    $found = [System.Collections.Generic.List[int64]]::new()
    $cb = [DemoInput.U+EnumWindowsProc]{
        param($h, $lp)
        [uint32]$wp = 0
        [void][DemoInput.U]::GetWindowThreadProcessId($h, [ref]$wp)
        if ($wp -eq $targetPid -and [DemoInput.U]::IsWindowVisible($h) -and [DemoInput.U]::GetWindow($h, 4) -eq [IntPtr]::Zero) {
            $tb = New-Object System.Text.StringBuilder 256
            [void][DemoInput.U]::GetWindowTextW($h, $tb, 256)
            if ($tb.ToString() -ceq $ExpectedTitle) {
                $found.Add($h.ToInt64())
            }
        }
        return $true
    }
    [void][DemoInput.U]::EnumWindows($cb, [IntPtr]::Zero)
    if ($found.Count -eq 1) { return $found[0] }
    return 0L
}

function Test-DemoInputDesktop {
    Initialize-DemoInput
    $desk = [DemoInput.U]::GetThreadDesktop([DemoInput.U]::GetCurrentThreadId())
    $io = $false; $needed = 0
    if ($desk -eq [IntPtr]::Zero -or -not [DemoInput.U]::GetUserObjectInformationW($desk, 6, [ref]$io, 4, [ref]$needed) -or -not $io) {
        throw 'current desktop does not receive input (UOI_IO=False; session locked or disconnected)'
    }
}

function Invoke-OwnedLeftClickAt([int64]$Hwnd, [int]$ClientX, [int]$ClientY) {
    Test-DemoInputDesktop
    $h = [IntPtr]$Hwnd
    $pt = New-Object DemoInput.U+POINT; $pt.X = $ClientX; $pt.Y = $ClientY
    [void][DemoInput.U]::ClientToScreen($h, [ref]$pt)
    [void][DemoInput.U]::SetCursorPos($pt.X, $pt.Y)
    $s1 = [DemoInput.U]::SendMouse(1, 0, 0x0001); Start-Sleep -Milliseconds 60
    $s2 = [DemoInput.U]::SendMouse(-1, 0, 0x0001); Start-Sleep -Milliseconds 120
    $s3 = [DemoInput.U]::SendMouse(0, 0, 0x0002); Start-Sleep -Milliseconds 60
    $s4 = [DemoInput.U]::SendMouse(0, 0, 0x0004)
    $cur = New-Object DemoInput.U+POINT
    [void][DemoInput.U]::GetCursorPos([ref]$cur)
    return @{ client_x = $ClientX; client_y = $ClientY; screen_x = $pt.X; screen_y = $pt.Y; cursor_x = $cur.X; cursor_y = $cur.Y; sent = @($s1, $s2, $s3, $s4) }
}

function Invoke-OwnedRightClick([int64]$Hwnd, [bool]$Prime = $false, [double]$ClientDipX = 60.0, [double]$ClientDipY = 40.0) {
    Test-DemoInputDesktop
    $h = [IntPtr]$Hwnd
    $fg = [DemoInput.U]::SetForegroundWindow($h)
    Start-Sleep -Milliseconds 350
    $isFg = ([DemoInput.U]::GetForegroundWindow() -eq $h)
    $dpi = [DemoInput.U]::GetDpiForWindow($h)
    $scale = if ($dpi -gt 0) { [double]$dpi / 96.0 } else { 1.0 }
    $wrect = New-Object DemoInput.U+RECT
    [void][DemoInput.U]::GetWindowRect($h, [ref]$wrect)
    $rect = New-Object DemoInput.U+RECT
    [void][DemoInput.U]::GetClientRect($h, [ref]$rect)
    $clientX = [int][Math]::Floor($ClientDipX * $scale + 0.5)
    $clientY = [int][Math]::Floor($ClientDipY * $scale + 0.5)
    $pt = New-Object DemoInput.U+POINT
    $pt.X = $clientX; $pt.Y = $clientY
    [void][DemoInput.U]::ClientToScreen($h, [ref]$pt)
    $ptHwnd = [DemoInput.U]::WindowFromPoint($pt)
    $ptRoot = if ($ptHwnd -ne [IntPtr]::Zero) { [DemoInput.U]::GetAncestor($ptHwnd, 2) } else { [IntPtr]::Zero }
    [uint32]$ptPid = 0
    if ($ptRoot -ne [IntPtr]::Zero) { [void][DemoInput.U]::GetWindowThreadProcessId($ptRoot, [ref]$ptPid) }
    $ptCls = New-Object System.Text.StringBuilder 128
    $ptTit = New-Object System.Text.StringBuilder 256
    if ($ptRoot -ne [IntPtr]::Zero) {
        [void][DemoInput.U]::GetClassNameW($ptRoot, $ptCls, 128)
        [void][DemoInput.U]::GetWindowTextW($ptRoot, $ptTit, 256)
    }
    [void][DemoInput.U]::SetCursorPos($pt.X, $pt.Y)
    # Win32 MOUSEEVENTF_* constants: MOVE=0x0001, LEFTDOWN=0x0002, LEFTUP=0x0004,
    # RIGHTDOWN=0x0008, RIGHTUP=0x0010. Never use MOVE|ABSOLUTE with dx=dy=0.
    # A real relative MOUSEMOVE first: GPUI hit-tests on hover, and a button
    # press with no preceding move has no hover target to dispatch to.
    $s1 = [DemoInput.U]::SendMouse(1, 0, 0x0001)
    Start-Sleep -Milliseconds 60
    $s2 = [DemoInput.U]::SendMouse(-1, 0, 0x0001)
    Start-Sleep -Milliseconds 120
    if ($Prime) {
        [void][DemoInput.U]::SendMouse(0, 0, 0x0002)
        Start-Sleep -Milliseconds 80
        [void][DemoInput.U]::SendMouse(0, 0, 0x0004)
        Start-Sleep -Milliseconds 300
    }
    $s3 = [DemoInput.U]::SendMouse(0, 0, 0x0008)
    Start-Sleep -Milliseconds 60
    $rDown = (([DemoInput.U]::GetAsyncKeyState(2) -band 0x8000) -ne 0)
    $s4 = [DemoInput.U]::SendMouse(0, 0, 0x0010)
    Start-Sleep -Milliseconds 60
    $rUp = (([DemoInput.U]::GetAsyncKeyState(2) -band 0x8000) -ne 0)
    $cur = New-Object DemoInput.U+POINT
    [void][DemoInput.U]::GetCursorPos([ref]$cur)
    $still = ([DemoInput.U]::GetForegroundWindow() -eq $h)
    return @{
        dpi=$dpi; scale=$scale; cursor_x=$cur.X; cursor_y=$cur.Y;
        is_foreground_after_click=$still; foreground_request=$fg; is_foreground=$isFg;
        client_x=$clientX; client_y=$clientY; screen_x=$pt.X; screen_y=$pt.Y;
        win_rect=@($wrect.L,$wrect.T,$wrect.R,$wrect.B); client_rect=@($rect.L,$rect.T,$rect.R,$rect.B);
        point_hwnd=$ptHwnd.ToInt64(); point_root=$ptRoot.ToInt64(); point_matches_owner=($ptRoot -eq $h);
        point_pid=$ptPid; point_class=$ptCls.ToString(); point_title=$ptTit.ToString();
        sent=@($s1,$s2,$s3,$s4); rbutton_down_async=$rDown; rbutton_up_async=$rUp
    }
}
if ($ReleasePathGuards) { . (Join-Path $PSScriptRoot 'component-demo-release-paths.ps1') }

function Write-Report($Value, [string]$File) {
    [IO.File]::WriteAllText($File, ($Value | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
}

function Build-SmokeExecutable([string]$Name, [string[]]$BuildArguments, [string[]]$Link = @()) {
    $lines = @(& cargo @BuildArguments --message-format=json-render-diagnostics @Link 2>&1 |
        Tee-Object -FilePath (Join-Path $run "$Name-build.log") | ForEach-Object { Write-Host $_; $_ })
    if ($LASTEXITCODE -ne 0) { throw "Build $Name failed: $LASTEXITCODE" }
    $actual = @($lines | Where-Object { $_ -is [string] -and $_.StartsWith('{') } | ForEach-Object { $_ | ConvertFrom-Json } |
        Where-Object { $_.reason -eq 'compiler-artifact' -and $_.target.name -eq $Name -and $null -ne $_.executable })
    if ($actual.Count -ne 1) { throw "Expected a single actual executable for $Name" }
    $artifact = $actual[0]
    if ($Release -and ($artifact.profile.opt_level -cne '3' -or $artifact.profile.debug_assertions -isnot [bool] -or $artifact.profile.debug_assertions -or $artifact.profile.test -isnot [bool] -or $artifact.profile.test)) {
        throw "Unexpected release Cargo profile for $Name"
    }
    return $artifact
}

function Read-DemoWitness($Launch, [string]$Mode, [string]$Backend) {
    $paint = [regex]::Matches($Launch.stdout,'(?m)^COMPONENT_DEMO_PAINT runtime=(?<runtime>[1-9]\d*) browser=(?<browser>[1-9]\d*) width=(?<width>[1-9]\d*) height=(?<height>[1-9]\d*) generation=(?<generation>[1-9]\d*) sequence=(?<sequence>[1-9]\d*) backend=(?<backend>\w+)\r?$')
    $stop = [regex]::Matches($Launch.stdout,'(?m)^COMPONENT_DEMO_STOP mode=(?<mode>\w+) backend=Some\((?<backend>\w+)\) main=(?<main>[1-9]\d*) ui=(?<ui>[1-9]\d*) shutdown_thread=(?<shutdown_thread>[1-9]\d*) pump=(?<pump>\d+) created=1 closed=1 tasks=(?<tasks>[1-9]\d*) images=(?<images>[1-9]\d*) final_cpu=0 shutdown=1 native_after=0\r?$')
    if ($paint.Count -ne 1 -or $stop.Count -ne 1) { return $null }
    $p = $paint[0]; $s = $stop[0]; $expected = "${Backend}Required"
    if ($p.Index -ge $s.Index -or $p.Groups['backend'].Value -cne $expected -or $s.Groups['backend'].Value -cne $expected -or
        $s.Groups['mode'].Value -cne $Mode -or $s.Groups['main'].Value -ne $s.Groups['shutdown_thread'].Value) { return $null }
    $pump = [long]$s.Groups['pump'].Value
    $sameThread = $s.Groups['main'].Value -ceq $s.Groups['ui'].Value
    if (($Mode -ceq 'Integrated' -and (-not $sameThread -or $pump -le 0)) -or ($Mode -ceq 'Threaded' -and ($sameThread -or $pump -ne 0))) { return $null }
    return @{ dimensions=@([int]$p.Groups['width'].Value,[int]$p.Groups['height'].Value); generation=[long]$p.Groups['generation'].Value;
        sequence=[long]$p.Groups['sequence'].Value; pump=$pump; tasks=[long]$s.Groups['tasks'].Value; images=[long]$s.Groups['images'].Value }
}

$token = [guid]::NewGuid().ToString('N')
$run = New-PocDirectory (Resolve-ValidationDir 'component-demo' $token)
$outside = Resolve-PocTempDir "public-component-demo-$token"
$bundle = Join-Path $outside '发行 component demo with spaces'
$profile = if ($Release) { 'release' } else { 'dev' }
$summary = @{ schema=1; stage='RootComponentDemo'; cases=@(); expected_cases=4; success=$false; error=$null;
    bundle=$bundle; outside_root=$outside; cargo_profile=$profile; dev_artifacts=(-not $Release); physical_input=$false; desktop_pixels=$false;
    clean_windows_account=$false; menu_matrix=$false; legacy_default_changed=$false; task_14_1_accepted=$false; task_14_3_accepted=$false }
$modes=@('Integrated','Threaded'); $backends=@('Software','D3d11')
if ($ReleasePathGuards) {
    $modes=@('Threaded'); $backends=@('D3d11')
    $summary.stage='RootComponentDemoReleasePaths'; $summary.expected_cases=1
    $summary.expected_path_refusals=4; $summary.path_refusals=@(); $summary.resource_mutation=$null
}
$oldCef = $env:CEF_PATH; $oldTarget = $env:CARGO_TARGET_DIR
New-Item -ItemType Directory -Force -Path $run,$outside | Out-Null
try {
    $cef = Resolve-CefPath $CefPath
    $env:CEF_PATH = $cef; $env:CARGO_TARGET_DIR = Resolve-TargetDir $TargetDir
    $cargoOptions = @('--locked','--target',$PocTarget)
    if ($Release) { $cargoOptions += '--release' }
    Push-Location $PocRoot
    try {
        $helper = Build-SmokeExecutable 'rust-cef-helper' (@('build') + $cargoOptions + @('-p','rust-cef-helper','--bin','rust-cef-helper'))
        $demoArgs = $cargoOptions + @('-p','gpui-cef-poc','--features','component-demo','--bin','gpui-cef-component-demo')
        if ($Release) {
            $demo = Build-SmokeExecutable 'gpui-cef-component-demo' (@('build') + $demoArgs)
        } else {
            # The previous PDB workaround is scoped to the dev smoke.
            $demo = Build-SmokeExecutable 'gpui-cef-component-demo' (@('rustc') + $demoArgs) @('--','-C','link-arg=/DEBUG:NONE','-C',('incremental='+(Join-Path $outside 'host-incremental')))
        }
    } finally { Pop-Location }
    $hostArtifact = $demo.executable; $helperArtifact = $helper.executable
    $version = if ($Release) { 'root-public-demo-release-smoke' } else { 'root-public-demo-smoke' }
    $hostInput=$hostArtifact; $helperInput=$helperArtifact
    if ($ReleasePathGuards) {
        $inputs=Join-Path $outside 'renamed Cargo inputs'
        New-Item -ItemType Directory -Path $inputs | Out-Null
        $hostInput=Join-Path $inputs 'independent viewer.exe'; $helperInput=Join-Path $inputs 'independent worker.exe'
        [IO.File]::Copy($hostArtifact,$hostInput,$false)
        [IO.File]::Copy($helperArtifact,$helperInput,$false)
        $version='root-public-demo-release-paths'
    }
    & (Join-Path $PSScriptRoot 'package-component.ps1') -HostExecutable $hostInput -HelperExecutable $helperInput -CefPath $cef -OutputDirectory $bundle -HostBuildVersion $version
    $hostFile = Join-Path $bundle ([IO.Path]::GetFileName($hostInput)); $helperFile = Join-Path $bundle ([IO.Path]::GetFileName($helperInput))
    $identity = @{ host_artifact=$hostArtifact; helper_artifact=$helperArtifact; host_profile=$demo.profile; helper_profile=$helper.profile;
        host_sha256=(Get-FileHash -LiteralPath $hostFile).Hash; helper_sha256=(Get-FileHash -LiteralPath $helperFile).Hash }
    if ($identity.host_sha256 -cne (Get-FileHash -LiteralPath $hostArtifact).Hash -or $identity.helper_sha256 -cne (Get-FileHash -LiteralPath $helperArtifact).Hash) { throw 'Staged demo differs from actual Cargo outputs' }
    Write-Report $identity (Join-Path $run 'artifact-identity.json')
    foreach ($mode in $modes) { foreach ($backend in $backends) {
        $case = Join-Path $outside "$mode-$backend"; $gate = Join-Path $case 'observer.release'; $cwd = Join-Path $case 'unrelated cwd'
        New-Item -ItemType Directory -Path $case,$cwd | Out-Null
        $identityPath = Join-Path $case 'window.json'
        $observe = {
            param([int]$OwnedPid)
            $p = Get-Process -Id $OwnedPid -ErrorAction Stop
            try {
                $hwnd = 0L
                $qualDeadline = [DateTime]::UtcNow.AddSeconds(3)
                while ([DateTime]::UtcNow -lt $qualDeadline) {
                    $p.Refresh()
                    $hwnd = Find-OwnedDemoWindow $OwnedPid 'GPUI-CEF component demo'
                    if ($p.Path -ieq $hostFile -and $hwnd -gt 0) { break }
                    Start-Sleep -Milliseconds 100
                }
                if ($p.Path -ine $hostFile -or $hwnd -le 0) {
                    throw "Original component demo window not qualified (path=$($p.Path) hwnd=$hwnd main_hwnd=$($p.MainWindowHandle.ToInt64()) main_title=$($p.MainWindowTitle))"
                }
                Write-Report @{ pid=$OwnedPid; hwnd=$hwnd; image=$p.Path; creation_filetime=$p.StartTime.ToUniversalTime().ToFileTimeUtc() } $identityPath
                if ($MenuProbe) {
                    # Wait until a frame has been consumed: a press that arrives
                    # before the browser view is live cannot be routed to it.
                    $receipt = Join-Path $case 'menu-receipt.json'
                    $j = $null
                    $deadline = [DateTime]::UtcNow.AddSeconds(25)
                    while ([DateTime]::UtcNow -lt $deadline) {
                        if (Test-Path -LiteralPath $receipt) {
                            try {
                                $j = [IO.File]::ReadAllText($receipt) | ConvertFrom-Json
                                if ([int]$j.consumed_updates -gt 0 -and [int]$j.paint_calls -gt 0 -and $j.bridge_present -eq $true) { break }
                            } catch { }
                        }
                        Start-Sleep -Milliseconds 100
                    }
                    $ready = $null -ne $j -and [int]$j.consumed_updates -gt 0 -and [int]$j.paint_calls -gt 0
                    # Right-click inside the editable #e box (0..280, 0..100 DIP).
                    $click = Invoke-OwnedRightClick $hwnd $false 60.0 40.0
                    # The menu opens asynchronously; wait for the host to publish rows with bounds.
                    $opened = $null
                    $lastOpenPoll = $null
                    $openPollError = $null
                    $deadline = [DateTime]::UtcNow.AddSeconds(15)
                    while ([DateTime]::UtcNow -lt $deadline) {
                        try {
                            $cand = [IO.File]::ReadAllText($receipt) | ConvertFrom-Json
                            $lastOpenPoll = $cand
                            if ($cand.open -eq $true -and @($cand.rows).Count -gt 0 -and (@($cand.rows | Where-Object { $_.bounds -eq $true }).Count -gt 0)) {
                                $opened = $cand
                                break
                            }
                        } catch {
                            $openPollError = $_.Exception.Message
                        }
                        Start-Sleep -Milliseconds 100
                    }
                    $targetDip = $null
                    $target = $null
                    $click2 = $null
                    if ($null -ne $opened -and $MenuTarget -gt 0) {
                        $row = @($opened.rows | Where-Object { $_.id -eq $MenuTarget -and $_.bounds -eq $true }) | Select-Object -First 1
                        if ($null -ne $row -and $row.rect) {
                            $parts = ($row.rect -split ',') | ForEach-Object { [double]$_ }
                            $scale = if ($opened.scale -and [double]$opened.scale -gt 0) { [double]$opened.scale } else { [double]$click.scale }
                            $dipX = $parts[0] + $parts[2] / 2.0
                            $dipY = $parts[1] + $parts[3] / 2.0
                            $targetDip = @{ x = $dipX; y = $dipY; scale = $scale }
                            $target = @{ client_x = [int][Math]::Floor($dipX * $scale + 0.5); client_y = [int][Math]::Floor($dipY * $scale + 0.5) }
                            $click2 = Invoke-OwnedLeftClickAt $hwnd $target.client_x $target.client_y
                        }
                    }
                    # Poll until the presenter records the reply and closes the menu.
                    $final = $null
                    $replyDeadline = [DateTime]::UtcNow.AddSeconds(8)
                    while ([DateTime]::UtcNow -lt $replyDeadline) {
                        try {
                            $cand = [IO.File]::ReadAllText($receipt) | ConvertFrom-Json
                            $final = $cand
                            if ($MenuTarget -le 0 -or ($cand.open -eq $false -and $cand.reply -and $cand.reply -ne 'none')) { break }
                        } catch { }
                        Start-Sleep -Milliseconds 100
                    }
                    $served = @{
                        ready = $ready
                        opened = ($null -ne $opened)
                        rows_count = if ($null -ne $opened) { @($opened.rows).Count } else { 0 }
                        row_ids = if ($null -ne $opened) { @($opened.rows | ForEach-Object { $_.id }) } else { @() }
                        target_id = $MenuTarget
                        target_dip = $targetDip
                        target_point = $target
                        right_click = $click
                        left_click = $click2
                        reply = if ($final) { $final.reply } else { '' }
                        native = if ($final) { $final.native } else { $null }
                        service = if ($final) { $final.service } else { $null }
                        last_open_poll = $lastOpenPoll
                        open_poll_error = $openPollError
                        final_receipt = $final
                    }
                    Write-Report $served (Join-Path $case 'menu-served.json')
                }
            } finally { $p.Dispose() }
        }
        $smokeMs = if ($MenuProbe) { '1500' } else { '400' }
        $argsForHost = @('--mode',$mode,'--backend',$backend,'--bundle',$bundle,'--helper',$helperFile,'--cache-root',(Join-Path $case 'cache'),'--smoke-ms',$smokeMs,'--observer-gate',$gate)
        if ($MenuCapabilities) {
            # Host-managed menu capability demo: CEF's windowless model has no
            # DevTools, checkable or submenu items, so they exist only when the
            # delivered host opts in.
            $argsForHost += @('--context-menu','true','--menu-structure','true','--menu-devtools','true')
        }
        $receiptPath = Join-Path $case 'menu-receipt.json'
        if ($MenuProbe) {
            # The edit menu only exists on an editable page: the delivered demo's
            # showcase page has no editable element at all.
            $argsForHost += @('--context-menu','true','--menu-structure','true','--menu-devtools','true','--page','menu','--menu-receipt',$receiptPath)
        }
        $launch = Invoke-Probe $hostFile $argsForHost $cwd $gate $helperFile $observe (Join-Path $case 'clean user dirs')
        Write-Report $launch (Join-Path $case 'launch.json')
        $witness = Read-DemoWitness $launch $mode $backend
        $servedPath = Join-Path $case 'menu-served.json'
        $servedResult = if ($MenuProbe -and (Test-Path -LiteralPath $servedPath)) { [IO.File]::ReadAllText($servedPath) | ConvertFrom-Json } else { $null }
        $menuOk = if (-not $MenuProbe) { $true } else {
            $null -ne $servedResult -and $servedResult.ready -eq $true -and $servedResult.opened -eq $true -and
            ($MenuTarget -le 0 -or $servedResult.reply -ceq "Select { command_id: $MenuTarget }") -and
            ($null -eq $servedResult.native -or $servedResult.native.failed -ne $true)
        }
        $passed = $null -ne $witness -and $menuOk -and (Test-Path -LiteralPath $identityPath) -and $launch.exit_code -eq 0 -and
            $launch.streams_complete -and -not $launch.timed_out -and $launch.live_images_verified -and $null -eq $launch.observer_error -and @($launch.remaining_processes).Count -eq 0
        $summary.cases += @{ mode=$mode; backend=$backend; pid=$launch.pid; exit_code=$launch.exit_code; passed=$passed; directory=$case; witness=$witness; menu_served=$servedResult }
        Write-Host "RootComponentDemo/$mode/$backend pid=$($launch.pid) exit=$($launch.exit_code) passed=$passed"
        if (-not $passed) { throw 'Root component demo case failed; preserve first actual result' }
    }}
    if ($ReleasePathGuards) {
        Invoke-DemoReleasePathGuards $summary $outside $bundle $hostFile $helperFile $hostInput $helperInput $cef
        if ($summary.path_refusals.Count -ne $summary.expected_path_refusals) { throw 'Incomplete release path refusal results' }
    }
    $summary.success = ($summary.cases.Count -eq $summary.expected_cases)
} catch {
    $summary.error = $_.ToString()
    Write-Host "Root component demo failed: $($summary.error)"
} finally {
    $env:CEF_PATH = $oldCef; $env:CARGO_TARGET_DIR = $oldTarget
    Write-Report $summary (Join-Path $run 'result.json')
    Write-Host "Evidence: $run"
}
if (-not $summary.success) { exit 1 }
