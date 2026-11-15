# Builds space-sweep.exe in release mode and packages it into dist\SpaceSweep-Setup-<version>.exe.
# Exit codes are checked explicitly: Windows PowerShell treats cargo's progress output on stderr as errors.
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
    $env:CARGO_INCREMENTAL = '0'
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    $iscc = @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $iscc) { throw "Inno Setup 6 not found. Install it from https://jrsoftware.org/isinfo.php" }
    & $iscc /Q "/DAppVersion=$version" installer\space-sweep.iss
    if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }
    Get-Item "dist\SpaceSweep-Setup-$version.exe" | Select-Object Name, @{n='MB';e={[math]::Round($_.Length/1MB,2)}}
} finally {
    Pop-Location
}
