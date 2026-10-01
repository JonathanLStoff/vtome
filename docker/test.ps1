# vtome's test matrix on native Windows — windows.Dockerfile's entry point, and
# runnable by hand on any Windows machine with Rust and ffmpeg:
#
#   powershell -File docker\test.ps1
#
# The same steps as test.sh's Linux profile, minus Linux's own libraries.

$ErrorActionPreference = 'Continue'

# Work on a copy, so nothing a test writes reaches the source tree.
if (Test-Path C:\src) {
    robocopy C:\src C:\work /E /XD target .git /NFL /NDL /NJH /NJS | Out-Null
    Set-Location C:\work
} else {
    Set-Location (Split-Path $PSScriptRoot)
}

$results = @()
$failed = $false

function Step($name, [scriptblock]$command) {
    Write-Host "==> $name"
    & $command
    if ($LASTEXITCODE -eq 0) {
        $script:results += "PASS  $name"
    } else {
        $script:results += "FAIL  $name"
        $script:failed = $true
    }
}

Step 'converters' { ffmpeg -hide_banner -version | Select-Object -First 1 }
Step 'default' { cargo test }
Step 'no-default-features' { cargo test --no-default-features }
Step 'render' { cargo test --features render }
Step 'window' { cargo test --features window --lib }
Step 'decode-platform' { cargo test --features render,decode-platform }
Step 'tauri' { cargo check --features tauri }

Write-Host ''
Write-Host 'vtome on windows:'
$results | ForEach-Object { Write-Host "  $_" }

if ($failed) { exit 1 }
