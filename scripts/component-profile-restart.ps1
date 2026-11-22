# Called by the Profiles driver; shares its artifact and owned-probe helpers.
function Invoke-ProfileRestartCases([string]$RunDirectory, [hashtable[]]$Cells, $Messages, [string]$HelperArtifact, [string]$Cef, [int]$TimeoutSeconds) {
    $result = @{ cases = @(); fixture = $null; error = $null }
    $process = $null
    $origin = $null
    $outRead = $null
    $errRead = $null
    $ready = Join-Path $RunDirectory 'http-ready.json'
    $final = Join-Path $RunDirectory 'http-final.json'
    try {
        $artifact = Get-ProfileArtifact $Messages 'profile-restart-probe'
        $bundle = Join-Path $RunDirectory ([string][char]0x53d1 + [char]0x884c + ' Restart bundle with spaces')
        $packageOutput = & (Join-Path $PSScriptRoot 'package-component.ps1') -HostExecutable $artifact -HelperExecutable $HelperArtifact -CefPath $Cef -OutputDirectory $bundle -HostBuildVersion 'profile-restart-test-support'
        foreach ($line in $packageOutput) { Write-Host $line }
        $executable = Join-Path $bundle ([IO.Path]::GetFileName($artifact))
        $helper = Join-Path $bundle ([IO.Path]::GetFileName($HelperArtifact))
        Write-ProfileJson @{ host_artifact = $artifact; helper_artifact = $HelperArtifact; host_sha256 = (Get-FileHash -LiteralPath $executable).Hash; helper_sha256 = (Get-FileHash -LiteralPath $helper).Hash } (Join-Path $RunDirectory 'restart-artifact-identity.json')
        $python = (Get-Command python -ErrorAction Stop).Source
        $info = New-Object Diagnostics.ProcessStartInfo
        $info.FileName = $python
        $arguments = @('-u', (Join-Path $PSScriptRoot 'component-profile-server.py'), '--ready-file', $ready, '--final-report', $final)
        $quoted = foreach ($argument in $arguments) {
            $escaped = [regex]::Replace($argument, '(\\*)"', '$1$1\"')
            '"' + [regex]::Replace($escaped, '(\\+)$', '$1$1') + '"'
        }
        $info.Arguments = $quoted -join ' '
        $info.UseShellExecute = $false
        $info.RedirectStandardOutput = $true
        $info.RedirectStandardError = $true
        $process = New-Object Diagnostics.Process
        $process.StartInfo = $info
        if (-not $process.Start()) { throw 'HTTP fixture did not start' }
        $outRead = $process.StandardOutput.ReadToEndAsync()
        $errRead = $process.StandardError.ReadToEndAsync()
        $watch = [Diagnostics.Stopwatch]::StartNew()
        while (-not (Test-Path -LiteralPath $ready)) {
            if ($process.WaitForExit(50)) { throw "HTTP fixture exited before readiness: $($process.ExitCode)" }
            if ($watch.Elapsed.TotalSeconds -gt 10) { throw 'HTTP fixture readiness timed out' }
        }
        $startup = Get-Content -LiteralPath $ready -Raw -Encoding UTF8 | ConvertFrom-Json
        if ($startup.pid -ne $process.Id -or $startup.origin -notmatch '^http://127\.0\.0\.1:\d+$') { throw 'HTTP fixture identity/origin invalid' }
        $origin = $startup.origin
        foreach ($cell in $Cells) {
            $selectedMode = $cell.mode
            $selectedBackend = $cell.backend
            $caseKey = "$selectedMode-$selectedBackend"
            foreach ($storage in @('Memory', 'Persistent')) {
                # Exactly the same root for Write then Read, distinct across cells.
                $root = Join-Path $RunDirectory "restart-cache-$caseKey-$storage"
                foreach ($phase in @('Write', 'Read')) {
                    $case = Join-Path $RunDirectory "restart-$caseKey-$storage-$phase"
                    $cwd = Join-Path $case 'unrelated cwd'
                    New-Item -ItemType Directory -Path $cwd -Force | Out-Null
                    $reportPath = Join-Path $case 'probe.json'
                    $gate = Join-Path $case 'observer.release'
                    $token = "$caseKey-$storage-$phase"
                    $launch = Invoke-Probe $executable @('--mode', $selectedMode, '--backend', $selectedBackend, '--report', $reportPath, '--helper', $helper, '--observer-gate', $gate, '--scenario', 'Smoke', '--storage', $storage, '--phase', $phase, '--root', $root, '--fixture-origin', $origin, '--token-prefix', $token) $cwd $gate $helper
                    Write-ProfileJson @{ children = $launch.children; image_verified = $launch.image_verified; observer_error = $launch.observer_error; remaining_processes = $launch.remaining_processes; observation_samples = $launch.observation_samples; residual_observations = $launch.residual_observations } (Join-Path $case 'child-images.json')
                    [IO.File]::WriteAllText((Join-Path $case 'stdout.log'), $launch.stdout)
                    [IO.File]::WriteAllText((Join-Path $case 'stderr.log'), $launch.stderr)
                    $observed = $null
                    $passed = $false
                    if (Test-Path -LiteralPath $reportPath) {
                        $observed = Get-Content -LiteralPath $reportPath -Raw -Encoding UTF8 | ConvertFrom-Json
                        $threadRule = ($selectedMode -eq 'Integrated' -and $observed.cef_ui_thread_id -eq $observed.main_thread_id) -or ($selectedMode -eq 'Threaded' -and $observed.cef_ui_thread_id -ne $observed.main_thread_id -and $observed.manual_pump_count -eq 0)
                        $checks = @($observed.storage_checks)
                        $passed = $launch.exit_code -eq 0 -and -not $launch.timed_out -and $launch.streams_complete -and $launch.image_verified -and
                            $null -eq $launch.observer_error -and $launch.remaining_processes.Count -eq 0 -and
                            $observed.schema -eq 1 -and $observed.kind -eq 'profile-restart' -and $observed.success -eq $true -and $observed.error -eq $null -and $observed.exit_code -eq 0 -and
                            $observed.pid -eq $launch.pid -and $observed.storage -eq $storage -and $observed.phase -eq $phase -and
                            $observed.requested_mode -eq $selectedMode -and $observed.actual_mode -eq $selectedMode -and
                             $observed.requested_backend -eq $selectedBackend -and $observed.actual_backend -eq $selectedBackend -and $threadRule -and
                            $observed.main_thread_id -gt 0 -and $observed.init_thread_id -eq $observed.main_thread_id -and $observed.shutdown_thread_id -eq $observed.main_thread_id -and
                            $observed.created -eq 4 -and $observed.closed -eq 4 -and $observed.valid_frame_browsers -eq 4 -and $observed.same_runtime_profile_reused -eq $true -and
                            $checks.Count -eq 3 -and @($checks | Where-Object { $_.passed -ne $true }).Count -eq 0 -and
                            $observed.cookie_flushes_accepted -eq 3 -and $observed.cookie_flushes_completed -eq 3 -and $observed.pending_cookie_flushes -eq 0 -and
                            $observed.pending_tasks -eq 0 -and ($observed.tasks_completed + $observed.task_post_failures) -eq $observed.tasks_posted -and
                            $observed.pending_profiles -eq 0 -and $observed.pending_requests -eq 0 -and $observed.profile_references_retained -eq 0 -and $observed.root_lease_held -eq $false -and $observed.root_lease_active_observed -eq $true -and
                            $observed.cef_shutdown_calls -eq 1 -and $observed.native_calls_after_shutdown -eq 0 -and $observed.app_reference_retained -eq $false -and $observed.single_process_disabled -eq $true
                    }
                    $result.cases += @{ kind = 'profile-restart'; mode = $selectedMode; backend = $selectedBackend; storage = $storage; phase = $phase; root = $root; fixture_origin = $origin; pid = $launch.pid; exit_code = $launch.exit_code; passed = $passed; report = $reportPath; observed = $observed }
                    Write-Host "restart/$selectedMode/$selectedBackend/$storage/$phase pid=$($launch.pid) exit=$($launch.exit_code) passed=$passed"
                }
            }
        }
    } catch { $result.error = $_.Exception.Message }
    finally {
        if ($null -ne $process) {
            try {
                if (-not $process.HasExited -and $null -ne $origin) {
                    $null = Invoke-WebRequest -Uri "$origin/stop" -Method Post -Body '{}' -ContentType 'application/json' -TimeoutSec 3 -UseBasicParsing
                }
                if (-not $process.WaitForExit(5000)) { throw 'HTTP fixture did not stop normally' }
                if ($process.ExitCode -ne 0) { throw "HTTP fixture exit: $($process.ExitCode)" }
                if (-not (Test-Path -LiteralPath $final)) { throw 'HTTP fixture final error snapshot missing' }
                $result.fixture = Get-Content -LiteralPath $final -Raw -Encoding UTF8 | ConvertFrom-Json
                if (@($result.fixture.errors).Count -ne 0) { throw 'HTTP fixture recorded sticky protocol errors' }
                if ($result.fixture.registered -ne ($Cells.Count * 16)) { throw 'HTTP fixture registration count does not cover the full restart matrix' }
            } catch {
                if ($null -eq $result.error) { $result.error = $_.Exception.Message }
                # Only the server we started is eligible for forced cleanup.
                if (-not $process.HasExited) { $process.Kill(); $null = $process.WaitForExit(5000) }
            } finally {
                if ($null -ne $outRead -and $outRead.Wait(1000)) { [IO.File]::WriteAllText((Join-Path $RunDirectory 'http-stdout.log'), $outRead.Result) }
                if ($null -ne $errRead -and $errRead.Wait(1000)) { [IO.File]::WriteAllText((Join-Path $RunDirectory 'http-stderr.log'), $errRead.Result) }
                $process.Dispose()
            }
        }
    }
    return $result
}
