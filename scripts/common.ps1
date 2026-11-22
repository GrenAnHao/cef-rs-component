# Shared helpers for the Windows build scripts. Dot-source this file.
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$script:PocTarget = "x86_64-pc-windows-msvc"
$script:PocRoot = Split-Path -Parent $PSScriptRoot
$script:PocRevision = "7b6bf84e76e919790bd61df077e29dc4132eb5e2"
$script:PocCefVersion = "156.0.31"

function Invoke-Checked {
    param([string]$Program, [string[]]$Arguments)
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Program failed with exit code $LASTEXITCODE" }
}

# depot_tools ships ninja as a Python wrapper that only re-executes a real
# binary found elsewhere on PATH. When that lookup fails it exits non-zero
# without a message, and CMake reports the useless "unknown error". Probe
# --version so a broken shim is skipped instead of poisoning the build.
function Resolve-Ninja {
    $candidates = @()
    $command = Get-Command ninja -ErrorAction SilentlyContinue
    if ($command) { $candidates += $command.Source }
    foreach ($dir in ($env:PATH -split ';')) {
        if ([string]::IsNullOrWhiteSpace($dir)) { continue }
        $exe = Join-Path $dir "ninja.exe"
        if (Test-Path $exe) { $candidates += $exe }
    }
    foreach ($candidate in ($candidates | Select-Object -Unique)) {
        # Run through cmd so the shim's stderr cannot surface as a PowerShell
        # NativeCommandError under $ErrorActionPreference = 'Stop'.
        $version = (& cmd.exe /c "`"$candidate`" --version 2>nul" | Out-String).Trim()
        if ($LASTEXITCODE -eq 0 -and $version -match '^\d+\.\d+') { return $candidate }
    }
    throw "No working Ninja found. The ninja shim in depot_tools is a wrapper that fails silently. Install a real ninja (for example 'pip install ninja') so that 'ninja --version' prints a version number."
}

# A plain terminal has no MSVC environment. Import it from vcvars64.bat so CMake
# and the linker see cl.exe with the Windows SDK include and library paths.
function Import-MsvcEnvironment {
    if (Get-Command cl.exe -ErrorAction SilentlyContinue) { return }
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) {
        throw "cl.exe is not in PATH and vswhere.exe was not found. Install Visual Studio Build Tools with the C++ workload, or open a Developer PowerShell for VS."
    }
    $vcvars = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -find "VC\Auxiliary\Build\vcvars64.bat"
    if (-not $vcvars -or -not (Test-Path $vcvars)) {
        throw "cl.exe is not in PATH and vcvars64.bat was not found. Install the MSVC x64 C++ build tools."
    }
    $lines = & cmd.exe /c "`"$vcvars`" >nul && set"
    if ($LASTEXITCODE -ne 0) { throw "Importing the MSVC environment from $vcvars failed." }
    foreach ($line in $lines) {
        if ($line -match '^([^=]+)=(.*)$') {
            Set-Item -Path ("env:" + $Matches[1]) -Value $Matches[2]
        }
    }
    if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) {
        throw "Imported $vcvars but cl.exe is still unavailable."
    }
}

# CEF headers contain UTF-8 comments. With a GBK code page (936) cl.exe emits
# C4819, and CEF compiles the wrapper with /WX, so that warning becomes a hard
# error. /utf-8 tells cl.exe the sources are UTF-8.
function Set-Utf8CompilerFlag {
    if ($env:CL -notmatch '/utf-8') { $env:CL = ([string]$env:CL + " /utf-8").Trim() }
}

# Windows PowerShell launched by Cargo may not auto-discover Get-FileHash.
# Keep hashes independent of module discovery and stream potentially large DLLs.
function Get-PocFileSha256([string]$File) {
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        $stream = [IO.File]::OpenRead($File)
        try { return [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '') }
        finally { $stream.Dispose() }
    } finally { $sha.Dispose() }
}

function Resolve-CefPath {
    param([string]$Configured)
    if ([string]::IsNullOrWhiteSpace($Configured)) {
        if ($env:CEF_PATH) { $Configured = $env:CEF_PATH }
        else { $Configured = Join-Path $PocRoot ".cef\${PocCefVersion}_windows_x86_64" }
    }
    $resolved = [IO.Path]::GetFullPath($Configured)
    # Match cef-dll-sys: a versioned download takes precedence over a flat export.
    $versioned = Join-Path $resolved $PocCefVersion
    if (Test-Path $versioned) {
        $resolved = Join-Path $versioned "cef_windows_x86_64"
    }
    if (-not (Test-Path $resolved)) {
        throw "CEF runtime not found at $resolved. Run scripts\run.ps1 once (it downloads it), or pass -CefPath."
    }
    foreach ($file in @("archive.json", "libcef.dll", "icudtl.dat", "CMakeLists.txt")) {
        if (-not (Test-Path (Join-Path $resolved $file))) {
            throw "CEF_PATH must be the cef-rs exported (flattened) $PocCefVersion directory. Missing $file in $resolved"
        }
    }
    if (-not (Test-Path (Join-Path $resolved "locales"))) { throw "CEF locales are missing in $resolved" }
    $archive = Get-Content -Raw (Join-Path $resolved "archive.json") | ConvertFrom-Json
    $cefRegex = "$([regex]::Escape($PocCefVersion)).*windows64"
    if ($archive.name -notmatch $cefRegex) {
        throw "CEF archive.json is not for CEF $PocCefVersion Windows x64. Do not mix binary and binding versions."
    }
    return $resolved
}

function Copy-CefRuntime {
    param([string]$CefPath, [string]$Destination)
    # CEF export flattens Release + Resources. Child processes need the same
    # DLLs, resource packs and locales beside the executable as the parent.
    Get-ChildItem -LiteralPath $CefPath -File | Where-Object {
        $_.Extension -in @(".dll", ".pak", ".dat", ".bin", ".json") -or $_.Name -match "LICENSE|CREDITS"
    } | Copy-Item -Destination $Destination -Force
    Copy-Item (Join-Path $CefPath "locales") $Destination -Recurse -Force
}

# cef-dll-sys configures CMake with CMAKE_OBJECT_PATH_MAX=500, which turns off
# CMake's object-name shortening. A normal target\ tree under a deep checkout
# pushes the /Fo path past MAX_PATH and cl.exe fails with C1083 / "无法打开编译器
# 生成的文件: ''". Keep cargo intermediates and packaged outputs on a short
# path under E:\cargo-target\gpui-cef-poc. Override cargo intermediates with
# -TargetDir or POC_TARGET_DIR; an explicit path, including C: or D:, is honored.
# Bundles, validation evidence and test scratch dirs use the layout root
# (optional POC_OUTPUT_ROOT), not the project tree and not the user %TEMP%.
$script:PocDefaultLayoutRoot = if (Test-Path 'E:\cargo-target') {
    'E:\cargo-target\gpui-cef-poc'
} else {
    Join-Path ([IO.Path]::GetPathRoot($PocRoot)) 'cargo-target\cef-rs-component'
}

function Get-PocFullPath([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path)) { throw 'Path is empty' }
    return [IO.Path]::GetFullPath($Path.Trim())
}

function Resolve-PocLayoutRoot {
    param([string]$Configured)
    if (-not [string]::IsNullOrWhiteSpace($Configured)) { return Get-PocFullPath $Configured }
    if ($env:POC_OUTPUT_ROOT) { return Get-PocFullPath $env:POC_OUTPUT_ROOT }
    return Get-PocFullPath $script:PocDefaultLayoutRoot
}

function Resolve-TargetDir {
    param([string]$Configured)
    if (-not [string]::IsNullOrWhiteSpace($Configured)) { return Get-PocFullPath $Configured }
    if ($env:POC_TARGET_DIR) { return Get-PocFullPath $env:POC_TARGET_DIR }
    return Get-PocFullPath (Join-Path $script:PocDefaultLayoutRoot 'cargo')
}

function Resolve-BundleDir {
    param([string]$Profile)
    if ([string]::IsNullOrWhiteSpace($Profile)) { throw 'Profile is empty' }
    return Join-Path (Resolve-PocLayoutRoot) "bundle-$Profile"
}

function Resolve-ComponentBundlesRoot {
    param([string]$StagingRoot)
    if (-not [string]::IsNullOrWhiteSpace($StagingRoot)) { return Get-PocFullPath $StagingRoot }
    return Join-Path (Resolve-PocLayoutRoot) 'component-bundles'
}

function Resolve-ValidationDir {
    param([string]$Stage, [string]$Token)
    $directory = Join-Path (Resolve-PocLayoutRoot) 'component-validation'
    if (-not [string]::IsNullOrWhiteSpace($Stage)) { $directory = Join-Path $directory $Stage.ToLowerInvariant() }
    if (-not [string]::IsNullOrWhiteSpace($Token)) { $directory = Join-Path $directory $Token }
    return $directory
}

function Resolve-PocTempDir {
    param([string]$Name)
    $directory = Join-Path (Resolve-PocLayoutRoot) 'tmp'
    if (-not [string]::IsNullOrWhiteSpace($Name)) { $directory = Join-Path $directory $Name }
    return $directory
}

function Resolve-CheckOutputDir {
    return Join-Path (Resolve-PocLayoutRoot) 'check'
}

function New-PocDirectory([string]$Path) {
    $full = Get-PocFullPath $Path
    New-Item -ItemType Directory -Force -Path $full | Out-Null
    return $full
}
