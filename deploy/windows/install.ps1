# Builds and installs the checkouts deploy/install-windows.sh copied as
# bundles into $Root\src, then restarts the runner's boot task. Run over ssh
# as the runner's account.
param(
    [string]$Root = 'C:\zeughaus'
)
$ErrorActionPreference = 'Stop'

# Runs a native command and stops with its exit code when it fails. Its
# stderr is shown, not turned into an error (Windows PowerShell 5.1 does that
# to the stderr of native commands under 'Stop').
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

$src = Join-Path $Root 'src'
$bin = Join-Path $Root 'bin'
$exe = Join-Path $bin 'zeughaus-runner.exe'

# weida next to zeughaus, as the path dependency `../weida` expects.
foreach ($repo in 'weida', 'zeughaus') {
    $dir = Join-Path $src $repo
    $bundle = Join-Path $src "$repo.bundle"
    if (-not (Test-Path (Join-Path $dir '.git'))) {
        Run 'git' @('init', '-q', $dir)
    }
    Run 'git' @('-C', $dir, 'fetch', '-q', $bundle, 'HEAD')
    Run 'git' @('-C', $dir, 'checkout', '-q', '--force', '--detach', 'FETCH_HEAD')
    Remove-Item -Force $bundle
}

Write-Host '[deploy] building'
$env:CARGO_TARGET_DIR = Join-Path $src 'target'
Push-Location (Join-Path $src 'zeughaus')
Run 'cargo' @('build', '--locked', '--release', '-p', 'zeughaus', '-p', 'zeughaus-runner')
Pop-Location

# The boot task's wrapper starts the runner again whenever it exits, so the
# wrapper goes first; then the runner, found by its path.
Write-Host '[deploy] stopping the runner'
Stop-ScheduledTask -TaskName 'zeughaus-runner'
Get-CimInstance Win32_Process -Filter "Name = 'cmd.exe'" |
    Where-Object { $_.CommandLine -like '*\bin\runner.cmd*' } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
Get-Process -Name 'zeughaus-runner' -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $exe } |
    ForEach-Object { Stop-Process -Id $_.Id -Force; $_.WaitForExit(30000) | Out-Null }

foreach ($file in 'zeughaus.exe', 'zeughaus-runner.exe') {
    Copy-Item -Force (Join-Path $env:CARGO_TARGET_DIR "release\$file") $bin
}

Write-Host '[deploy] starting the runner'
$endpoint = Join-Path $Root 'state\endpoint'
$before = if (Test-Path $endpoint) { (Get-Item $endpoint).LastWriteTimeUtc } else { [DateTime]::MinValue }
Start-ScheduledTask -TaskName 'zeughaus-runner'
$deadline = (Get-Date).AddSeconds(60)
while ((Get-Date) -lt $deadline) {
    if ((Test-Path $endpoint) -and (Get-Item $endpoint).LastWriteTimeUtc -gt $before) {
        Write-Host "[deploy] serving at $((Get-Content -Raw $endpoint).Trim())"
        exit 0
    }
    Start-Sleep -Seconds 1
}
Write-Host "[deploy] the runner did not come up; see $(Join-Path $Root 'state\runner.log')"
exit 1
