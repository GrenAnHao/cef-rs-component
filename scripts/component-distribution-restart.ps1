# Sourced by test-component-distribution.ps1; reuses the public Embed observer.
function Invoke-DistributionFixture([string]$Origin, [string]$Path, [ValidateSet('Get','Post')][string]$Method = 'Get', $Body = $null) {
    $request = @{ Uri = "$Origin$Path"; Method = $Method; NoProxy = $true; MaximumRedirection = 0; TimeoutSec = 3 }
    if ($Method -eq 'Post') {
        $request.Body = ConvertTo-Json -InputObject $Body -Compress
        $request.ContentType = 'application/json'
    }
    Invoke-RestMethod @request
}

function Confirm-DistributionPage($Reply, [string]$Origin, [string]$Token, [string]$Operation, [string]$Value) {
    $expected = "$Origin/profile-test?token=$Token&op=$Operation&value=$Value"
    if ($Reply.url -cne $expected) { throw 'Fixture returned a different restart page' }
    return $expected
}

function Observe-DistributionStorage([string]$Origin, [string]$Token, [AllowNull()][object]$ExpectedValue, [int]$OwnedPid, [string]$Case) {
    $deadline = [Diagnostics.Stopwatch]::StartNew()
    do {
        $observation = Invoke-DistributionFixture $Origin "/observation?token=$Token"
        if ($observation.ready -isnot [bool]) { throw 'Missing typed storage readiness' }
        if ($observation.ready) { break }
        if ($deadline.Elapsed.TotalSeconds -ge 8) { throw 'No actual page storage result before detach' }
        Start-Sleep -Milliseconds 40
    } while ($true)
    if ($observation.token -cne $Token -or $null -eq $observation.PSObject.Properties['cookie'] -or
        $null -eq $observation.PSObject.Properties['local_storage']) { throw 'Storage result identity or field missing' }
    $cookie = $observation.cookie; $local = $observation.local_storage
    $cookieMatches = if ($null -eq $ExpectedValue) { $null -eq $cookie } else { $cookie -is [string] -and $cookie -ceq $ExpectedValue }
    $localMatches = if ($null -eq $ExpectedValue) { $null -eq $local } else { $local -is [string] -and $local -ceq $ExpectedValue }
    $health = Invoke-DistributionFixture $Origin '/health'
    Write-Report @{ token = $Token; pid = $OwnedPid; ready = $true; cookie_matches = $cookieMatches; local_storage_matches = $localMatches;
        cookie_is_null = ($null -eq $cookie); local_storage_is_null = ($null -eq $local); fixture_error_count = @($health.errors).Count } (Join-Path $Case 'storage-observation.json')
    if (-not ($cookieMatches -and $localMatches) -or @($health.errors).Count -ne 0) { throw 'Actual restart storage result differed from expectation' }
}

function Invoke-DistributionWriteRead($Summary, [string]$Origin, [string]$Outside) {
    $expectedKeys = @()
    foreach ($thread in @('Integrated','Threaded')) { foreach ($render in @('Software','D3d11')) { foreach ($storage in @('Memory','Persistent')) {
        $pair = "$thread-$render-$storage"
        $profileRoot = Join-Path $Outside "profile root $pair"
        if (Test-Path -LiteralPath $profileRoot) { throw 'Write requires an unused root' }
        $seed = [guid]::NewGuid().ToString('N')
        $writePid = $null
        foreach ($phase in @('Write','Read')) {
            $restartToken = "$pair-$phase"
            $expectedKeys += $restartToken
            $caseDirectory = Join-Path $Outside $restartToken
            $operation = $phase.ToLowerInvariant()
            $registrationValue = if ($phase -ceq 'Write') { $seed } else { '' }
            $reply = Invoke-DistributionFixture $Origin '/register' Post @{ token = $restartToken; op = $operation; value = $registrationValue }
            $page = Confirm-DistributionPage $reply $Origin $restartToken $operation $registrationValue
            $expectedValue = if ($phase -ceq 'Read' -and $storage -ceq 'Memory') { $null } else { $seed }
            $observe = {
                param([int]$OwnedPid)
                Observe-DistributionStorage $Origin $restartToken $expectedValue $OwnedPid $caseDirectory
            }
            $extra = @('--restart-storage',$storage,'--restart-phase',$phase,'--restart-root',$profileRoot,'--restart-page',$page,'--restart-token',$restartToken)
            $record = Invoke-DistributionEmbedCase $caseDirectory $thread $render $extra $observe
            $record.storage = $storage; $record.phase = $phase; $record.token = $restartToken
            $record.root = $profileRoot; $record.fixture_origin = $Origin
            $Summary.cases += $record
            if ($record.passed) {
                $probe = Get-Content -LiteralPath (Join-Path $caseDirectory 'probe.json') -Raw -Encoding UTF8 | ConvertFrom-Json
                $storageResult = Get-Content -LiteralPath (Join-Path $caseDirectory 'storage-observation.json') -Raw -Encoding UTF8 | ConvertFrom-Json
                $p = $probe.profile_restart
                $flushes = if ($phase -ceq 'Write') { 1 } else { 0 }
                $localHandle = if ($storage -ceq 'Memory') { 0 } else { 1 }
                $expectAbsent = $null -eq $expectedValue
                $record.passed = $null -ne $p -and $p.root -ceq $profileRoot -and $p.origin -ceq $Origin -and $p.token -ceq $restartToken -and
                    $p.storage -ceq $storage -and $p.phase -ceq $phase -and $p.root_lease_observed -and
                    @($p.profile).Count -eq 2 -and $p.profile[0] -gt 0 -and $p.profile[1] -eq $localHandle -and
                    $p.cookie_flush_complete -eq ($flushes -eq 1) -and $probe.pending_cookie_flushes -eq 0 -and
                    $probe.cookie_flushes_accepted -eq $flushes -and $probe.cookie_flushes_completed -eq $flushes -and
                    $storageResult.token -ceq $restartToken -and $storageResult.pid -eq $record.pid -and $storageResult.ready -and
                    $storageResult.cookie_matches -and $storageResult.local_storage_matches -and $storageResult.fixture_error_count -eq 0 -and
                    $storageResult.cookie_is_null -eq $expectAbsent -and $storageResult.local_storage_is_null -eq $expectAbsent -and
                    (Test-Path -LiteralPath $profileRoot -PathType Container) -and
                    ($phase -ceq 'Write' -or ($null -ne $writePid -and $writePid -ne $record.pid))
            }
            Write-Host "DistributionRestart/$pair/$phase pid=$($record.pid) exit=$($record.exit_code) passed=$($record.passed)"
            if (-not $record.passed) { throw "Distribution restart case failed: $restartToken" }
            if ($phase -ceq 'Write') { $writePid = $record.pid }
        }
    }}}
    $keys = @($Summary.cases | ForEach-Object { $_.token })
    if ($keys.Count -ne 16 -or ($keys -join ',') -cne ($expectedKeys -join ',')) { throw 'Distributed restart manifest incomplete' }
}

function Invoke-DistributionProfileRestart($Summary, [string]$Run, [string]$Outside) {
    $python = (Get-Command python -CommandType Application -TotalCount 1 -ErrorAction Stop).Source
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $python
    $info.WorkingDirectory = $Outside
    $ready = Join-Path $Run 'http-ready.json'; $final = Join-Path $Run 'http-final.json'
    foreach ($arg in @('-B','-u',(Join-Path $PSScriptRoot 'component-profile-server.py'),'--ready-file',$ready,'--final-report',$final)) { $info.ArgumentList.Add($arg) }
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    $process = [Diagnostics.Process]::Start($info)
    $origin = $null; $failure = $null; $cleanup = $null
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync(); $stderr = $process.StandardError.ReadToEndAsync()
        $watch = [Diagnostics.Stopwatch]::StartNew()
        while (-not [IO.File]::Exists($ready)) {
            if ($process.WaitForExit(50)) { throw "Restart HTTP fixture exited before readiness: $($process.ExitCode)" }
            if ($watch.Elapsed.TotalSeconds -ge 10) { throw 'Restart HTTP fixture readiness timed out' }
        }
        $start = Get-Content -LiteralPath $ready -Raw -Encoding UTF8 | ConvertFrom-Json
        if ($start.pid -ne $process.Id -or $start.origin -cnotmatch '^http://127\.0\.0\.1:[1-9][0-9]{0,4}$') { throw 'Restart HTTP fixture identity invalid' }
        $port = ([uri]$start.origin).Port
        if ($port -lt 1 -or $port -gt 65535) { throw 'Invalid fixture port' }
        $origin = $start.origin
        Invoke-DistributionWriteRead $Summary $origin $Outside
    } catch { $failure = $_ }
    finally {
        try {
            if (-not $process.HasExited -and $origin) { $null = Invoke-DistributionFixture $origin '/stop' Post @{} }
            if (-not $process.WaitForExit(5000)) { throw 'Owned HTTP fixture did not exit normally' }
            if ($process.ExitCode -ne 0) { throw "HTTP fixture exit $($process.ExitCode)" }
            $result = Get-Content -LiteralPath $final -Raw -Encoding UTF8 | ConvertFrom-Json
            $Summary.fixture = @{ pid = $process.Id; exit_code = $process.ExitCode; origin = $origin; result = $result }
            if ($result.registered -ne 16 -or @($result.errors).Count -ne 0) { throw 'Restart fixture registration or sticky error check failed' }
        } catch {
            $cleanup = $_
            if (-not $process.HasExited) { $process.Kill(); $null = $process.WaitForExit(5000) }
        } finally {
            if ($stdout.Wait(1000)) { [IO.File]::WriteAllText((Join-Path $Run 'http-stdout.log'), $stdout.Result) }
            if ($stderr.Wait(1000)) { [IO.File]::WriteAllText((Join-Path $Run 'http-stderr.log'), $stderr.Result) }
            $process.Dispose()
        }
    }
    if ($null -ne $cleanup) { $Summary.fixture_cleanup_error = $cleanup.ToString() }
    if ($null -ne $failure) { throw $failure }
    if ($null -ne $cleanup) { throw $cleanup }
}
