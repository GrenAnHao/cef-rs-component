#Requires -Version 7.2
# Run the actual workspace suite including the opt-in legacy bin. The caller
# supplies a new short target and a disposable/verified workspace copy if the
# existing workspace's test-output paths must not be touched.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$TargetDir,
    [Parameter(Mandatory)][string]$EvidenceDirectory,
    [string]$CefPath,
    [ValidateRange(1,16)][int]$Jobs = 4
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'common.ps1')

function Write-TestReport($Value, [string]$Path) {
    [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
}

function Invoke-TestCargo([string[]]$Arguments, [string]$Log) {
    $PSNativeCommandUseErrorActionPreference = $false
    $file = [IO.File]::Open($Log, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    $writer = [IO.StreamWriter]::new($file, [Text.UTF8Encoding]::new($false))
    try {
        $writer.AutoFlush = $true
        & cargo @Arguments 2>&1 | ForEach-Object {
            $writer.WriteLine([string]$_)
            Write-Host $_
        }
        return $LASTEXITCODE
    } finally { $writer.Dispose() }
}

function Get-NonAliasedDirectory([string]$Path) {
    $full = [IO.Path]::TrimEndingDirectorySeparator([IO.Path]::GetFullPath($Path))
    for ($dir = [IO.DirectoryInfo]::new($full); $null -ne $dir; $dir = $dir.Parent) {
        if ([IO.File]::Exists($dir.FullName)) { throw "Expected a directory: $($dir.FullName)" }
        $attributes = $dir.Attributes
        if ([int]$attributes -ne -1 -and ($attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "Reparse paths are not accepted for workspace validation: $($dir.FullName)"
        }
    }
    return $full
}

function Test-WithinDirectory([string]$Path, [string]$Directory) {
    return $Path.Equals($Directory, [StringComparison]::OrdinalIgnoreCase) -or
        $Path.StartsWith($Directory.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)
}

function Assert-SeparateDirectories([string]$First, [string]$Second) {
    if ((Test-WithinDirectory $First $Second) -or (Test-WithinDirectory $Second $First)) {
        throw "Workspace validation directories must not overlap: $First and $Second"
    }
}

function Get-HarnessKey([string]$Package, $Target) {
    "$Package|$($Target.kind -join ',')|$($Target.name)"
}

function Get-WorkspaceTestPlan($Metadata) {
    $expected = @(); $excluded = @()
    foreach ($p in $Metadata.packages) {
        if ($p.id -cnotin $Metadata.workspace_members) { continue }
        $nodes = @($Metadata.resolve.nodes | Where-Object { $_.id -ceq $p.id })
        if ($nodes.Count -ne 1) { throw "No unique resolved features for $($p.name)" }
        foreach ($t in $p.targets) {
            if (-not $t.test -or @($t.kind | Where-Object { $_ -in @('lib','bin','test') }).Count -eq 0) { continue }
            $key = Get-HarnessKey $p.id $t
            $requires = $t.PSObject.Properties['required-features']
            $missing = if ($null -ne $requires) { @($requires.Value | Where-Object { $_ -cnotin $nodes[0].features }) } else { @() }
            if (@($missing).Count -gt 0) {
                $excluded += @{ key = $key; missing_features = @($missing) }
            } else {
                $expected += $key
            }
        }
    }
    if ($expected.Count -eq 0 -or @($expected | Sort-Object -Unique).Count -ne $expected.Count) { throw 'Absent or ambiguous workspace harness plan' }
    if (@($expected | Where-Object { $_ -cmatch '\|bin\|gpui-cef-poc$' }).Count -ne 1 -or
        @($expected | Where-Object { $_ -cmatch '\|bin\|gpui-cef-component-demo$' }).Count -ne 1) {
        throw 'Workspace plan must include the actual legacy and default root bins'
    }
    return @{ enabled = $expected; opt_in_not_enabled = $excluded }
}

# These paths must be new; keep failed outputs rather than removing or
# replacing them. Do not hand the original development target to staging.
$target = Get-NonAliasedDirectory $TargetDir
$evidence = Get-NonAliasedDirectory $EvidenceDirectory
$cef = Get-NonAliasedDirectory (Resolve-CefPath $CefPath)
Assert-SeparateDirectories $target $evidence
Assert-SeparateDirectories $target $cef
Assert-SeparateDirectories $evidence $cef
if ((Test-Path -LiteralPath $target) -or (Test-Path -LiteralPath $evidence)) {
    throw 'TargetDir and EvidenceDirectory must be unused paths'
}
New-Item -ItemType Directory -Path $evidence -ErrorAction Stop | Out-Null
$summary = @{ schema=2; scope='actual-workspace-test-with-legacy-demo'; workspace=$PocRoot; target=$target; cef=$cef; plan=$null;
    metadata_exit=$null; build_exit=$null; test_exit=$null; receipt_exit=$null; staged=$false; success=$false;
    error=$null; physical_input=$false; profile_debug_symbols=$false }
$fields = @('PATH','CL','CEF_PATH','CARGO_TARGET_DIR','CARGO_INCREMENTAL','CARGO_PROFILE_DEV_DEBUG','CARGO_PROFILE_TEST_DEBUG','PYTHONIOENCODING')
$old = @{}; foreach ($name in $fields) { $old[$name] = [Environment]::GetEnvironmentVariable($name) }
Push-Location $PocRoot
try {
    $ninja = Resolve-Ninja
    $env:PATH = (Split-Path -Parent $ninja) + ';' + $env:PATH
    Import-MsvcEnvironment
    Set-Utf8CompilerFlag
    if ($env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER) { throw 'Remove the inherited Cargo runner override to use the repository runner' }
    $python = (Get-Command python.exe -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
    $env:CEF_PATH = $cef; $env:CARGO_TARGET_DIR = $target
    # Size control only; preserve test assertions and the locked graph.
    $env:CARGO_INCREMENTAL = '0'; $env:CARGO_PROFILE_DEV_DEBUG = '0'; $env:CARGO_PROFILE_TEST_DEBUG = '0'
    $PSNativeCommandUseErrorActionPreference = $false
    $metadataArguments = @('metadata','--locked','--offline','--format-version','1','--filter-platform',$PocTarget,'--features','legacy-demo')
    $argsForTest = @('test','--locked','--offline','--target',$PocTarget,'--workspace','--features','legacy-demo','--jobs',$Jobs)
    $buildArguments = $argsForTest + @('--no-run','--message-format=json-render-diagnostics')
    $summary.arguments = @{ metadata=$metadataArguments; build=$buildArguments; test=$argsForTest }
    Write-TestReport $summary.arguments (Join-Path $evidence 'invocation.json')
    $metadataLog = Join-Path $evidence 'metadata.json'
    & cargo @metadataArguments > $metadataLog 2> (Join-Path $evidence 'metadata.stderr.log')
    $summary.metadata_exit = $LASTEXITCODE
    if ($summary.metadata_exit -ne 0) { throw 'Locked workspace metadata failed' }
    $plan = Get-WorkspaceTestPlan (Get-Content -LiteralPath $metadataLog -Raw | ConvertFrom-Json)
    $summary.plan = $plan
    Write-TestReport $plan (Join-Path $evidence 'harness-plan.json')
    $buildLog = Join-Path $evidence 'build.log'
    $summary.build_exit = Invoke-TestCargo $buildArguments $buildLog
    if ($summary.build_exit -ne 0) { throw "Actual workspace harness build failed: $($summary.build_exit)" }
    $emitted = @(Get-Content -LiteralPath $buildLog | Where-Object { $_.StartsWith('{') } | ForEach-Object { $_ | ConvertFrom-Json } |
        Where-Object { $_.reason -eq 'compiler-artifact' -and $null -ne $_.executable })
    $builtTests = @($emitted | Where-Object { $_.profile.test -or 'test' -in $_.target.kind })
    $keys = @($builtTests | ForEach-Object { Get-HarnessKey $_.package_id $_.target })
    if ((($keys | Sort-Object) -join "`n") -cne (($plan.enabled | Sort-Object) -join "`n")) { throw 'Cargo executables do not match the whole workspace harness plan' }

    # Loader material is confined to directories of actual Cargo executables
    # in the new target, including the real helper used by harness=false.
    $directories = @()
    $artifacts = foreach ($entry in $emitted) {
        $exe = (Resolve-Path -LiteralPath $entry.executable).ProviderPath
        $dir = Get-NonAliasedDirectory (Split-Path -Parent $exe)
        if (-not (Test-WithinDirectory $dir $target)) { throw "Cargo executable escaped the owned target: $exe" }
        $directories += $dir
        @{ key = (Get-HarnessKey $entry.package_id $entry.target); executable = $exe;
           sha256 = (Get-FileHash -LiteralPath $exe).Hash; profile_test = $entry.profile.test }
    }
    foreach ($dir in @($directories | Sort-Object -Unique)) { Copy-CefRuntime $cef $dir }
    Write-TestReport @($artifacts) (Join-Path $evidence 'test-artifacts.json')
    $summary.staged = $true
    # No filters, no feature weakening and no manual substitution for cargo test.
    $summary.test_exit = Invoke-TestCargo $argsForTest (Join-Path $evidence 'tests.log')
    if ($summary.test_exit -ne 0) { throw "Actual workspace test command failed: $($summary.test_exit)" }
    foreach ($a in $artifacts) {
        if ((Get-FileHash -LiteralPath $a.executable).Hash -cne $a.sha256) { throw "Test executable changed between staging and execution: $($a.executable)" }
    }
    $env:PYTHONIOENCODING = 'utf-8'
    & $python -B (Join-Path $PSScriptRoot 'verify-component-workspace.py') $evidence $PocRoot > (Join-Path $evidence 'receipt.json') 2> (Join-Path $evidence 'receipt.stderr.log')
    $summary.receipt_exit = $LASTEXITCODE
    if ($summary.receipt_exit -ne 0) { throw 'Workspace test receipts do not match the enabled harness and doctest plan' }
    $summary.success = $true
} catch {
    $summary.error = $_.ToString()
    Write-Host $summary.error
} finally {
    foreach ($name in $fields) { [Environment]::SetEnvironmentVariable($name, $old[$name]) }
    Pop-Location
    Write-TestReport $summary (Join-Path $evidence 'result.json')
    Write-Host "Evidence: $evidence"
}
if (-not $summary.success) { exit 1 }
