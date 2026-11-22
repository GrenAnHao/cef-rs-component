# Pure classification plus narrow OS confirmation of snapshot candidate PIDs.
function Get-ObservedProcessState([int]$ProcessId, [int]$WaitMilliseconds = 0, [long]$ExpectedCreationTicks = 0) {
    try { $process = [Diagnostics.Process]::GetProcessById($ProcessId) }
    catch {
        if ($_.Exception.GetBaseException() -is [ArgumentException]) {
            return @{ pid = $ProcessId; exited_confirmed = $true; executable_path = $null; error = $null }
        }
        return @{ pid = $ProcessId; exited_confirmed = $false; executable_path = $null; error = $_.Exception.Message }
    }
    try {
        if ($process.HasExited) { return @{ pid = $ProcessId; exited_confirmed = $true; executable_path = $null; error = $null } }
        if ($ExpectedCreationTicks -gt 0) {
            $actualTicks = $process.StartTime.ToUniversalTime().Ticks
            # CIM CreationDate is microsecond precision; Process.StartTime may
            # retain 100ns precision. Compare the same process identity, not PID alone.
            if ([Math]::Abs($actualTicks - $ExpectedCreationTicks) -ge 10) {
                return @{ pid = $ProcessId; exited_confirmed = $false; executable_path = $null; error = 'PID creation-time mismatch'; creation_ticks = $actualTicks }
            }
        }
        if ($process.HasExited -or ($WaitMilliseconds -gt 0 -and $process.WaitForExit($WaitMilliseconds))) {
            return @{ pid = $ProcessId; exited_confirmed = $true; executable_path = $null; error = $null }
        }
        try { $image = $process.MainModule.FileName }
        catch {
            if ($process.HasExited) { return @{ pid = $ProcessId; exited_confirmed = $true; executable_path = $null; error = $null } }
            return @{ pid = $ProcessId; exited_confirmed = $false; executable_path = $null; error = $_.Exception.Message }
        }
        return @{ pid = $ProcessId; exited_confirmed = $false; executable_path = $image; error = $null }
    } catch {
        return @{ pid = $ProcessId; exited_confirmed = $false; executable_path = $null; error = $_.Exception.Message }
    } finally { $process.Dispose() }
}

function Test-ObservedSnapshotHistory($Samples, [string]$Helper) {
    foreach ($sample in $Samples) {
        foreach ($child in $sample.children) {
            if ($child.ContainsKey('exited_confirmed') -and $child.exited_confirmed) { continue }
            if ([string]::IsNullOrEmpty($child.role) -or [string]::IsNullOrEmpty($child.executable_path) -or
                -not [string]::Equals($child.executable_path, $Helper, [StringComparison]::OrdinalIgnoreCase)) { return $false }
        }
    }
    return $true
}

function Get-ObservedSurvivors($Candidates, [string]$Helper) {
    $unknown = @($Candidates | Where-Object { [string]::IsNullOrEmpty($_.executable_path) })
    if ($unknown.Count -gt 0) { throw "Residual process observation incomplete: unreadable image for candidate PIDs $($unknown.pid -join ',')" }
    $Candidates | Where-Object { [string]::Equals($_.executable_path, $Helper, [StringComparison]::OrdinalIgnoreCase) } | ForEach-Object { [int]$_.pid }
}

function Test-ObservedChildImages($Children, [string]$Helper) {
    if (@($Children).Count -eq 0) { return $false }
    if (@($Children | Where-Object { $_.ContainsKey('exited_confirmed') -and $_.exited_confirmed }).Count -gt 0) { return $false }
    if (@($Children | Where-Object { [string]::IsNullOrEmpty($_.role) }).Count -gt 0) { return $false }
    $renderers = @($Children | Where-Object { $_.role -eq 'renderer' })
    $wrongImages = @($Children | Where-Object {
        [string]::IsNullOrEmpty($_.executable_path) -or
        -not [string]::Equals($_.executable_path, $Helper, [StringComparison]::OrdinalIgnoreCase)
    })
    return $renderers.Count -gt 0 -and $wrongImages.Count -eq 0
}
