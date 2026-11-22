# Helpers for the component validation entry point; no CEF work on the Core path.
function New-ComponentStageDirectory([string]$Stage) {
    $directory = New-PocDirectory (Resolve-ValidationDir $Stage ([guid]::NewGuid().ToString('N')))
    $directory
}

function Assert-ComponentKeys([string[]]$Actual, [string[]]$Expected, [string]$Label) {
    if ($Expected.Count -eq 0 -or $Actual.Count -ne $Expected.Count -or
        (($Actual | Sort-Object -CaseSensitive) -join "`n") -cne (($Expected | Sort-Object -CaseSensitive) -join "`n")) {
        throw "$Label did not cover the exact $($Expected.Count)-case plan"
    }
}

function Invoke-ComponentCore([string]$TargetDirectory, [int]$Timeout) {
    $run = New-ComponentStageDirectory 'Core'
    $summary = @{ schema = 1; stage = 'Core'; cases = @(); tests_passed = 0; error = $null; success = $false }
    $isWindowsHost = [Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT
    try {
        $arguments = @('test', '--locked', '-p', 'rust-cef-core', '--lib', '--tests', '--no-run', '--message-format=json-render-diagnostics')
        if ($isWindowsHost) { $arguments += @('--target', $PocTarget) }
        if ($TargetDirectory -or $isWindowsHost) { $arguments += @('--target-dir', (Resolve-TargetDir $TargetDirectory)) }
        Push-Location $PocRoot
        try {
            $metadata = @(& cargo metadata --locked --no-deps --format-version=1)
            if ($LASTEXITCODE -ne 0) { throw 'Core metadata query failed' }
            [IO.File]::WriteAllText((Join-Path $run 'cargo-metadata.json'), ($metadata -join "`n"))
            $packages = @(($metadata -join "`n" | ConvertFrom-Json).packages | Where-Object { $_.name -ceq 'rust-cef-core' })
            if ($packages.Count -ne 1 -or @($packages[0].dependencies).Count -ne 0) {
                throw 'Expected one dependency-free Core package'
            }
            $package = $packages[0]
            $summary.artifact_plan = @(foreach ($target in $package.targets) {
                if ($target.test -and ($target.kind -contains 'lib' -or $target.kind -contains 'test')) {
                    "$($target.kind -join '+')/$($target.name)"
                }
            })
            $ErrorActionPreference = 'Continue'
            $lines = @(& cargo @arguments 2>&1 | Tee-Object -FilePath (Join-Path $run 'build.log') | ForEach-Object { Write-Host $_; $_ })
            $exitCode = $LASTEXITCODE
        } finally { $ErrorActionPreference = 'Stop'; Pop-Location }
        if ($exitCode -ne 0) { throw "Core test build failed: $exitCode" }
        $artifacts = @(foreach ($line in $lines) {
            if ($line -isnot [string] -or -not $line.StartsWith('{')) { continue }
            $message = $line | ConvertFrom-Json
            if ($message.reason -eq 'compiler-artifact' -and $message.profile.test -and $message.executable) {
                if ($message.package_id -cne $package.id) { throw 'Non-Core test artifact returned by the Core build' }
                $message
            }
        })
        $actualKeys = @($artifacts | ForEach-Object { "$($_.target.kind -join '+')/$($_.target.name)" })
        Assert-ComponentKeys $actualKeys $summary.artifact_plan 'Core Cargo artifacts'
        $unique = @($artifacts | ForEach-Object { $_.executable } | Sort-Object -Unique)
        if ($unique.Count -ne $artifacts.Count) { throw 'Duplicate Core executable artifact' }
        $summary.artifacts = $unique.Count
        for ($i = 0; $i -lt $artifacts.Count; $i++) {
            $artifact = $artifacts[$i]
            $caseDir = Join-Path $run ('harness-' + $i)
            $cwd = Join-Path $caseDir 'unrelated cwd'
            New-Item -ItemType Directory -Path $cwd -Force | Out-Null
            $output = Join-Path $caseDir 'stdout.log'
            $errors = Join-Path $caseDir 'stderr.log'
            $path = [IO.Path]::GetFullPath($artifact.executable)
            $staged = Join-Path $caseDir ([IO.Path]::GetFileName($path))
            [IO.File]::Copy($path, $staged, $false)
            if (-not $isWindowsHost) { Invoke-Checked 'chmod' @('u+x', '--', $staged) }
            $hash = (Get-FileHash -LiteralPath $staged).Hash
            if ($hash -cne (Get-FileHash -LiteralPath $path).Hash) { throw "Staged Core executable mismatch: $path" }
            $case = @{
                target = $artifact.target.name; kind = $artifact.target.kind; cargo_executable = $path; executable = $staged;
                sha256 = $hash; cwd = $cwd; stdout = $output; stderr = $errors; pid = $null; exit_code = $null;
                timed_out = $false; test_count = 0; passed = $false
            }
            $summary.cases += $case
            # Owned process with file redirection: no pipe EOF dependency or shared runner.
            $process = Start-Process -FilePath $staged -WorkingDirectory $cwd -RedirectStandardOutput $output -RedirectStandardError $errors -NoNewWindow -PassThru
            try {
                $case.pid = $process.Id
                if (-not $process.WaitForExit($Timeout * 1000)) {
                    $case.timed_out = $true
                    if (-not $process.HasExited) { $process.Kill() }
                    if (-not $process.WaitForExit(5000)) { throw "Core harness did not stop: $staged" }
                }
                $case.exit_code = $process.ExitCode
            } finally { $process.Dispose() }
            $text = [IO.File]::ReadAllText($output)
            $started = [regex]::Matches($text, '(?m)^running (\d+) tests?\r?$')
            $result = [regex]::Matches($text, '(?m)^test result: ok\. (\d+) passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;')
            if ($started.Count -eq 1 -and $result.Count -eq 1) {
                $case.test_count = [int]$result[0].Groups[1].Value
                $case.passed = -not $case.timed_out -and $case.exit_code -eq 0 -and $case.test_count -gt 0 -and
                    $case.test_count -eq [int]$started[0].Groups[1].Value
            }
            if ($case.passed) { $summary.tests_passed += $case.test_count }
            Write-Host "core/$($case.target) pid=$($case.pid) exit=$($case.exit_code) tests=$($case.test_count) passed=$($case.passed)"
        }
        $summary.success = $summary.cases.Count -eq $summary.artifacts -and $summary.tests_passed -gt 0 -and
            @($summary.cases | Where-Object { -not $_.passed }).Count -eq 0
        if (-not $summary.success) { throw 'A Core executable failed, was empty, or did not run every test' }
    } catch {
        $summary.error = $_.Exception.Message
        Write-Host "Core validation failed: $($summary.error)"
    } finally {
        Write-JsonFile $summary (Join-Path $run 'result.json')
        Write-Host "Evidence: $run"
    }
    return $summary.success
}

function Test-ComponentCount($Value, [long]$Minimum = 0) {
    return ($Value -is [int] -or $Value -is [long]) -and $Value -ge $Minimum
}

# The child still owns scenario-specific checks. The parent independently binds
# each successful case to the actual report for its already-validated plan key.
function Assert-ComponentCaseReport($Case, [string]$Stage, [string]$Directory) {
    $cell = "$($Case.mode)-$($Case.backend)"
    $relative = if ($Stage -ceq 'Native') { "$($Case.kind)-$cell-$($Case.scenario)/probe.json" }
        elseif ($Case.kind -ceq 'profile-restart') { "restart-$cell-$($Case.storage)-$($Case.phase)/probe.json" }
        else { "$cell/probe.json" }
    $report = [IO.Path]::GetFullPath((Join-Path $Directory $relative))
    if ($Case.report -isnot [string] -or -not [IO.Path]::IsPathFullyQualified($Case.report) -or
        [IO.Path]::GetFullPath($Case.report) -ine $report) { throw "Unexpected case report path: $relative" }
    $file = [IO.File]::OpenRead($report)
    try {
        if ($file.Length -le 0 -or $file.Length -gt 128KB) { throw "Invalid case report size: $relative" }
        $reader = [IO.StreamReader]::new($file)
        try { $observed = $reader.ReadToEnd() | ConvertFrom-Json -Depth 32 }
        finally { $reader.Dispose() }
    } finally { $file.Dispose() }
    $embedded = ConvertTo-Json -InputObject $Case.observed -Depth 32 -Compress
    $primary = ConvertTo-Json -InputObject $observed -Depth 32 -Compress
    if ($primary -cne $embedded) { throw "Case report disagrees with its embedded observation: $relative" }
    if (-not (Test-ComponentCount $observed.schema 1) -or $observed.schema -ne 1 -or
        -not (Test-ComponentCount $observed.pid 1) -or $observed.pid -ne $Case.pid -or
        -not (Test-ComponentCount $observed.exit_code) -or $observed.exit_code -ne 0 -or $null -ne $observed.error -or
        $observed.requested_mode -cne $Case.mode -or $observed.actual_mode -cne $Case.mode -or
        $observed.requested_backend -cne $Case.backend) { throw "Case report identity/mode mismatch: $relative" }
    if ($Stage -ceq 'Native') {
        if ($observed.scenario -cne $Case.scenario) { throw "Case report scenario mismatch: $relative" }
        if ($Case.kind -ceq 'gpui' -and $observed.kind -cne 'gpui') { throw "Case report host kind mismatch: $relative" }
    } elseif ($observed.kind -cne $Case.kind -or $observed.success -isnot [bool] -or -not $observed.success) {
        throw "Profile case kind/status mismatch: $relative"
    } elseif ($Case.kind -ceq 'profile-restart' -and ($observed.storage -cne $Case.storage -or $observed.phase -cne $Case.phase -or
            $observed.root -cne $Case.root)) {
        throw "Profile restart case scope mismatch: $relative"
    }
    # Software early lifecycle cases can close before a valid VIEW. They are
    # not rendering witnesses even if the probe already observed its backend.
    $softwareEarlyClose = $Stage -ceq 'Native' -and $Case.kind -ceq 'native' -and $Case.backend -ceq 'Software' -and
        $Case.scenario -cin @('Creating','Loading')
    $noFrameEarlyClose = $softwareEarlyClose -and $observed.valid_frame_browsers -eq 0
    if (-not (Test-ComponentCount $observed.valid_frame_browsers) -or
        (-not $noFrameEarlyClose -and $observed.valid_frame_browsers -lt 1) -or
        ($observed.actual_backend -cne $Case.backend -and -not ($noFrameEarlyClose -and $null -eq $observed.actual_backend))) {
        throw "Case report frame/backend mismatch: $relative"
    }
    foreach ($name in @('pending_tasks','pending_requests','task_post_failures','native_calls_after_shutdown')) {
        if (-not (Test-ComponentCount $observed.$name) -or $observed.$name -ne 0) {
            throw "Case report lifecycle counts mismatch ($name): $relative"
        }
    }
    if (-not (Test-ComponentCount $observed.main_thread_id 1) -or -not (Test-ComponentCount $observed.cef_ui_thread_id 1) -or
        -not (Test-ComponentCount $observed.init_thread_id 1) -or $observed.init_thread_id -ne $observed.main_thread_id -or
        -not (Test-ComponentCount $observed.shutdown_thread_id 1) -or $observed.shutdown_thread_id -ne $observed.main_thread_id -or
        -not (Test-ComponentCount $observed.tasks_posted 1) -or -not (Test-ComponentCount $observed.tasks_completed 1) -or
        $observed.tasks_completed -ne $observed.tasks_posted -or
        -not (Test-ComponentCount $observed.cef_shutdown_calls 1) -or $observed.cef_shutdown_calls -ne 1 -or
        $observed.app_reference_retained -isnot [bool] -or $observed.app_reference_retained) {
        throw "Case report lifecycle counts mismatch: $relative"
    }
    $integrated = $Case.mode -ceq 'Integrated'
    if (-not (Test-ComponentCount $observed.manual_pump_count) -or
        ($observed.main_thread_id -eq $observed.cef_ui_thread_id) -ne $integrated -or
        ($observed.manual_pump_count -gt 0) -ne $integrated) { throw "Case report thread/pump mismatch: $relative" }
}

function Assert-ComponentStageResult($Result, [string]$Stage, [string]$Directory) {
    if (-not (Test-ComponentCount $Result.schema 1) -or $Result.schema -ne 1 -or $Result.stage -cne $Stage -or
        $Result.success -isnot [bool] -or -not $Result.success -or $null -ne $Result.error -or @($Result.cases).Count -eq 0) {
        throw "$Stage did not complete with nonempty passing cases"
    }
    foreach ($case in $Result.cases) {
        $timedOut = $case.PSObject.Properties['timed_out']
        # Legacy restart rows omit this field; their launcher checks timeout
        # before marking passed, and the primary report still has to agree.
        $restart = $Stage -ceq 'Profiles' -and $case.kind -ceq 'profile-restart'
        if ($case.passed -isnot [bool] -or -not $case.passed -or
            -not (Test-ComponentCount $case.exit_code) -or $case.exit_code -ne 0 -or -not (Test-ComponentCount $case.pid 1) -or
            ($null -eq $timedOut -and -not $restart) -or
            ($null -ne $timedOut -and ($timedOut.Value -isnot [bool] -or $timedOut.Value))) {
            throw "$Stage did not complete with nonempty passing cases"
        }
    }
    if ($Stage -eq 'Core') {
        if (-not (Test-ComponentCount $Result.artifacts 1) -or $Result.artifacts -ne @($Result.cases).Count -or
            -not (Test-ComponentCount $Result.tests_passed 1) -or
            @($Result.cases | Where-Object { -not (Test-ComponentCount $_.test_count 1) }).Count -ne 0 -or
            $Result.tests_passed -ne ($Result.cases | Measure-Object -Property test_count -Sum).Sum) {
            throw 'Core result does not cover all staged executable tests'
        }
        $actual = @($Result.cases | ForEach-Object { "$($_.kind -join '+')/$($_.target)" })
        Assert-ComponentKeys $actual $Result.artifact_plan 'Core execution'
        return
    }
    $expected = @(foreach ($mode in @('Integrated', 'Threaded')) {
        foreach ($backend in @('Software', 'D3d11')) {
            if ($Stage -eq 'Native') {
                foreach ($scenario in @('Smoke', 'Multiple', 'Creating', 'Loading', 'PendingRequest', 'InFlight', 'CloseTimeout')) {
                    "native/$mode/$backend/$scenario"
                }
                "gpui/$mode/$backend/Smoke"
            } else {
                "profiles/$mode/$backend"
                foreach ($storage in @('Memory', 'Persistent')) {
                    foreach ($phase in @('Write', 'Read')) { "profile-restart/$mode/$backend/$storage/$phase" }
                }
            }
        }
    })
    $actual = @(foreach ($case in $Result.cases) {
        if ($Stage -eq 'Native') { "$($case.kind)/$($case.mode)/$($case.backend)/$($case.scenario)" }
        elseif ($case.kind -ceq 'profile-restart') { "$($case.kind)/$($case.mode)/$($case.backend)/$($case.storage)/$($case.phase)" }
        else { "$($case.kind)/$($case.mode)/$($case.backend)" }
    })
    Assert-ComponentKeys $actual $expected $Stage
    if (-not $Directory) { throw "$Stage report directory is required" }
    foreach ($case in $Result.cases) { Assert-ComponentCaseReport $case $Stage $Directory }
}

function Invoke-ComponentAll([string]$CefDirectory, [string]$TargetDirectory, [int]$Timeout) {
    $run = New-ComponentStageDirectory 'All'
    $summary = @{ schema = 1; stage = 'All'; plan = @('Core', 'Native', 'Profiles'); stages = @(); error = $null; success = $false }
    try {
        $shell = (Get-Process -Id $PID).Path
        $driver = Join-Path $PSScriptRoot 'test-component.ps1'
        foreach ($stage in $summary.plan) {
            $log = Join-Path $run "$stage.log"
            $row = @{ stage = $stage; log = $log; result = $null; exit_code = $null; case_count = 0; passed = $false; error = $null }
            $summary.stages += $row
            try {
                $arguments = @('-Stage', $stage, '-TimeoutSeconds', $Timeout)
                if ($TargetDirectory) { $arguments += @('-TargetDir', $TargetDirectory) }
                if ($stage -ne 'Core') {
                    $arguments += @('-Mode', 'All', '-Backend', 'All')
                    if ($CefDirectory) { $arguments += @('-CefPath', $CefDirectory) }
                }
                $ErrorActionPreference = 'Continue'
                try {
                    $paths = @(& $shell -NoProfile -File $driver @arguments 2>&1 | Tee-Object -FilePath $log | ForEach-Object {
                        Write-Host $_
                        if ([string]$_ -cmatch '^Evidence: (.+)$') { $Matches[1] }
                    })
                    $row.exit_code = $LASTEXITCODE
                } finally { $ErrorActionPreference = 'Stop' }
                if ($paths.Count -ne 1) { throw "$stage must return one evidence path, got $($paths.Count)" }
                $directory = [IO.Path]::GetFullPath($paths[0])
                $parent = Resolve-ValidationDir $stage
                if ([IO.Path]::GetDirectoryName($directory) -ine [IO.Path]::GetFullPath($parent) -or
                    [IO.Path]::GetFileName($directory) -cnotmatch '^[0-9a-f]{32}$') {
                    throw "$stage returned an unexpected evidence path"
                }
                $row.result = Join-Path $directory 'result.json'
                $result = Get-Content -LiteralPath $row.result -Raw -Encoding UTF8 | ConvertFrom-Json
                $row.case_count = @($result.cases).Count
                if ($row.exit_code -ne 0) { throw "$stage exited $($row.exit_code); see its result and log" }
                Assert-ComponentStageResult $result $stage $directory
                $row.passed = $true
            } catch { $row.error = $_.Exception.Message; Write-Host $row.error }
        }
        $summary.success = $summary.stages.Count -eq $summary.plan.Count -and
            @($summary.stages | Where-Object { -not $_.passed }).Count -eq 0
        if (-not $summary.success) { throw 'At least one required component stage failed' }
    } catch { $summary.error = $_.Exception.Message; Write-Host $summary.error }
    finally {
        Write-JsonFile $summary (Join-Path $run 'result.json')
        Write-Host "Evidence: $run"
    }
    return $summary.success
}
