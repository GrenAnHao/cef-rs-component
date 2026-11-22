[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateNotNullOrEmpty()][string]$HostExecutable,
    [Parameter(Mandatory)][ValidateNotNullOrEmpty()][string]$HelperExecutable,
    [Parameter(Mandatory)][ValidateNotNullOrEmpty()][string]$CefPath,
    [Parameter(Mandatory)][ValidateNotNullOrEmpty()][string]$OutputDirectory,
    [Parameter(Mandatory)][ValidateNotNullOrEmpty()][string]$HostBuildVersion,
    [ValidateNotNullOrEmpty()][string]$ComponentVersion = "0.1.0"
)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot "common.ps1")

function Get-PlainFullPath([string]$Value) {
    # std::fs::canonicalize callers may supply Win32 verbatim paths. PS5 providers
    # need the equivalent DOS/UNC form; this never searches PATH or a host name.
    if ($Value.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) { $Value = '\\' + $Value.Substring(8) }
    elseif ($Value.StartsWith('\\?\', [StringComparison]::OrdinalIgnoreCase)) { $Value = $Value.Substring(4) }
    return [IO.Path]::GetFullPath($Value)
}

# Resolve existing directory identities through junctions/symlinks. Missing output
# suffixes are appended only after resolving their nearest existing ancestor.
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;
namespace GPUI.Cef.Build {
    public static class DirectoryIdentity {
        [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern SafeFileHandle CreateFileW(string name, uint access, uint share,
            IntPtr security, uint creation, uint flags, IntPtr template);
        [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern uint GetFinalPathNameByHandleW(SafeFileHandle file,
            StringBuilder path, uint size, uint flags);
        public static string Resolve(string name) {
            using (var file = CreateFileW(name, 0, 7, IntPtr.Zero, 3, 0x02000000, IntPtr.Zero)) {
                if (file.IsInvalid) throw new Win32Exception(Marshal.GetLastWin32Error());
                var path = new StringBuilder(32768);
                uint count = GetFinalPathNameByHandleW(file, path, (uint)path.Capacity, 0);
                if (count == 0 || count >= path.Capacity)
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                return path.ToString();
            }
        }
    }
}
'@
function Get-PhysicalDirectory([string]$Path) {
    $current = Get-PlainFullPath $Path
    $suffix = New-Object 'System.Collections.Generic.Stack[string]'
    while (-not [IO.Directory]::Exists($current)) {
        if ([IO.File]::Exists($current)) { throw "Output path contains a file instead of a directory: $current" }
        $suffix.Push([IO.Path]::GetFileName($current))
        $parent = [IO.Directory]::GetParent($current)
        if ($null -eq $parent) { throw "No existing directory ancestor: $Path" }
        $current = $parent.FullName
    }
    $physical = Get-PlainFullPath ([GPUI.Cef.Build.DirectoryIdentity]::Resolve($current))
    while ($suffix.Count -gt 0) { $physical = Join-Path $physical $suffix.Pop() }
    return $physical.TrimEnd([char[]]"\/")
}
function Test-Within([string]$Child, [string]$Parent) {
    return [string]::Equals($Child, $Parent, [StringComparison]::OrdinalIgnoreCase) -or
        $Child.StartsWith($Parent + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)
}

function Assert-X64([string]$File) {
    $reader = New-Object IO.BinaryReader([IO.File]::OpenRead($File))
    try {
        if ($reader.BaseStream.Length -lt 64 -or $reader.ReadUInt16() -ne 0x5a4d) { throw "Invalid PE: $File" }
        $reader.BaseStream.Position = 0x3c
        $offset = $reader.ReadUInt32()
        if ($offset -gt $reader.BaseStream.Length - 6) { throw "Invalid PE offset: $File" }
        $reader.BaseStream.Position = $offset
        if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne 0x8664) { throw "Not a Windows x64 PE: $File" }
    } finally { $reader.Dispose() }
}

function Get-Sha([string]$File) { return (Get-PocFileSha256 $File).ToLowerInvariant() }
function Copy-Different([string]$From, [string]$To) {
    if (-not [string]::Equals([IO.Path]::GetFullPath($From), [IO.Path]::GetFullPath($To), [StringComparison]::OrdinalIgnoreCase)) {
        Copy-Item -LiteralPath $From -Destination $To -Force
    }
}

$hostFile = (Resolve-Path -LiteralPath (Get-PlainFullPath $HostExecutable)).ProviderPath
$helperFile = (Resolve-Path -LiteralPath (Get-PlainFullPath $HelperExecutable)).ProviderPath
$cef = Resolve-CefPath (Get-PlainFullPath $CefPath)
$out = Get-PlainFullPath $OutputDirectory
if ([string]::IsNullOrWhiteSpace($HostBuildVersion)) { throw "HostBuildVersion must be explicit" }
$physicalCef = Get-PhysicalDirectory $cef
$physicalOutput = Get-PhysicalDirectory $out
if ((Test-Within $physicalOutput $physicalCef) -or (Test-Within $physicalCef $physicalOutput)) {
    throw "OutputDirectory and source CEF SDK must not overlap, including directory aliases"
}
if (Test-Path -LiteralPath (Join-Path $out "CMakeLists.txt")) { throw "OutputDirectory cannot be a CMake source directory" }
$hostName = [IO.Path]::GetFileName($hostFile)
$helperName = [IO.Path]::GetFileName($helperFile)
if ($hostName -ieq $helperName) { throw "Host and helper file names must be distinct" }
Assert-X64 $hostFile
Assert-X64 $helperFile
$stamp = "GPUI-CEF-COMPONENT|helper|schema=1|target=$PocTarget|component=$ComponentVersion|cef=$PocCefVersion|binding=$PocRevision"
if (-not [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($helperFile)).Contains($stamp)) { throw "Helper does not advertise the selected component/CEF cohort" }

$rootFiles = @(Get-ChildItem -LiteralPath $cef -File | Where-Object {
    ($_.Extension -in @(".dll", ".pak", ".dat", ".bin", ".json") -or $_.Name -match "LICENSE|CREDITS") -and $_.Name -ne "component-runtime.json"
})
$items = @()
foreach ($file in $rootFiles) {
    if ($file.Name -ieq $hostName -or $file.Name -ieq $helperName) { throw "Executable collides with CEF resource: $($file.Name)" }
    if ($file.Extension -ieq ".dll") { Assert-X64 $file.FullName }
    $items += @{ source = $file.FullName; path = $file.Name }
}
$localeRoot = Join-Path $cef "locales"
foreach ($file in (Get-ChildItem -LiteralPath $localeRoot -File -Recurse)) {
    $relative = $file.FullName.Substring($localeRoot.Length).TrimStart([char[]]"\/").Replace('\', '/')
    $items += @{ source = $file.FullName; path = ("locales/" + $relative) }
}
$required = @("libcef.dll", "chrome_elf.dll", "d3dcompiler_47.dll", "dxcompiler.dll", "vulkan-1.dll", "vk_swiftshader.dll", "vk_swiftshader_icd.json", "chrome_100_percent.pak", "chrome_200_percent.pak", "resources.pak", "icudtl.dat", "v8_context_snapshot.bin", "archive.json", "locales/en-US.pak")
foreach ($name in $required) {
    if (-not ($items | Where-Object { $_.path -ieq $name })) { throw "Missing required runtime source: $name" }
}
# All cheap source validation precedes creating or copying the output.
New-Item -ItemType Directory -Force -Path $out | Out-Null
$out = (Resolve-Path -LiteralPath $out).ProviderPath
Copy-Different $hostFile (Join-Path $out $hostName)
Copy-Different $helperFile (Join-Path $out $helperName)
$records = @()
foreach ($item in $items) {
    $destination = Join-Path $out $item.path
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $destination) | Out-Null
    Copy-Different $item.source $destination
    $records += @{ path = $item.path; sha256 = (Get-Sha $destination) }
}
$manifest = @{
    schema = 1; target = $PocTarget; cef_version = $PocCefVersion; binding_revision = $PocRevision; component_version = $ComponentVersion
    host = @{ file_name = $hostName; build_version = $HostBuildVersion; sha256 = (Get-Sha (Join-Path $out $hostName)) }
    helper = @{ file_name = $helperName; build_version = $ComponentVersion; sha256 = (Get-Sha (Join-Path $out $helperName)) }
    runtime = @($records | Sort-Object { $_.path })
}
[IO.File]::WriteAllText((Join-Path $out "component-runtime.json"), ($manifest | ConvertTo-Json -Depth 8), (New-Object Text.UTF8Encoding($false)))
Write-Output $out
