# Sourced only by the explicit release path-validation smoke.
function Get-DemoStartupDiagnostic($Launch) {
    $pattern = '(?m)^component demo start: Deployment\((?<kind>[A-Za-z]+) \{ path: (?<path>"(?:[^"\\]|\\.)*")(?:, reason: (?<reason>"(?:[^"\\]|\\.)*"))? \}\)\r?$'
    $diagnostics = [regex]::Matches($Launch.stderr, $pattern)
    if ($diagnostics.Count -ne 1 -or [regex]::Matches($Launch.stderr, '(?m)^component demo start: Deployment\(').Count -ne 1) { return $null }
    $path = $diagnostics[0].Groups['path'].Value | ConvertFrom-Json
    if ($path -isnot [string]) { return $null }
    if ($path.StartsWith('\\?\', [StringComparison]::Ordinal)) { $path = $path.Substring(4) }
    if (-not [IO.Path]::IsPathFullyQualified($path)) { return $null }
    $kind = $diagnostics[0].Groups['kind'].Value
    $reasonGroup = $diagnostics[0].Groups['reason']
    if ($reasonGroup.Success -ne ($kind -ceq 'ManifestInvalid')) { return $null }
    $result = @{ kind = $kind; path = $path }
    if ($reasonGroup.Success) {
        $reason = $reasonGroup.Value | ConvertFrom-Json
        if ($reason -isnot [string] -or -not $reason.Trim()) { return $null }
        $result.reason = $reason
    }
    return $result
}

function Test-DemoPathRefusal($Launch, $Diagnostic, [string]$Kind, [string]$Path, [string]$Cache, [string]$Gate, [string]$ExpectedReason = '') {
    $reason = if ($Diagnostic -and $Diagnostic.ContainsKey('reason')) { [string]$Diagnostic.reason } else { '' }
    return ($reason -ceq $ExpectedReason) -and $null -ne $Diagnostic -and $Diagnostic.kind -ceq $Kind -and
        [string]::Equals($Diagnostic.path, $Path, [StringComparison]::OrdinalIgnoreCase) -and
        $Launch.stream_decoding -ceq 'UTF-8' -and $Launch.exit_code -eq 1 -and $Launch.streams_complete -and -not $Launch.timed_out -and
        -not $Launch.live_images_verified -and $null -eq $Launch.observer_error -and
        @($Launch.children).Count -eq 0 -and @($Launch.remaining_processes).Count -eq 0 -and
        $Launch.stdout -notmatch '(?m)^COMPONENT_DEMO_(?:VIEWPORT|PAINT|STOP) ' -and
        -not (Test-Path -LiteralPath ([IO.Path]::ChangeExtension($Gate,'ready'))) -and
        -not (Test-Path -LiteralPath $Gate) -and -not (Test-Path -LiteralPath $Cache)
}

function Invoke-DemoPathRefusal([string]$Root, [string]$Name, [string]$Bundle, [string]$Exe, [string]$Helper, [string]$ObserverHelper, [string]$Kind, [string]$ExpectedPath, [string]$ExpectedReason = '') {
    $case=Join-Path $Root $Name
    $cwd=Join-Path $case 'unrelated cwd'; $gate=Join-Path $case 'observer.release'; $cache=Join-Path $case 'cache'
    New-Item -ItemType Directory -Path $case,$cwd | Out-Null
    $cli=@('--mode','Threaded','--backend','D3d11','--bundle',$Bundle,'--helper',$Helper,'--cache-root',$cache,'--smoke-ms','400','--observer-gate',$gate)
    $launch=Invoke-Probe $Exe $cli $cwd $gate $ObserverHelper $null (Join-Path $case 'clean user dirs') -Utf8Streams
    Write-Report $launch (Join-Path $case 'launch.json')
    $diagnostic=Get-DemoStartupDiagnostic $launch
    $pass=Test-DemoPathRefusal $launch $diagnostic $Kind $ExpectedPath $cache $gate $ExpectedReason
    Write-Host "RootReleasePath/$Name pid=$($launch.pid) exit=$($launch.exit_code) passed=$pass"
    return @{ name=$Name; directory=$case; pid=$launch.pid; exit_code=$launch.exit_code; passed=$pass;
        expected_kind=$Kind; expected_path=$ExpectedPath; expected_reason=$ExpectedReason; diagnostic=$diagnostic }
}

function New-DemoDuplicateManifestFixture([string]$Bundle, [string]$Destination) {
    $original = Join-Path $Bundle 'component-runtime.json'
    $manifest = [IO.File]::ReadAllText($original) | ConvertFrom-Json
    $locales = @($manifest.runtime | Where-Object { $_.path -ceq 'locales/en-US.pak' })
    $duplicate = 'LOCALES\EN-US.PAK'
    if ($locales.Count -ne 1 -or ($manifest.runtime.path -contains $duplicate)) { throw 'Expected one original locale entry and no alias' }
    $prefix = [IO.Path]::GetFullPath($Bundle).TrimEnd('\') + '\'
    $dest = [IO.Path]::GetFullPath($Destination)
    if ($dest.StartsWith($prefix,[StringComparison]::OrdinalIgnoreCase) -or (Test-Path -LiteralPath $dest)) {
        throw 'Manifest refusal fixture must be a fresh independent bundle'
    }
    New-Item -ItemType Directory -Path $dest -ErrorAction Stop | Out-Null
    foreach ($entry in @($manifest.runtime) + @($manifest.host,$manifest.helper)) {
        $relative = if ($entry.PSObject.Properties['path']) { $entry.path } else { $entry.file_name }
        if ([IO.Path]::IsPathRooted($relative) -or ($relative -split '[/\\]') -contains '..') { throw 'Unexpected path in original staged package' }
        $src = [IO.Path]::GetFullPath((Join-Path $Bundle $relative))
        $copy = [IO.Path]::GetFullPath((Join-Path $dest $relative))
        if (-not $src.StartsWith($prefix,[StringComparison]::OrdinalIgnoreCase) -or
            -not $copy.StartsWith($dest.TrimEnd('\')+'\',[StringComparison]::OrdinalIgnoreCase) -or
            (([IO.File]::GetAttributes($src)) -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
            (Get-PocFileSha256 $src) -ine $entry.sha256) { throw 'Original staged bytes/layout do not match the manifest' }
        [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($copy))
        [IO.File]::Copy($src,$copy,$false)
        if ((Get-PocFileSha256 $copy) -ine $entry.sha256) { throw 'Independent copy differs from original' }
    }
    $manifest.runtime = @($manifest.runtime) + @(@{path=$duplicate; sha256=$locales[0].sha256})
    $output = Join-Path $dest 'component-runtime.json'
    $bytes = [Text.Encoding]::UTF8.GetBytes((ConvertTo-Json -InputObject $manifest -Depth 20))
    $stream = [IO.File]::Open($output,[IO.FileMode]::CreateNew)
    try { $stream.Write($bytes,0,$bytes.Length) } finally { $stream.Dispose() }
    return $output
}

function Invoke-DemoReleasePathGuards($Summary, [string]$Root, [string]$Bundle, [string]$Exe, [string]$Helper,
    [string]$HostInput, [string]$HelperInput, [string]$Cef) {
    $missing=Join-Path $Bundle 'absent worker.exe'
    if(Test-Path -LiteralPath $missing){throw 'Missing-helper fixture already exists'}
    $Summary.path_refusals+=Invoke-DemoPathRefusal $Root 'missing-explicit-helper' $Bundle $Exe $missing $Helper 'MissingFile' $missing
    if(-not $Summary.path_refusals[-1].passed){throw 'Missing-helper process did not fail closed'}

    $elsewhere=Join-Path $Root 'out-of-bundle worker.exe'
    [IO.File]::Copy($Helper,$elsewhere,$false)
    if((Get-PocFileSha256 $elsewhere) -cne (Get-PocFileSha256 $Helper)){throw 'The outside helper must be an actual byte-identical helper'}
    $Summary.path_refusals+=Invoke-DemoPathRefusal $Root 'outside-sibling-layout' $Bundle $Exe $elsewhere $elsewhere 'InvalidLayout' $elsewhere
    if(-not $Summary.path_refusals[-1].passed){throw 'Outside helper was not rejected'}

    # Only this newly packaged fixture is damaged. The positive bundle and SDK
    # stay byte-identical to their original manifests; no reparse point is written.
    $damaged=Join-Path $Root 'damaged resource bundle'
    if(Test-Path -LiteralPath $damaged){throw 'Damaged fixture must be newly staged'}
    & (Join-Path $PSScriptRoot 'package-component.ps1') -HostExecutable $HostInput -HelperExecutable $HelperInput -CefPath $Cef -OutputDirectory $damaged -HostBuildVersion 'root-public-demo-release-paths-corrupt-fixture' | Out-Null
    $file=Join-Path $damaged 'icudtl.dat'; $item=Get-Item -LiteralPath $file
    if(($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or $item.Length -lt 1){throw 'Refuse to mutate an aliased or empty resource fixture'}
    $before=Get-PocFileSha256 $file
    $anchor=Get-PocFileSha256 (Join-Path $Bundle 'icudtl.dat')
    if($before -cne $anchor){throw 'Fresh package resource differs before mutation'}
    $stream=[IO.File]::Open($file,[IO.FileMode]::Open,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
    try {
        $byte=$stream.ReadByte()
        if($byte -lt 0){throw 'Expected real owned resource byte'}
        $stream.Position=0
        $stream.WriteByte([byte]($byte -bxor 1))
    } finally { $stream.Dispose() }
    $after=Get-PocFileSha256 $file
    if($after -ceq $before -or (Get-PocFileSha256 (Join-Path $Bundle 'icudtl.dat')) -cne $anchor){throw 'Resource mutation or original isolation failed'}
    $badHelper=Join-Path $damaged ([IO.Path]::GetFileName($Helper))
    $badExe=Join-Path $damaged ([IO.Path]::GetFileName($Exe))
    $Summary.resource_mutation=@{ bundle=$damaged; path=$file; before_sha256=$before; after_sha256=$after; original_sha256=$anchor; offset=0; original_byte=$byte; fixture_byte=($byte -bxor 1) }
    $Summary.path_refusals+=Invoke-DemoPathRefusal $Root 'bad-resource-bytes' $damaged $badExe $badHelper $badHelper 'HashMismatch' $file
    if(-not $Summary.path_refusals[-1].passed){throw 'Damaged resource process did not fail closed'}

    $duplicate = Join-Path $Root 'duplicate manifest bundle'
    $duplicateManifest = New-DemoDuplicateManifestFixture $Bundle $duplicate
    $duplicateExe = Join-Path $duplicate ([IO.Path]::GetFileName($Exe))
    $duplicateHelper = Join-Path $duplicate ([IO.Path]::GetFileName($Helper))
    $Summary.path_refusals+=Invoke-DemoPathRefusal $Root 'duplicate-manifest-path' $duplicate $duplicateExe $duplicateHelper $duplicateHelper 'ManifestInvalid' $duplicateManifest 'duplicate runtime path'
    if(-not $Summary.path_refusals[-1].passed){throw 'Duplicate runtime manifest path did not fail closed'}
}
