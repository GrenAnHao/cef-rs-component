[CmdletBinding()]
param(
    [ValidateSet('Threaded', 'Integrated')][string]$Mode = 'Threaded',
    [ValidateSet('D3d11', 'Software')][string]$Backend = 'D3d11',
    [switch]$NativeHwnd,
    [switch]$DoNotDeElevate,
    [switch]$SelfTestOsr,
    [switch]$SelfTestOsrDnd,
    [string]$Url = 'https://cn.bing.com/',
    [ValidateRange(0, 60)][int]$SmokeSeconds = 0,
    [switch]$Release,
    [switch]$BuildOnly,
    [string]$CefPath = '',
    [string]$TargetDir = ''
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

    Write-Host 'Building bing_cn example and rust-cef-helper...'
    Invoke-Checked 'cargo' ($cargoArgs + @('-p', 'gpui-cef-poc', '--example', 'bing_cn'))
    Invoke-Checked 'cargo' ($cargoArgs + @('-p', 'rust-cef-helper', '--bin', 'rust-cef-helper'))

    $binDir = Join-Path $target "$PocTarget\$profile"
    $hostExe = Join-Path $binDir 'examples\bing_cn.exe'
    $helperExe = Join-Path $binDir 'rust-cef-helper.exe'
    $bundleDir = Join-Path (Resolve-ComponentBundlesRoot '') "bing-cn-example-$profile"

    $packed = & (Join-Path $PSScriptRoot 'package-component.ps1') `
        -HostExecutable $hostExe `
        -HelperExecutable $helperExe `
        -CefPath $cef `
        -OutputDirectory $bundleDir `
        -HostBuildVersion "bing-cn-$profile"

    Write-Host "Packaged bing_cn example: $packed"
    if ($BuildOnly) { return }

    $cacheRoot = Resolve-PocTempDir "bing-cn-cache-$profile"
    $stagedExe = Join-Path $packed 'bing_cn.exe'
    $cliArgs = @(
        '--url', $Url,
        '--mode', $Mode,
        '--backend', $Backend,
        '--bundle', $packed,
        '--helper', (Join-Path $packed 'rust-cef-helper.exe'),
        '--cache-root', $cacheRoot
    )
    if ($NativeHwnd) { $cliArgs += '--native-hwnd' }
    if ($SelfTestOsr) { $cliArgs += '--self-test-osr' }
    if ($SelfTestOsrDnd) { $cliArgs += '--self-test-osr-dnd' }
    if ($SmokeSeconds -gt 0) { $cliArgs += @('--smoke-ms', [string]($SmokeSeconds * 1000)) }

    $isElevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator
    )
    if ($DoNotDeElevate -or $isElevated) {
        if ($isElevated -and -not $DoNotDeElevate) {
            Write-Warning 'Elevated (Administrator) PowerShell detected; passing --do-not-de-elevate so CEF 156 does not exit with code 38 (CEF_RESULT_CODE_NORMAL_EXIT_AUTO_DE_ELEVATED). Running from a standard non-elevated terminal is recommended.'
        }
        $cliArgs += '--do-not-de-elevate'
    }

    Invoke-Checked $stagedExe $cliArgs
} finally {
    Set-Location $oldLoc
    $env:CEF_PATH = $oldCef
    $env:CARGO_TARGET_DIR = $oldTarget
    $env:CL = $oldCl
    $env:PATH = $oldPath
}
