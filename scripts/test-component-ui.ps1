#Requires -Version 7.0
[CmdletBinding()]
param(
    [ValidateSet('Integrated','Threaded','All')][string]$Mode = 'All',
    [ValidateSet('Software','D3d11','All')][string]$Backend = 'All',
    [ValidateSet('Full','ProfileTiming','ProfileWindows','ProfileWindowPressure','ProfileWindowMenuPressure','ProfileWindowMenuFlushPressure','ProfileWindowSourcePressure','ProfileWindowFindPressure','ProfileWindowEventPressure','ProfileWindowPrintPressure','ProfileWindowPairTiming','ProfileWindowReadbackAdapter','ProfileWindowRendererAdapter','ProfileProductUi')][string]$Scope = 'Full',
    [string]$CefPath,
    [string]$TargetDir,
    [ValidateRange(30,900)][int]$TimeoutSeconds = 120
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ($Scope -eq 'Full') {
    # Physical-interaction, IME, menu/managed-services and OLE cells are all
    # guarded to one explicit cell each, so Full must orchestrate them one by
    # one and then run the passive matrices. Any failing phase fails the whole
    # scope, and the receipt keeps every phase and its evidence directory.
    . (Join-Path $PSScriptRoot 'common.ps1')
    $run = New-PocDirectory (Resolve-ValidationDir 'component-host' ([guid]::NewGuid().ToString('N')))
    $driver = Join-Path $PSScriptRoot 'test-component-host.ps1'
    $summary = @{ schema = 1; scope = 'Full'; success = $false; error = $null; phases = @(); cells = @() }
    function Invoke-FullPhase([string]$Name, [string[]]$Arguments) {
        $log = Join-Path $run "$Name.log"
        $ErrorActionPreference = 'Continue'
        try { $lines = @(& pwsh -NoProfile -File $driver @Arguments 2>&1 | Tee-Object -FilePath $log | ForEach-Object { Write-Host $_; $_ }); $code = $LASTEXITCODE }
        finally { $ErrorActionPreference = 'Stop' }
        $paths = @($lines | Where-Object { $_ -is [string] } | ForEach-Object { if ($_ -cmatch '^Evidence: (.+)$') { $Matches[1] } })
        $dir = if ($paths.Count -eq 1) { [IO.Path]::GetFullPath($paths[0]) } else { $null }
        $script:summary.phases = @($script:summary.phases) + @{ name = $Name; exit_code = $code; log = $log; evidence = $dir }
        if ($code -ne 0) {
            throw [pscustomobject]@{ Phase = $Name; Code = $code; Log = $log; Evidence = $dir }
        }
        return $dir
    }
    try {
        $common = @('-Mode','All','-Backend','All','-TimeoutSeconds',$TimeoutSeconds)
        if ($CefPath) { $common += @('-CefPath',$CefPath) }
        if ($TargetDir) { $common += @('-TargetDir',$TargetDir) }
        # Passive, no desktop input: safe on the All axis.
        # SourceViewer and FindView now carry the same approved foreground path as
        # the menu phase: their readiness gate needs the owner window active, while
        # their drivers still send no desktop input.
        foreach ($sc in @('SourceViewer','FindView')) {
            $attempt = 0
            while ($true) {
                $attempt++
                try {
                    Invoke-FullPhase $sc (@('-Scenario',$sc,'-Mode','All','-Backend','All','-MenuForegroundApproved','-TimeoutSeconds',$TimeoutSeconds)) | Out-Null
                    break
                } catch {
                    $failure = $_
                    $log = if ($failure.TargetObject -and $failure.TargetObject.Log) { $failure.TargetObject.Log } else { $null }
                    $haystack = @()
                    if ($failure.TargetObject -and $failure.TargetObject.Evidence) {
                        $haystack += @(Get-ChildItem -LiteralPath $failure.TargetObject.Evidence -Recurse -Filter 'probe.json' -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName })
                    }
                    if ($log) { $haystack += $log }
                    $environment = $false
                    foreach ($f in $haystack) {
                        if (Select-String -LiteralPath $f -Pattern 'approved foreground denied|approved foreground not confirmed|SetForegroundWindow denied|observer timeout|unexpected raw input|readiness timed out|observer released without validating|input sequence, position or key differs' -Quiet) { $environment = $true; break }
                    }
                    if (-not $environment -or $attempt -ge 12) { throw }
                    Write-Host "$sc attempt $attempt hit desktop interference; retrying with a fresh process"
                    Start-Sleep -Milliseconds 2000
                }
            }
        }
        # The menu phase is also subject to desktop interference, so it shares the
        # same bounded, cause-specific retry as the IME cells.
        $menuAttempt = 0
        while ($true) {
            $menuAttempt++
            try {
                Invoke-FullPhase 'menu-default' (@('-Scenario','MenuDefault','-Mode','Threaded','-Backend','Software','-InteractiveInputApproved','-MenuForegroundApproved','-TimeoutSeconds',$TimeoutSeconds) + $(if($CefPath){@('-CefPath',$CefPath)}else{@()}) + $(if($TargetDir){@('-TargetDir',$TargetDir)}else{@()})) | Out-Null
                break
            } catch {
                $failure = $_
                $log = if ($failure.TargetObject -and $failure.TargetObject.Log) { $failure.TargetObject.Log } else { $null }
                # The human log carries only the wrapper message; the real cause
                # lives in the per-cell probe.json, so search both.
                $haystack = @()
                if ($failure.TargetObject -and $failure.TargetObject.Evidence) {
                    $haystack += @(Get-ChildItem -LiteralPath $failure.TargetObject.Evidence -Recurse -Filter 'probe.json' -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName })
                }
                if ($log) { $haystack += $log }
                $foreign = $false
                foreach ($f in $haystack) {
                    if (Select-String -LiteralPath $f -Pattern 'observer released before MenuDefault stage|unexpected raw input|ordinary owned SetForegroundWindow denied|input sequence, position or key differs from armed stage|approved foreground denied|approved foreground not confirmed' -Quiet) { $foreign = $true; break }
                }
                if (-not $foreign -or $menuAttempt -ge 12) { throw }
                Write-Host "menu-default attempt $menuAttempt hit desktop interference; retrying with a fresh process"
                Start-Sleep -Milliseconds 2000
            }
        }
        # Approved window operations and IME/input cells, one explicit cell each.
        foreach ($m in @('Integrated','Threaded')) {
            foreach ($b in @('Software','D3d11')) {
                $visAttempt = 0
                while ($true) {
                    $visAttempt++
                    try {
                        Invoke-FullPhase "visibility-$m-$b" (@('-Scenario','Visibility','-Mode',$m,'-Backend',$b,'-VisibilityOperationsApproved','-TimeoutSeconds',$TimeoutSeconds)) | Out-Null
                        break
                    } catch {
                        $failure = $_
                        $log = if ($failure.TargetObject -and $failure.TargetObject.Log) { $failure.TargetObject.Log } else { $null }
                        $haystack = @()
                        if ($failure.TargetObject -and $failure.TargetObject.Evidence) {
                            $haystack += @(Get-ChildItem -LiteralPath $failure.TargetObject.Evidence -Recurse -Filter 'probe.json' -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName })
                        }
                        if ($log) { $haystack += $log }
                        $foreign = $false
                        foreach ($f in $haystack) {
                            if (Select-String -LiteralPath $f -Pattern 'ordinary owned SetForegroundWindow denied|observer timeout|unexpected raw input' -Quiet) { $foreign = $true; break }
                        }
                        if (-not $foreign -or $visAttempt -ge 12) { throw }
                        Write-Host "visibility-$m-$b attempt $visAttempt hit desktop interference; retrying with a fresh process"
                        Start-Sleep -Milliseconds 2000
                    }
                }
                # Each activation phase needs the desktop to settle before the next
                # one requests foreground again; without this the second request
                # is denied by the OS and the failure looks like a product fault.
                Start-Sleep -Milliseconds 3000
                # Desktop interference can inject foreign raw input into the armed
                # observation window. Retry only that distinguishable cause, keep
                # every attempt's log and evidence, and never relax the assertions.
                $imeAttempt = 0
                while ($true) {
                    $imeAttempt++
                    try {
                        Invoke-FullPhase "ime-$m-$b" (@('-Scenario','ImeObserve','-Mode',$m,'-Backend',$b,'-InteractiveInputApproved','-TimeoutSeconds',$TimeoutSeconds)) | Out-Null
                        break
                    } catch {
                        $failure = $_
                        $text = if ($failure.TargetObject -is [string]) { $failure.TargetObject } else { "$failure" }
                        $log = if ($failure.TargetObject -and $failure.TargetObject.Log) { $failure.TargetObject.Log } else { $null }
                        $haystack = @()
                        if ($failure.TargetObject -and $failure.TargetObject.Evidence) {
                            $haystack += @(Get-ChildItem -LiteralPath $failure.TargetObject.Evidence -Recurse -Filter 'probe.json' -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName })
                        }
                        if ($log) { $haystack += $log }
                        $foreign = $false
                        foreach ($f in $haystack) {
                            if (Select-String -LiteralPath $f -Pattern 'unexpected raw input|ordinary owned SetForegroundWindow denied|observer released before|input sequence, position or key differs from armed stage|approved foreground denied|approved foreground not confirmed' -Quiet) { $foreign = $true; break }
                        }
                        if (-not $foreign -or $imeAttempt -ge 12) { throw }
                        Write-Host "ime-$m-$b attempt $imeAttempt hit foreign raw input; retrying with a fresh process"
                        Start-Sleep -Milliseconds 2000
                    }
                }
                Start-Sleep -Milliseconds 3000
            }
        }
        # The passive product-UI matrix (multi-profile + both timing lanes).
        # ProfileProductUi lives on this script, not on the host driver, so the
        # final phase re-enters test-component-ui.ps1 with that scope.
        $productScript = $PSCommandPath
        $productLog = Join-Path $run 'product-ui.log'
        $ErrorActionPreference = 'Continue'
        try {
            $lines = @(& pwsh -NoProfile -File $productScript -Scope ProfileProductUi -Mode All -Backend All -TimeoutSeconds $TimeoutSeconds 2>&1 | Tee-Object -FilePath $productLog | ForEach-Object { Write-Host $_; $_ })
            $code = $LASTEXITCODE
        } finally { $ErrorActionPreference = 'Stop' }
        $paths = @($lines | Where-Object { $_ -is [string] } | ForEach-Object { if ($_ -cmatch '^Evidence: (.+)$') { $Matches[1] } })
        $dir = if ($paths.Count -eq 1) { [IO.Path]::GetFullPath($paths[0]) } else { $null }
        $script:summary.phases = @($script:summary.phases) + @{ name = 'product-ui'; exit_code = $code; log = $productLog; evidence = $dir }
        if ($code -ne 0) { throw "product-ui phase exited $code; see $productLog" }
        # Every required phase is now part of the matrix, so the scope may pass.
        $summary.success = $true
    } catch {
        $t = $_.TargetObject
        $summary.error = if ($t -is [string]) { $t } elseif ($t -and $t.Phase) { "$($t.Phase) phase exited $($t.Code)" } else { $_.Exception.Message }
        Write-Host "Full failed: $($summary.error)"
    } finally {
        [IO.File]::WriteAllText((Join-Path $run 'result.json'),($summary | ConvertTo-Json -Depth 12),[Text.UTF8Encoding]::new($false))
        Write-Host "Evidence: $run"
    }
    if (-not $summary.success) { exit 1 }
    exit 0
}
$adapterScope = $Scope -in @('ProfileWindowReadbackAdapter','ProfileWindowRendererAdapter')
$requiredBackend = if ($adapterScope) { 'D3d11' } else { 'All' }
if ($Mode -ne 'All' -or $Backend -ne $requiredBackend) { throw "$Scope requires Mode All and Backend $requiredBackend" }
if ($Scope -eq 'ProfileProductUi') {
    # Two real, passive GPUI phases in one public invocation: multi-profile
    # product UI (four cells) and per-window copy->paint timing (four cells).
    . (Join-Path $PSScriptRoot 'common.ps1')
    . (Join-Path $PSScriptRoot 'component-product-ui-receipt.ps1')
    $run = New-PocDirectory (Resolve-ValidationDir 'component-host' ([guid]::NewGuid().ToString('N')))
    $driver = Join-Path $PSScriptRoot 'test-component-host.ps1'
    function Invoke-Phase([string]$Name, [string[]]$Arguments) {
        # A failed phase still reports the evidence directory it preserved, so the
        # combined receipt never hides which cell stopped the matrix.
        $log = Join-Path $run "$Name.log"
        $ErrorActionPreference = 'Continue'
        try { $lines = @(& pwsh -NoProfile -File $driver @Arguments 2>&1 | Tee-Object -FilePath $log | ForEach-Object { Write-Host $_; $_ }); $code = $LASTEXITCODE }
        finally { $ErrorActionPreference = 'Stop' }
        $paths = @($lines | Where-Object { $_ -is [string] } | ForEach-Object { if ($_ -cmatch '^Evidence: (.+)$') { $Matches[1] } })
        if ($paths.Count -ne 1) { throw "$Name phase returned $($paths.Count) evidence paths" }
        $dir = [IO.Path]::GetFullPath($paths[0])
        $parent = Resolve-ValidationDir 'component-host'
        if ([IO.Path]::GetDirectoryName($dir) -ine [IO.Path]::GetFullPath($parent)) { throw "$Name phase returned an unexpected evidence path" }
        return [pscustomobject]@{ name = $Name; code = $code; log = $log; evidence = $dir }
    }
    function Add-Phase($Summary, $Phase) {
        $Summary.phases = @($Summary.phases) + @{ name = $Phase.name; exit_code = $Phase.code; log = $Phase.log; evidence = $Phase.evidence; bundle = $null }
        return $Summary
    }
    $summary = @{ schema = 1; scope = 'ProfileProductUi'; success = $false; error = $null; plan = @('ProfileDispatch','PaintLatency'); phases = @(); scale = $null; cells = @() }
    try {
        $common = @('-Mode','All','-Backend','All','-TimeoutSeconds',$TimeoutSeconds)
        if ($CefPath) { $common += @('-CefPath',$CefPath) }
        if ($TargetDir) { $common += @('-TargetDir',$TargetDir) }
        $dispatchPhase = Invoke-Phase 'dispatch' (@('-Scenario','ProfileDispatch') + $common)
        $summary = Add-Phase $summary $dispatchPhase
        if ($dispatchPhase.code -ne 0) { throw "dispatch phase exited $($dispatchPhase.code); see $($dispatchPhase.log)" }
        $timingPhase = Invoke-Phase 'timing' (@('-Scenario','PaintLatency','-TimingProfilePeer','-TimingProfileWindows','-TimingMeasureBoth','-TimingHostNoPdb','-StageBundleInTemp') + $common)
        $summary = Add-Phase $summary $timingPhase
        if ($timingPhase.code -ne 0) { throw "timing phase exited $($timingPhase.code); see $($timingPhase.log)" }
        $dispatch = Get-Content -LiteralPath (Join-Path $dispatchPhase.evidence 'result.json') -Raw -Encoding UTF8 | ConvertFrom-Json
        $timing = Get-Content -LiteralPath (Join-Path $timingPhase.evidence 'result.json') -Raw -Encoding UTF8 | ConvertFrom-Json
        $bundles = @{}
        foreach ($p in @(@{phase=$dispatchPhase; receipt=$dispatch}, @{phase=$timingPhase; receipt=$timing})) {
            # Staged-in-temp timing reports the exact bundle it used; the dispatch
            # phase keeps its bundle beside the per-cell evidence.
            $dirs = if ($p.receipt.PSObject.Properties.Name -contains 'bundle_path' -and $p.receipt.bundle_path) {
                @([IO.Path]::GetFullPath($p.receipt.bundle_path))
            } else {
                @(Get-ChildItem -LiteralPath $p.phase.evidence -Directory | Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'component-runtime.json') } | ForEach-Object { $_.FullName })
            }
            $dirs = @($dirs)
            # A single string would index to its first character, so normalise first.
            if ($dirs.Count -ne 1 -or -not (Test-Path -LiteralPath (Join-Path $dirs[0] 'component-runtime.json'))) { throw "$($p.phase.name) phase does not expose exactly one staged runtime manifest" }
            $manifest = Get-Content -LiteralPath (Join-Path $dirs[0] 'component-runtime.json') -Raw -Encoding UTF8 | ConvertFrom-Json
            if (-not (Test-ProductUiBundle $manifest)) { throw "$($p.phase.name) staged CEF/binding identity is not the pinned matrix" }
            $bundles[$p.phase.name] = $dirs[0]
        }
        foreach ($phase in $summary.phases) { $phase.bundle = $bundles[$phase.name] }
        $proof = Test-ProductUiReceipts $dispatch $timing
        if ($null -eq $proof) { throw 'Product UI receipts did not cover four passive cells, multi-profile and both timing lanes' }
        $summary.scale = $proof.scale
        $summary.cells = @($proof.cells)
        $summary.success = $true
    } catch {
        $summary.error = $_.Exception.Message
        Write-Host "ProfileProductUi failed: $($summary.error)"
    } finally {
        [IO.File]::WriteAllText((Join-Path $run 'result.json'),($summary | ConvertTo-Json -Depth 12),[Text.UTF8Encoding]::new($false))
        Write-Host "Evidence: $run"
    }
    if (-not $summary.success) { exit 1 }
    exit 0
}
$argsForHost = @('-Mode',$Mode,'-Backend',$Backend,'-Scenario','PaintLatency','-TimingProfilePeer','-StageBundleInTemp','-TimingHostNoPdb','-TimeoutSeconds',$TimeoutSeconds)
if ($Scope -eq 'ProfileWindowPairTiming' -or $adapterScope) {
    $argsForHost += @('-TimingProfileWindows','-TimingMeasureBoth')
    if ($adapterScope) { $argsForHost += '-TimingReadbackAdapter' }
    if ($Scope -eq 'ProfileWindowRendererAdapter') { $argsForHost += '-TimingRendererAdapter' }
} else {
    $scopes=@('ProfileTiming','ProfileWindows','ProfileWindowPressure','ProfileWindowMenuPressure','ProfileWindowMenuFlushPressure','ProfileWindowSourcePressure','ProfileWindowFindPressure','ProfileWindowEventPressure','ProfileWindowPrintPressure')
    $pressureFlags=@('-TimingProfileWindows','-TimingWindowPressure','-TimingWindowMenuPressure','-TimingWindowMenuFlushPressure','-TimingWindowSourcePressure','-TimingWindowFindPressure','-TimingWindowEventPressure','-TimingWindowPrintPressure')
    $rank=-1
    for ($i=0; $i -lt $scopes.Count; $i++) { if ($scopes[$i] -ieq $Scope) { $rank=$i; break } }
    if ($rank -lt 0) { throw 'Unknown passive validation scope' }
    if ($rank -gt 0) { $argsForHost += $pressureFlags[0..($rank-1)] }
}
if ($CefPath) { $argsForHost += @('-CefPath',$CefPath) }
if ($TargetDir) { $argsForHost += @('-TargetDir',$TargetDir) }
& pwsh -NoProfile -File (Join-Path $PSScriptRoot 'test-component-host.ps1') @argsForHost
exit $LASTEXITCODE
