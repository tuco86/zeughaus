# Dot-sourced by the Windows build and check jobs; no header, so not a job.
# Sets `Run`, `Install-Nextest`, `$cache`, CARGO_TARGET_DIR and
# CARGO_PROFILE_DEV_DEBUG.
$ErrorActionPreference = 'Stop'

# Runs a native command and ends the script with its exit code when it fails.
# Its output goes straight to the job's console, not through a pipe: so cargo
# and nextest see a terminal and color their output, and their UTF-8 is not
# decoded again with the console codepage. 'Continue' because Windows
# PowerShell 5.1 turns a native command's stderr into errors under 'Stop'
# where it captures it.
function Run([string]$Command, [string[]]$Arguments = @()) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $clock = [Diagnostics.Stopwatch]::StartNew()
    try {
        & $Command @Arguments
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
    }
    $seconds = [int][Math]::Round($clock.Elapsed.TotalSeconds)
    if ($code -ne 0) {
        Write-Host "== $Command $($Arguments -join ' ') failed after $seconds s"
        Write-Host "$Command $($Arguments -join ' ') failed with exit code $code"
        exit $code
    }
    Write-Host "== $Command $($Arguments -join ' '): $seconds s"
}

$cache = Join-Path $env:CI_WORKSPACE '.ci-cache'
$env:CARGO_TARGET_DIR = Join-Path $cache 'target'
# Test and clippy builds keep file:line for panics and backtraces, while
# target directories shrink and links get faster. The test profile inherits
# it; release builds are unaffected; local builds keep full debug info.
$env:CARGO_PROFILE_DEV_DEBUG = 'line-tables-only'

# cargo-nextest from its prebuilt release into `$cache\bin`, kept between runs
# by the warm workspace; on PATH so `cargo nextest` finds it.
function Install-Nextest {
    $bin = Join-Path $cache 'bin'
    if (-not (Test-Path (Join-Path $bin 'cargo-nextest.exe'))) {
        New-Item -ItemType Directory -Force -Path $bin | Out-Null
        $zip = Join-Path $env:TEMP "cargo-nextest-$PID.zip"
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        Invoke-WebRequest -UseBasicParsing -Uri 'https://get.nexte.st/0.9/windows' -OutFile $zip
        Expand-Archive -Path $zip -DestinationPath $bin -Force
        Remove-Item -LiteralPath $zip
    }
    $env:PATH = "$bin;$env:PATH"
}
