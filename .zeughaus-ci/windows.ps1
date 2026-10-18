# Dot-sourced by the Windows build and check jobs; no header, so not a job.
# Sets `Run`, `$cache` and CARGO_TARGET_DIR, and makes `..\weida` the weida
# checkout the workspace builds against.
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

# A manual run brings its own weida in CI_SIBLINGS; otherwise it is weida's
# main branch, as weida.sh does for the Unix jobs, kept in the cache and
# fetched with the read-only token, which travels in the environment, not in
# argv.
$source = $null
if ($env:CI_SIBLINGS -and (Test-Path (Join-Path (Join-Path $env:CI_SIBLINGS 'weida') '.git'))) {
    $source = Join-Path $env:CI_SIBLINGS 'weida'
} else {
    $source = Join-Path $cache 'weida'
    $weidaUrl = 'https://git.doodleshnookie.net/tuco86/weida.git'
    $basic = [Convert]::ToBase64String([Text.Encoding]::ASCII.GetBytes("zeughaus-ci:$($env:FORGEJO_READ_TOKEN)"))
    $env:GIT_CONFIG_COUNT = '1'
    $env:GIT_CONFIG_KEY_0 = 'http.extraHeader'
    $env:GIT_CONFIG_VALUE_0 = "Authorization: Basic $basic"
    $env:GIT_TERMINAL_PROMPT = '0'
    if (Test-Path (Join-Path $source '.git')) {
        Run 'git' @('-C', $source, 'fetch', '-q', $weidaUrl, 'main')
        Run 'git' @('-C', $source, 'checkout', '-q', '--force', '--detach', 'FETCH_HEAD')
    } else {
        Run 'git' @('clone', '-q', '--branch', 'main', $weidaUrl, $source)
    }
    Remove-Item Env:GIT_CONFIG_COUNT, Env:GIT_CONFIG_KEY_0, Env:GIT_CONFIG_VALUE_0, Env:GIT_TERMINAL_PROMPT -ErrorAction SilentlyContinue
    $basic = $null
}
Remove-Item Env:FORGEJO_READ_TOKEN -ErrorAction SilentlyContinue

# `..\weida` is a junction to the source. A repository's jobs at one place
# share one workspace and run one at a time, so nothing else writes it
# meanwhile. `rmdir` removes a junction, not its target; a real directory is
# a clone an older job left there.
$link = Join-Path (Split-Path $env:CI_WORKSPACE -Parent) 'weida'
$existing = Get-Item -LiteralPath $link -Force -ErrorAction SilentlyContinue
if ($existing) {
    if ($existing.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        cmd /c rmdir "$link"
    } else {
        Remove-Item -LiteralPath $link -Recurse -Force
    }
}
New-Item -ItemType Junction -Path $link -Target $source | Out-Null
Run 'git' @('-C', $link, 'log', '--oneline', '-1')
