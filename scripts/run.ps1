[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$BuildOnly,
    [switch]$Legacy,
    [ValidateRange(0, 300)][int]$SmokeSeconds = 0,
    [ValidateSet('Integrated','Threaded')][string]$Mode = 'Threaded',
    [ValidateSet('Software','D3d11')][string]$Backend = 'Software',
    [string]$CefPath = "",
    [string]$TargetDir = "",
    [string]$StagingRoot = "",
    [string]$CacheRoot = ""
)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot "common.ps1")

$OldCefPath = $env:CEF_PATH
$OldSmoke = $env:POC_EXIT_AFTER_MS
$OldLocation = Get-Location
$OldPath = $env:PATH

function Export-CefRuntime {
    param([string]$Destination)
    $source = Join-Path $PocRoot ".deps\cef-rs"
    if (-not (Test-Path $source)) {
        New-Item -ItemType Directory -Force -Path (Split-Path $source) | Out-Null
        Invoke-Checked "git" @("init", $source)
        Invoke-Checked "git" @("-C", $source, "remote", "add", "origin", "https://github.com/tauri-apps/cef-rs")
        Invoke-Checked "git" @("-C", $source, "fetch", "--depth", "1", "origin", $PocRevision)
        Invoke-Checked "git" @("-C", $source, "checkout", "--detach", "FETCH_HEAD")
    }
    $actual = (& git -C $source rev-parse HEAD 2>$null | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $actual -ne $PocRevision) {
        Invoke-Checked "git" @("-C", $source, "fetch", "--depth", "1", "origin", $PocRevision)
        Invoke-Checked "git" @("-C", $source, "checkout", "--detach", "FETCH_HEAD")
        $actual = (& git -C $source rev-parse HEAD | Out-String).Trim()
    }
    if ($LASTEXITCODE -ne 0 -or $actual -ne $PocRevision) {
        throw "The cached cef-rs checkout is not at the pinned revision. Use a fresh project copy."
    }
    New-Item -ItemType Directory -Force -Path (Split-Path $Destination) | Out-Null
    # The upstream exporter can delete fixed sibling extraction paths.
    # Isolate it in a brand-new parent, then move only its completed output.
    New-Item -ItemType Directory -Force -Path (Join-Path $PocRoot ".cef") | Out-Null
    $stage = Join-Path $PocRoot (".cef\download-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $stage | Out-Null
    $exported = Join-Path $stage "runtime"
    Invoke-Checked "cargo" @("run", "--manifest-path", (Join-Path $source "Cargo.toml"), "-p", "export-cef-dir", "--", "--target", $PocTarget, "--version", $PocCefVersion, $exported)
    if (Test-Path $Destination) { throw "CEF destination appeared while downloading; refusing to overwrite it." }
    Move-Item -LiteralPath $exported -Destination $Destination
}

try {
    if ($Legacy -and ($StagingRoot -or $CacheRoot -or $PSBoundParameters.ContainsKey('Mode') -or $PSBoundParameters.ContainsKey('Backend'))) {
        throw 'Legacy retains its original staging, cache, mode and backend behavior'
    }
    if (-not $Legacy -and $SmokeSeconds -gt 30) { throw 'Component smoke seconds must be in 0..30' }
    if ($env:OS -ne "Windows_NT" -or -not [Environment]::Is64BitProcess) {
        throw "Run this script in x64 Windows PowerShell or PowerShell 7 on Windows."
    }
    Set-Location $PocRoot
    foreach ($tool in @("cargo", "rustc", "git", "cmake")) {
        if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
            throw "Missing $tool. Install Rust (x86_64-pc-windows-msvc), CMake, Ninja and Git."
        }
    }
    $ninja = Resolve-Ninja
    $env:PATH = (Split-Path -Parent $ninja) + ";" + $env:PATH

    if ([string]::IsNullOrWhiteSpace($CefPath)) {
        if ($env:CEF_PATH) { $CefPath = $env:CEF_PATH }
        else { $CefPath = Join-Path $PocRoot ".cef\${PocCefVersion}_windows_x86_64" }
    }
    $CefPath = [IO.Path]::GetFullPath($CefPath)
    if (-not (Test-Path $CefPath)) { Export-CefRuntime $CefPath }
    $CefPath = Resolve-CefPath $CefPath

    $buildOptions = @{ CefPath = $CefPath }
    if ($Release) { $buildOptions.Release = $true }
    if ($Legacy) { $buildOptions.Legacy = $true }
    if ($StagingRoot) { $buildOptions.StagingRoot = $StagingRoot }
    if (-not [string]::IsNullOrWhiteSpace($TargetDir)) { $buildOptions.TargetDir = $TargetDir }
    $bundle = & (Join-Path $PSScriptRoot "build.ps1") @buildOptions
    Write-Host "Bundle: $bundle"

    if (-not $BuildOnly) {
        if ($Legacy) {
            if ($SmokeSeconds -gt 0) { $env:POC_EXIT_AFTER_MS = [string]($SmokeSeconds * 1000) }
            else { $env:POC_EXIT_AFTER_MS = $null }
            Invoke-Checked (Join-Path $bundle 'gpui-cef-poc.exe') @()
        } else {
            $root = if ($CacheRoot) { [IO.Path]::GetFullPath($CacheRoot) } else { Join-Path (Split-Path -Parent $bundle) 'cache' }
            # Match the Rust CLI's canonical values even on case-insensitive PS5 validation.
            $cliMode = if ($Mode -ieq 'Integrated') { 'Integrated' } else { 'Threaded' }
            $cliBackend = if ($Backend -ieq 'D3d11') { 'D3d11' } else { 'Software' }
            $arguments = @('--bundle',$bundle,'--helper',(Join-Path $bundle 'rust-cef-helper.exe'),'--cache-root',$root,'--mode',$cliMode,'--backend',$cliBackend)
            if ($SmokeSeconds -gt 0) { $arguments += @('--smoke-ms',[string]($SmokeSeconds * 1000)) }
            Invoke-Checked (Join-Path $bundle 'gpui-cef-component-demo.exe') $arguments
        }
    }
} finally {
    $env:CEF_PATH = $OldCefPath
    if ($Legacy) { $env:POC_EXIT_AFTER_MS = $OldSmoke }
    $env:PATH = $OldPath
    Set-Location $OldLocation
}
