# ---------------------------------------------------------------------------
# cargo build wrapper (ASCII-only on purpose)
#
# WHY THIS EXISTS:
#   The local Rust toolchain is x86_64-pc-windows-msvc. Linking requires MSVC's
#   link.exe and the Windows SDK, which are NOT on PATH by default. Without them
#   cargo fails with a misleading error like:
#       "can't find crate for `core` which `std` depends on"
#
#   This script loads vcvars64.bat's environment into the current process, then
#   runs cargo.
#
# NOTE: Keep this file ASCII-only. Windows PowerShell 5.1 reads .ps1 files using
#   the system ANSI codepage (GBK on zh-CN). Multi-byte characters can corrupt
#   string literals and produce bogus parse errors.
#
# USAGE:
#   powershell -File scripts\cargo.ps1 build
#   powershell -File scripts\cargo.ps1 test
#   powershell -File scripts\cargo.ps1 run -- probe
# ---------------------------------------------------------------------------

param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CargoArgs
)

$ErrorActionPreference = 'Stop'

$vcvars = 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
if (-not (Test-Path $vcvars)) {
    Write-Error "vcvars64.bat not found: $vcvars  -- edit scripts\cargo.ps1"
    exit 1
}

# Single-quote the whole cmd line so PowerShell does not try to parse && or quotes.
$cmdLine = '"' + $vcvars + '" >nul 2>&1 && set'
$raw = & cmd.exe /c $cmdLine 2>&1

if ($LASTEXITCODE -ne 0 -or -not $raw) {
    Write-Error 'failed to load MSVC environment'
    exit 1
}

$applied = 0
foreach ($line in $raw) {
    if ($line -isnot [string]) { continue }
    $idx = $line.IndexOf('=')
    if ($idx -lt 1) { continue }
    $name = $line.Substring(0, $idx)
    $value = $line.Substring($idx + 1)
    if ($name -match '^[A-Za-z_][A-Za-z0-9_]*$') {
        [System.Environment]::SetEnvironmentVariable($name, $value, 'Process')
        $applied++
    }
}

$link = Get-Command link.exe -ErrorAction SilentlyContinue
if (-not $link) {
    Write-Error "MSVC env loaded ($applied vars) but link.exe still not on PATH"
    exit 1
}

# --- libclang (needed by bindgen for sherpa-rs-sys) -------------------------
# Without this, sherpa-rs-sys fails with "Unable to find libclang", which looks
# like a dependency/network problem but is not.
foreach ($cand in @('C:\Program Files\LLVM\bin', 'C:\Program Files (x86)\LLVM\bin')) {
    if (Test-Path (Join-Path $cand 'libclang.dll')) {
        [System.Environment]::SetEnvironmentVariable('LIBCLANG_PATH', $cand, 'Process')
        if ($env:PATH -notlike "*$cand*") {
            [System.Environment]::SetEnvironmentVariable('PATH', "$cand;$env:PATH", 'Process')
        }
        break
    }
}
if (-not $env:LIBCLANG_PATH) {
    Write-Host '[warn] libclang not found; building sherpa-rs-sys will fail.'
    Write-Host '       fix: winget install LLVM.LLVM'
}

$repoRoot = Split-Path -Parent $PSScriptRoot

# --- sherpa-onnx natives ---------------------------------------------------
# sherpa-rs-sys downloads a prebuilt archive from GitHub on first build.
# That download uses ureq with no retry/resume and frequently times out here.
#
# So we vendor the small set of files it actually needs (lib/ + dll/, ~15MB) in
# native/sherpa-rs/ and point SHERPA_LIB_PATH at it.
#
# NOTE: build.rs OVERWRITES SHERPA_LIB_PATH when download-binaries is on, so
# this alone is not enough -- the extracted archive must also exist in the cargo
# cache dir ($env:LOCALAPPDATA\sherpa-rs\<target>\<checksum>\). See README.
$vendored = Join-Path $repoRoot 'native\sherpa-rs'
if (Test-Path (Join-Path $vendored 'lib')) {
    [System.Environment]::SetEnvironmentVariable('SHERPA_LIB_PATH', $vendored, 'Process')
} else {
    Write-Host "[warn] $vendored\lib not found; sherpa-rs-sys will try to download."
}

Set-Location -Path $repoRoot

if (-not $CargoArgs -or $CargoArgs.Count -eq 0) {
    $CargoArgs = @('build')
}

& cargo @CargoArgs
exit $LASTEXITCODE
