<#
build_windows.ps1 — generic Windows build for llm_proxy (no machine-specific paths).

Requires:
  - Visual Studio 2022 Build Tools (C++ / MSVC workload) -> rc.exe, link.exe, editbin
  - Rust toolchain (x86_64-pc-windows-msvc) on PATH

Run (from the project root):
  powershell -ExecutionPolicy Bypass -File .\build_windows.ps1

Output:
  dist\llm_proxy.exe
  dist\llm_proxy.toml   (from llm_proxy_dist.toml)

Notes:
  - The exe embeds the icon + version resource (resource.rc -> resource.res):
    no path-privacy remap.
  - cargo uses a dedicated target dir (target-build) so a build can never
    lock a running llm_proxy.exe.
#>
$ErrorActionPreference = 'Stop'
$root  = $PSScriptRoot
Set-Location $root

# --- 1) Locate the VS C++ (MSVC) toolset via vswhere (no hardcoded path) ---
$pf86 = [Environment]::GetEnvironmentVariable('ProgramFiles(x86)')
$vswhere = Join-Path $pf86 'Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path $vswhere)) { throw 'vswhere not found. Install VS 2022 Build Tools (C++ workload).' }
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw 'No Visual Studio with the C++ (MSVC) workload found.' }

# --- 2) Import the MSVC dev environment (vcvars64) into this process ---
$vcvars = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
$envDump = & cmd.exe /c "`"$vcvars`" >nul 2>&1 && set"
foreach ($line in $envDump) {
    if ($line -match '^([^=]+)=(.*)$') {
        [System.Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process')
    }
}

# Ensure cargo is on PATH (fallback to the standard location).
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $cargoHome = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path $cargoHome) { $env:PATH = "$cargoHome;$env:PATH" }
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw 'cargo not found. Install the Rust toolchain.' }

# --- 3) Compile the PE resource (resource.rc -> resource.res: icon + version) ---
$rc = Get-Command rc.exe -ErrorAction SilentlyContinue
if (-not $rc) { throw 'rc.exe not found. Install VS 2022 Build Tools (C++ workload).' }
$res = Join-Path $root 'resource.res'
& $rc.Source /nologo /fo $res (Join-Path $root 'resource.rc')
if ($LASTEXITCODE -ne 0) { throw 'rc.exe failed.' }

# --- 4) cargo build (release): embed the resource, static CRT, no remap ---
$env:RUSTFLAGS = "-C link-arg=$res -C target-feature=+crt-static"
& cargo build --release --target-dir target-build
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed.' }

# --- 5) GUI subsystem: no console window at start (silent launch) ---
$exe = Join-Path $root 'target-build\release\llm_proxy.exe'
$editbin = Get-Command editbin -ErrorAction SilentlyContinue
if ($editbin) {
    & $editbin.Source /SUBSYSTEM:WINDOWS $exe
    if ($LASTEXITCODE -ne 0) { throw 'editbin failed.' }
} else {
    Write-Warning 'editbin not found - skipping SUBSYSTEM:WINDOWS (the exe will open a console).'
}

# --- 6) Copy to dist (+ the distribution toml) ---
$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null
Copy-Item -Force $exe (Join-Path $dist 'llm_proxy.exe')
$distToml = Join-Path $root 'llm_proxy_dist.toml'
if (Test-Path $distToml) {
    Copy-Item -Force $distToml (Join-Path $dist 'llm_proxy.toml')
} else {
    Write-Warning 'llm_proxy_dist.toml not found - skipping dist\llm_proxy.toml.'
}

Write-Host "Built: $dist\llm_proxy.exe"
