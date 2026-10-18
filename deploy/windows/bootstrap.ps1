# Installs the latest build of a release channel on a Windows runner host,
# once; every later update comes from the installed runner itself.
# deploy/setup-windows.sh copies this into $Root\src and runs it over ssh as
# the runner's account.
#
#   1. the build number from <Url>/<Channel>/latest, SHA256SUMS of that
#      build, and the windows-amd64 archive checked against its line;
#   2. the archive unpacked into a temporary directory (tar.exe reads zip);
#   3. the runner's boot task, its wrapper and the runner stopped;
#   4. `zeughaus-runner.exe install windows` of the unpacked build: both
#      binaries into $Root\bin, maintain.toml, update.toml and install.toml
#      in $Root\state;
#   5. the boot task started, and a wait for the runner's endpoint file.
#
# This is the only download that is not checked against a signature: it
# trusts HTTPS to our own host. Every later update is checked against the
# key compiled into the binary installed here.
param(
    [string]$Channel = 'dev',
    [string]$Url = 'https://zeughaus.doodleshnookie.net/updates',
    [string]$Root = 'C:\zeughaus'
)
$ErrorActionPreference = 'Stop'
# Windows PowerShell 5.1 negotiates TLS 1.0 by default.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

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

$bin = Join-Path $Root 'bin'
$exe = Join-Path $bin 'zeughaus-runner.exe'
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("zeughaus-bootstrap-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
    Write-Host "[deploy] channel $Channel at $Url"
    $build = ((Invoke-WebRequest -UseBasicParsing -Uri "$Url/$Channel/latest").Content).Trim()
    if (-not $build) {
        throw "$Url/$Channel/latest is empty"
    }
    $sums = (Invoke-WebRequest -UseBasicParsing -Uri "$Url/$Channel/$build/SHA256SUMS").Content
    $line = $sums -split "`n" |
        ForEach-Object { $_.Trim() } |
        Where-Object { $_ -match '-windows-amd64\.zip$' } |
        Select-Object -First 1
    if (-not $line) {
        throw "build $build has no archive for windows-amd64"
    }
    $parts = $line -split '\s+', 2
    $hash = $parts[0].ToLowerInvariant()
    $file = $parts[1].TrimStart('*')
    Write-Host "[deploy] build ${build}: $file"
    $archive = Join-Path $tmp $file
    Invoke-WebRequest -UseBasicParsing -Uri "$Url/$Channel/$build/$file" -OutFile $archive
    $actual = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
    if ($actual -ne $hash) {
        throw "$file has the SHA-256 $actual, SHA256SUMS says $hash"
    }

    $unpacked = Join-Path $tmp 'unpacked'
    New-Item -ItemType Directory -Force -Path $unpacked | Out-Null
    Run 'tar.exe' @('-xf', $archive, '-C', $unpacked)
    $top = Join-Path $unpacked ([IO.Path]::GetFileNameWithoutExtension($file))
    $new = Join-Path $top 'zeughaus-runner.exe'
    if (-not (Test-Path $new)) {
        throw "the archive has no $new"
    }

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

    Run $new @('--state-dir', (Join-Path $Root 'state'), 'install', 'windows',
        '--bin-dir', $bin, '--channel', $Channel)

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
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
