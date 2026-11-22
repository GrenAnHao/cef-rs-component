# Shared owned-process launcher for native and profile validation hosts.
# Caller sources component-process-observer.ps1 and supplies TimeoutSeconds.
function Invoke-Probe([string]$Executable, [string[]]$Arguments, [string]$Cwd, [string]$ObserverGate, [string]$Helper, [scriptblock]$BeforeRelease = $null, [string]$CleanUserRoot = '', [switch]$Utf8Streams) {
    # Same MSCRT argv quoting as the independently tested Cargo runner.
    $quoted = foreach ($argument in $Arguments) {
        $escaped = [regex]::Replace($argument, '(\\*)"', '$1$1\"')
        '"' + [regex]::Replace($escaped, '(\\+)$', '$1$1') + '"'
    }
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = $Executable
    $info.Arguments = $quoted -join ' '
    $info.WorkingDirectory = $Cwd
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    if ($Utf8Streams) {
        # Rust's redirected Release startup diagnostic must preserve Unicode paths.
        $info.StandardOutputEncoding = [Text.UTF8Encoding]::new($false, $true)
        $info.StandardErrorEncoding = [Text.UTF8Encoding]::new($false, $true)
    }
    # Deliberately remove development settings only from this child's environment.
    foreach ($key in @($info.EnvironmentVariables.Keys)) {
        if ($key -match '^(POC_|CARGO_)|^(CEF_PATH|START_URL)$') { $info.EnvironmentVariables.Remove($key) }
    }
    $info.EnvironmentVariables['PATH'] = "$env:SystemRoot\System32;$env:SystemRoot"
    $cleanUser = $null
    if ($CleanUserRoot) {
        if (-not [IO.Path]::IsPathFullyQualified($CleanUserRoot) -or [IO.Directory]::Exists($CleanUserRoot)) {
            throw 'CleanUserRoot must be an unused absolute directory in the owned case'
        }
        $paths = @{ USERPROFILE = $CleanUserRoot; HOME = $CleanUserRoot; APPDATA = (Join-Path $CleanUserRoot 'Roaming'); LOCALAPPDATA = (Join-Path $CleanUserRoot 'Local'); TEMP = (Join-Path $CleanUserRoot 'Temp') }
        $paths.TMP = $paths.TEMP
        foreach ($key in $paths.Keys) {
            $null = [IO.Directory]::CreateDirectory($paths[$key])
            $info.EnvironmentVariables[$key] = $paths[$key]
        }
        foreach ($key in @($info.EnvironmentVariables.Keys)) {
            if ($key -match '^(RUSTUP_|XDG_)|^(RUSTC|RUSTDOC|RUSTFLAGS|RUSTC_WRAPPER|HOMEDRIVE|HOMEPATH)$') { $info.EnvironmentVariables.Remove($key) }
        }
        $cleanUser = @{ executable = $Executable; cwd = $Cwd; arguments = $Arguments; user_dirs = $paths; path = $info.EnvironmentVariables['PATH'] }
    }
    $process = New-Object Diagnostics.Process
    $process.StartInfo = $info
    try {
        if (-not $process.Start()) { throw 'Probe did not start' }
        $childId = $process.Id
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $watch = [Diagnostics.Stopwatch]::StartNew()
        $children = @()
        $observationSamples = @()
        $residualObservations = @()
        $observerError = $null
        $observedChildren = $false
        while (-not $process.WaitForExit(100) -and $watch.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
            if (-not $observedChildren -and (Test-Path -LiteralPath ([IO.Path]::ChangeExtension($ObserverGate, 'ready')))) {
                try {
                    $readyPid = [IO.File]::ReadAllText([IO.Path]::ChangeExtension($ObserverGate, 'ready')).Trim()
                    if ($readyPid -ne [string]$childId) { throw 'Observer readiness PID mismatch' }
                    # Query the OS while the actual Browser remains alive, not
                    # the helper setting or a fabricated --type invocation.
                    for ($sample = 0; $sample -lt 3; $sample++) {
                        $records = @(Get-CimInstance Win32_Process -Filter "ParentProcessId = $childId" -Property ProcessId, ParentProcessId, ExecutablePath, CommandLine, CreationDate -OperationTimeoutSec 5)
                        $children = @($records | ForEach-Object {
                            $role = $null
                            if ($_.CommandLine -match '(?:^|\s)--type=([^\s"]+)') { $role = $Matches[1] }
                            if ($null -eq $_.CreationDate) { throw 'Missing process creation time' }
                            $created = $_.CreationDate.ToUniversalTime().Ticks
                            $state = Get-ObservedProcessState ([int]$_.ProcessId) 0 $created
                            @{ pid = [int]$_.ProcessId; parent_pid = [int]$_.ParentProcessId; creation_ticks = $created; executable_path = $state.executable_path; cim_executable_path = $_.ExecutablePath; role = $role; exited_confirmed = $state.exited_confirmed; state_error = $state.error }
                        })
                        $observationSamples += @{ sample = $sample; children = $children }
                        if (-not (Test-ObservedSnapshotHistory $observationSamples $Helper)) { throw 'An observed live image/identity/role was wrong or unverifiable; later snapshots cannot erase it' }
                        # An exited snapshot member cannot establish an image.
                        # Preserve it and take a fresh coherent snapshot; never
                        # discard an unreadable live member or a wrong image.
                        if (@($children | Where-Object { $_.exited_confirmed }).Count -eq 0) { break }
                    }
                    # Optional owned-host visual check. The default path remains
                    # byte-for-byte the same observer/gate lifecycle; callback
                    # failures are sticky observation failures, never ignored.
                    if ($null -ne $BeforeRelease) { $null = & $BeforeRelease $childId }
                } catch { $observerError = "child observation: $($_.Exception.GetType().FullName) HRESULT=$($_.Exception.HResult) id=$($_.FullyQualifiedErrorId): $($_.Exception.Message)" }
                finally {
                    $observedChildren = $true
                    [IO.File]::WriteAllText($ObserverGate, 'release')
                }
            }
        }
        $timedOut = -not $process.HasExited
        if ($timedOut) {
            # This owns only the launched process. A timeout is always failure;
            # it does not authorize CEF shutdown or claim descendants were drained.
            $process.Kill()
            if (-not $process.WaitForExit(5000)) { throw "Timed out probe $childId could not exit" }
        }
        $streamsComplete = $stdout.Wait(3000) -and $stderr.Wait(3000)
        $textOut = if ($stdout.IsCompleted) { $stdout.Result } else { '[stdout remained open after parent exit]' }
        $textErr = if ($stderr.IsCompleted) { $stderr.Result } else { '[stderr remained open after parent exit]' }
        $imagesVerified = $observedChildren -and $null -eq $observerError -and (Test-ObservedChildImages $children $Helper)
        $remaining = @()
        try {
            # The bundle is unique to this run. Check actual surviving images,
            # including children that exited before the readiness snapshot.
            $name = [IO.Path]::GetFileName($Helper)
            $candidates = @(Get-CimInstance Win32_Process -Filter "Name = '$name'" -Property ProcessId, ExecutablePath, CreationDate -OperationTimeoutSec 5 | ForEach-Object {
                if ($null -eq $_.CreationDate) { throw 'Missing survivor candidate creation time' }
                @{ pid = [int]$_.ProcessId; executable_path = $_.ExecutablePath; creation_ticks = $_.CreationDate.ToUniversalTime().Ticks }
            })
            $grace = [Diagnostics.Stopwatch]::StartNew()
            $active = @()
            foreach ($candidate in $candidates) {
                $state = Get-ObservedProcessState $candidate.pid 0 $candidate.creation_ticks
                if (-not $state.exited_confirmed -and [string]::Equals($state.executable_path, $Helper, [StringComparison]::OrdinalIgnoreCase)) {
                    $left = [Math]::Max(0, 5000 - [int]$grace.ElapsedMilliseconds)
                    $state = Get-ObservedProcessState $candidate.pid $left $candidate.creation_ticks
                }
                $residualObservations += @{ pid = $candidate.pid; creation_ticks = $candidate.creation_ticks; cim_executable_path = $candidate.executable_path; executable_path = $state.executable_path; exited_confirmed = $state.exited_confirmed; state_error = $state.error }
                if (-not $state.exited_confirmed) { $active += @{ pid = $candidate.pid; executable_path = $state.executable_path } }
            }
            $remaining = @(Get-ObservedSurvivors $active $Helper)
        } catch { $observerError = "survivor observation: $($_.Exception.GetType().FullName) HRESULT=$($_.Exception.HResult) id=$($_.FullyQualifiedErrorId): $($_.Exception.Message)" }
        $result = @{ pid = $childId; exit_code = $process.ExitCode; timed_out = $timedOut; streams_complete = $streamsComplete; stdout = $textOut; stderr = $textErr; children = $children; image_verified = $imagesVerified; live_images_verified = $imagesVerified; observer_error = $observerError; remaining_processes = $remaining; observation_samples = $observationSamples; residual_observations = $residualObservations }
        if ($null -ne $cleanUser) { $result.clean_launch = $cleanUser }
        if ($Utf8Streams) { $result.stream_decoding = 'UTF-8' }
        return $result
    } finally { $process.Dispose() }
}

