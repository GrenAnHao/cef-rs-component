[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$Legacy,
    [string]$CefPath = "",
    [string]$TargetDir = "",
    [string]$StagingRoot = ""
)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot "common.ps1")

$OldCefPath = $env:CEF_PATH
$OldTargetDir = $env:CARGO_TARGET_DIR
$OldCl = $env:CL
$OldLocation = Get-Location
$OldPath = $env:PATH

function Build-ComponentArtifact([string]$Package, [string]$Binary, [string[]]$CargoArgs, [string]$Log) {
    # PS 5.1 must not turn ordinary compiler stderr into a terminating error.
    $ErrorActionPreference = 'Continue'
    $lines = @(& cargo @CargoArgs -p $Package --bin $Binary --message-format=json-render-diagnostics 2>&1 |
        ForEach-Object { Write-Host $_; $_ })
    $code = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    # File I/O is outside the relaxed stderr policy and must succeed before packaging.
    [IO.File]::WriteAllLines($Log, ([string[]]@($lines | ForEach-Object { [string]$_ })), [Text.UTF8Encoding]::new($false))
    if ($code -ne 0) { throw "Build $Binary failed with exit code $code; see $Log" }
    $outputs = @($lines | Where-Object { $_ -is [string] -and $_.StartsWith('{') } | ForEach-Object { $_ | ConvertFrom-Json } |
        Where-Object { $_.reason -eq 'compiler-artifact' -and $_.target.name -ceq $Binary -and $null -ne $_.executable })
    if ($outputs.Count -ne 1) { throw "Expected exactly one Cargo executable for $Binary" }
    return (Resolve-Path -LiteralPath $outputs[0].executable).ProviderPath
}

try {
    if ($Legacy -and $StagingRoot) { throw 'StagingRoot is for the packaged component; the legacy fixtures retain their original bundle path' }
    if ($env:OS -ne "Windows_NT" -or -not [Environment]::Is64BitProcess) {
        throw "Run this script in x64 Windows PowerShell or PowerShell 7 on Windows."
    }
    Set-Location $PocRoot
    foreach ($tool in @("cargo", "rustc", "cmake")) {
        if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
            throw "Missing $tool. Install Rust (x86_64-pc-windows-msvc), CMake and Ninja."
        }
    }
    $ninja = Resolve-Ninja
    $env:PATH = (Split-Path -Parent $ninja) + ";" + $env:PATH

    $rustInfo = (& rustc -vV) -join "`n"
    if ($rustInfo -notmatch "host: x86_64-pc-windows-msvc") {
        throw "Select the x86_64-pc-windows-msvc Rust toolchain."
    }

    Import-MsvcEnvironment
    Set-Utf8CompilerFlag

    if ($Release -and -not (Get-Command fxc.exe -ErrorAction SilentlyContinue) -and -not $env:GPUI_FXC_PATH) {
        Write-Warning "Release builds need Windows SDK fxc.exe. If not found automatically, set GPUI_FXC_PATH to its full path."
    }

    $cef = Resolve-CefPath $CefPath
    $targetDir = Resolve-TargetDir $TargetDir

    $buildArgs = @("build", "--locked", "--target", $PocTarget)
    $profile = "debug"
    if ($Release) { $buildArgs += "--release"; $profile = "release" }

    Write-Host "CEF:        $cef"
    Write-Host "Ninja:      $ninja"
    Write-Host "Target dir: $targetDir"
    $env:CEF_PATH = $cef
    $env:CARGO_TARGET_DIR = $targetDir
    if ($Legacy) {
        Invoke-Checked 'cargo' ($buildArgs + @('-p','gpui-cef-poc','--no-default-features','--features','legacy-demo','--bin','gpui-cef-poc'))
        $bundle = Resolve-BundleDir $profile
        New-Item -ItemType Directory -Force -Path $bundle | Out-Null
        Copy-Item (Join-Path $targetDir "$PocTarget\$profile\gpui-cef-poc.exe") $bundle -Force
        Copy-CefRuntime $cef $bundle
        Write-Output $bundle
        return
    }

    $base = Resolve-ComponentBundlesRoot $StagingRoot
    $session = Join-Path $base ([guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $session -ErrorAction Stop | Out-Null
    Write-Host "Staging session: $session"
    $demo = Build-ComponentArtifact 'gpui-cef-poc' 'gpui-cef-component-demo' $buildArgs (Join-Path $session 'demo-build.log')
    $demoHash = (Get-FileHash -LiteralPath $demo).Hash
    $helper = Build-ComponentArtifact 'rust-cef-helper' 'rust-cef-helper' $buildArgs (Join-Path $session 'helper-build.log')
    $helperHash = (Get-FileHash -LiteralPath $helper).Hash
    $bundle = Join-Path $session (([string][char]0x53d1) + [char]0x884c + " component bundle $profile")
    $packed = & (Join-Path $PSScriptRoot 'package-component.ps1') -HostExecutable $demo -HelperExecutable $helper -CefPath $cef -OutputDirectory $bundle -HostBuildVersion "root-public-demo-$profile"
    $manifest = Get-Content -LiteralPath (Join-Path $packed 'component-runtime.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($manifest.host.file_name -cne [IO.Path]::GetFileName($demo) -or $manifest.helper.file_name -cne [IO.Path]::GetFileName($helper) -or
        $manifest.host.sha256 -ine $demoHash -or $manifest.helper.sha256 -ine $helperHash -or
        (Get-FileHash -LiteralPath (Join-Path $packed $manifest.host.file_name)).Hash -cne $demoHash -or
        (Get-FileHash -LiteralPath (Join-Path $packed $manifest.helper.file_name)).Hash -cne $helperHash -or
        (Get-FileHash -LiteralPath $demo).Hash -cne $demoHash -or (Get-FileHash -LiteralPath $helper).Hash -cne $helperHash) {
        throw 'Cargo artifact or staged bytes changed while packaging; do not launch the bundle'
    }
    $identity = @{ demo_artifact=$demo; helper_artifact=$helper; demo_sha256=$demoHash; helper_sha256=$helperHash; bundle=$packed; profile=$profile }
    $pending = Join-Path $session 'artifact-identity.pending'
    [IO.File]::WriteAllText($pending, ($identity | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
    [IO.File]::Move($pending, (Join-Path $session 'artifact-identity.json'))
    Write-Output $packed
} finally {
    $env:CEF_PATH = $OldCefPath
    $env:CARGO_TARGET_DIR = $OldTargetDir
    $env:CL = $OldCl
    $env:PATH = $OldPath
    Set-Location $OldLocation
}
