param(
    [Parameter(Mandatory = $true, Position = 0)][string]$Executable,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$ExecutableArguments
)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot "common.ps1")

# Cargo supplies the actual app/test path, including --target/--target-dir.
$Executable = (Resolve-Path -LiteralPath $Executable).Path
$name = [IO.Path]::GetFileName($Executable)
if ($name -ieq 'gpui-cef-component-demo.exe') {
    # Cargo's build directory is not a release. Use the caller's explicitly
    # packaged, byte-identical host; never build a helper from inside a runner.
    $values = @($ExecutableArguments)
    $bundles = @()
    if ($values.Count % 2 -eq 0) {
        for ($i = 0; $i -lt $values.Count; $i += 2) {
            if ($values[$i] -ceq '--bundle') { $bundles += $values[$i + 1] }
        }
    }
    if ($bundles.Count -ne 1 -or $bundles[0] -notmatch '^(?:[A-Za-z]:[\\/]|[\\/]{2}[^\\/]+[\\/][^\\/]+)') {
        throw 'Component Cargo launch requires one explicit absolute --bundle and CLI pairs; use scripts/run.ps1 to build, package and run'
    }
    $staged = Join-Path $bundles[0] $name
    if (-not (Test-Path -LiteralPath $staged -PathType Leaf) -or
        -not (Test-Path -LiteralPath (Join-Path $bundles[0] 'component-runtime.json') -PathType Leaf)) {
        throw 'Component Cargo launch requires an existing packaged demo and manifest; use scripts/build.ps1'
    }
    if ((Get-PocFileSha256 $Executable) -cne (Get-PocFileSha256 $staged)) {
        throw 'Packaged demo differs from the current Cargo executable; rebuild and repackage'
    }
    $Executable = (Resolve-Path -LiteralPath $staged).Path
} elseif ($name -match '^(?:gpui[-_]cef[-_]poc(?:-[0-9a-f]+)?|gpui[-_]cef[-_]component[-_]demo-[0-9a-f]+|rust[-_]cef[-_]runtime(?:-[0-9a-f]+)?|rust[-_]cef[-_]gpui(?:-[0-9a-f]+)?|(?:deployment|lifecycle|packaging|probe|process_timeout|profile_paths|profile_registry|subprocess|deployment_distribution)-[0-9a-f]+)\.exe$') {
    # Legacy host and CEF-linked Cargo test/binary runners keep local loader staging.
    # In particular the upstream export-cef-dir stays independent of CEF.
    $cef = Resolve-CefPath ""
    $destDir = Split-Path -Parent $Executable
    $dstArchive = Join-Path $destDir 'archive.json'
    $srcArchive = Join-Path $cef 'archive.json'
    $dstLibCef = Join-Path $destDir 'libcef.dll'
    $srcLibCef = Join-Path $cef 'libcef.dll'
    $needsSync = (-not (Test-Path -LiteralPath $dstArchive -PathType Leaf)) -or
        (-not (Test-Path -LiteralPath $dstLibCef -PathType Leaf)) -or
        ((Get-Item -LiteralPath $dstLibCef).Length -ne (Get-Item -LiteralPath $srcLibCef).Length) -or
        ((Get-Content -Raw -LiteralPath $dstArchive) -cne (Get-Content -Raw -LiteralPath $srcArchive))
    if ($needsSync) {
        foreach ($obsolete in @('libEGL.dll', 'libGLESv2.dll', 'dxil.dll')) {
            $oldPath = Join-Path $destDir $obsolete
            if (Test-Path -LiteralPath $oldPath -PathType Leaf) {
                Remove-Item -LiteralPath $oldPath -Force -ErrorAction SilentlyContinue
            }
        }
        Copy-CefRuntime $cef $destDir
    }
}
# Windows PowerShell 5.1's native invocation drops empty arguments and quotes.
# Build the Windows argv command line explicitly instead of using that path.
$quotedArguments = foreach ($argument in $ExecutableArguments) {
    $escaped = [regex]::Replace($argument, '(\\*)"', '$1$1\"')
    '"' + [regex]::Replace($escaped, '(\\+)$', '$1$1') + '"'
}
$startInfo = New-Object System.Diagnostics.ProcessStartInfo
$startInfo.FileName = $Executable
$startInfo.Arguments = $quotedArguments -join " "
$startInfo.UseShellExecute = $false
$process = [System.Diagnostics.Process]::Start($startInfo)
$process.WaitForExit()
$exitCode = $process.ExitCode
$process.Dispose()
exit $exitCode
