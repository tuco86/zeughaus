# /// ci
# on = ["push main", "tag v*", "cron 0 3 * * *"]
# machine = "wohnzimmer"
# cache = [".ci-cache"]
# secrets = ["FORGEJO_READ_TOKEN"]
# timeout_minutes = 180
# ///
# The windows-amd64 release archive: editor and runner built on the Windows
# runner, zipped into the job's output for publish.sh.
$ErrorActionPreference = 'Stop'

# Runs a native command and ends the script with its exit code when it fails.
# Its stderr is shown, not turned into an error (Windows PowerShell 5.1 does
# that to the stderr of native commands under 'Stop').
function Run([string]$Command, [string[]]$Arguments = @()) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Command @Arguments 2>&1 | ForEach-Object { Write-Host "$_" }
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
    }
    if ($code -ne 0) {
        Write-Host "$Command $($Arguments -join ' ') failed with exit code $code"
        exit $code
    }
}

$cache = Join-Path $env:CI_WORKSPACE '.ci-cache'
$env:CARGO_TARGET_DIR = Join-Path $cache 'target'
$env:ZEUGHAUS_BUILD = $env:CI_PIPELINE

# The workspace builds weida from the sibling checkout `..\weida`, at its main
# branch, as weida.sh does for the Unix jobs. The read-only token travels in
# the environment, not in argv.
$weida = Join-Path (Split-Path $env:CI_WORKSPACE -Parent) 'weida'
$weidaUrl = 'https://git.doodleshnookie.net/tuco86/weida.git'
$basic = [Convert]::ToBase64String([Text.Encoding]::ASCII.GetBytes("zeughaus-ci:$($env:FORGEJO_READ_TOKEN)"))
$env:GIT_CONFIG_COUNT = '1'
$env:GIT_CONFIG_KEY_0 = 'http.extraHeader'
$env:GIT_CONFIG_VALUE_0 = "Authorization: Basic $basic"
$env:GIT_TERMINAL_PROMPT = '0'
if (Test-Path (Join-Path $weida '.git')) {
    Run 'git' @('-C', $weida, 'fetch', '-q', $weidaUrl, 'main')
    Run 'git' @('-C', $weida, 'checkout', '-q', '--force', '--detach', 'FETCH_HEAD')
} else {
    Run 'git' @('clone', '-q', '--branch', 'main', $weidaUrl, $weida)
}
Remove-Item Env:GIT_CONFIG_COUNT, Env:GIT_CONFIG_KEY_0, Env:GIT_CONFIG_VALUE_0, Env:GIT_TERMINAL_PROMPT, Env:FORGEJO_READ_TOKEN -ErrorAction SilentlyContinue
$basic = $null

# The workspace version names the archive and its top directory.
$version = $null
foreach ($line in Get-Content Cargo.toml) {
    if ($line -match '^version = "(.*)"') { $version = $Matches[1]; break }
}
if (-not $version) {
    Write-Host 'no version in Cargo.toml'
    exit 1
}

Run 'cargo' @('build', '--locked', '--release', '-p', 'zeughaus', '-p', 'zeughaus-runner')

$name = "zeughaus-$version-windows-amd64"
$stage = Join-Path (Join-Path $cache 'stage') $name
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
foreach ($file in 'zeughaus.exe', 'zeughaus-runner.exe') {
    Copy-Item (Join-Path $env:CARGO_TARGET_DIR "release\$file") $stage
}
Copy-Item 'LICENSE-MIT', 'LICENSE-APACHE' $stage
Run (Join-Path $stage 'zeughaus-runner.exe') @('--help')

# Windows' bsdtar rather than Compress-Archive: Windows PowerShell 5.1 writes
# backslashes into zip entry names, which unzip elsewhere reads as file names.
New-Item -ItemType Directory -Force -Path $env:CI_OUTPUT | Out-Null
Run 'tar.exe' @('-a', '-c', '-f', (Join-Path $env:CI_OUTPUT "$name.zip"), '-C', (Join-Path $cache 'stage'), $name)
