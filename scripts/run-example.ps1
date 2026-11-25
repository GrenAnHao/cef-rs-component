[CmdletBinding()]
param(
    [ValidateSet('Threaded', 'Integrated')][string]$Mode = 'Threaded',
    [ValidateSet('D3d11', 'Software')][string]$Backend = 'D3d11',
    [switch]$NativeHwnd,
    [switch]$ContextMenu,
    [string]$Url = '',
    [ValidateRange(0, 30)][int]$SmokeSeconds = 0,
    [switch]$Release,
    [switch]$BuildOnly,
    [string]$CefPath = '',
    [string]$TargetDir = '',
    [string]$StagingRoot = ''
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'common.ps1')

$oldCef = $env:CEF_PATH
$oldTarget = $env:CARGO_TARGET_DIR
$oldCl = $env:CL
$oldPath = $env:PATH
$oldLoc = Get-Location

try {
    Set-Location $PocRoot
    $ninja = Resolve-Ninja
    $env:PATH = (Split-Path -Parent $ninja) + ';' + $env:PATH
    Import-MsvcEnvironment
    Set-Utf8CompilerFlag

    if ([string]::IsNullOrWhiteSpace($CefPath)) {
        if ($env:CEF_PATH) { $CefPath = $env:CEF_PATH }
        else { $CefPath = Join-Path $PocRoot ".cef\${PocCefVersion}_windows_x86_64" }
    }
    $cef = Resolve-CefPath ([IO.Path]::GetFullPath($CefPath))
    $target = Resolve-TargetDir $TargetDir
    $env:CEF_PATH = $cef
    $env:CARGO_TARGET_DIR = $target

    $profile = if ($Release) { 'release' } else { 'debug' }
    $cargoArgs = @('build', '--locked', '--target', $PocTarget)
    if ($Release) { $cargoArgs += '--release' }

    Write-Host 'Building gpui-cef-example and rust-cef-helper...'
    Invoke-Checked 'cargo' ($cargoArgs + @('-p', 'gpui-cef-poc', '--features', 'component-demo', '--bin', 'gpui-cef-example'))
    Invoke-Checked 'cargo' ($cargoArgs + @('-p', 'rust-cef-helper', '--bin', 'rust-cef-helper'))

    $binDir = Join-Path $target "$PocTarget\$profile"
    $hostExe = Join-Path $binDir 'gpui-cef-example.exe'
    $helperExe = Join-Path $binDir 'rust-cef-helper.exe'
    $resolvedStaging = if (-not [string]::IsNullOrWhiteSpace($StagingRoot) -and -not [IO.Path]::IsPathRooted($StagingRoot)) {
        [IO.Path]::GetFullPath((Join-Path $PocRoot $StagingRoot))
    } else {
        $StagingRoot
    }
    $bundleDir = Join-Path (Resolve-ComponentBundlesRoot $resolvedStaging) "cef-example-$profile"

    $packed = & (Join-Path $PSScriptRoot 'package-component.ps1') `
        -HostExecutable $hostExe `
        -HelperExecutable $helperExe `
        -CefPath $cef `
        -OutputDirectory $bundleDir `
        -HostBuildVersion "cef-example-$profile"

    Write-Host "Packaged gpui-cef-example: $packed"
    if ($BuildOnly) {
        Write-Output $packed
        return
    }

    $cacheRoot = Resolve-PocTempDir "cef-example-cache-$profile"
    $stagedExe = Join-Path $packed 'gpui-cef-example.exe'
    $cliArgs = @(
        '--bundle', $packed,
        '--helper', (Join-Path $packed 'rust-cef-helper.exe'),
        '--cache-root', $cacheRoot,
        '--mode', $Mode,
        '--backend', $Backend
    )
    if ($NativeHwnd) { $cliArgs += @('--host-mode', 'native-hwnd') }
    if ($ContextMenu) { $cliArgs += @('--context-menu', 'true') }
    if (-not [string]::IsNullOrWhiteSpace($Url)) { $cliArgs += @('--url', $Url) }
    if ($SmokeSeconds -gt 0) { $cliArgs += @('--smoke-ms', [string]($SmokeSeconds * 1000)) }

    Invoke-Checked $stagedExe $cliArgs
} finally {
    Set-Location $oldLoc
    $env:CEF_PATH = $oldCef
    $env:CARGO_TARGET_DIR = $oldTarget
    $env:CL = $oldCl
    $env:PATH = $oldPath
}
