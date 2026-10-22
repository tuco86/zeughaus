# /// ci
# on = ["push main", "tag v*", "cron 0 3 * * *"]
# machine = "wohnzimmer"
# timeout_minutes = 180
# ///
# The windows-amd64 release archive: editor and runner built on the Windows
# runner, zipped into the job's output for publish.sh.
. (Join-Path $env:CI_WORKSPACE '.zeughaus-ci\windows.ps1')
$env:ZEUGHAUS_BUILD = $env:CI_PIPELINE

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
# What `install windows` and every update write besides the binaries, under
# its path in the repository.
$share = Join-Path $stage 'share\deploy\windows'
New-Item -ItemType Directory -Force -Path $share | Out-Null
Copy-Item (Join-Path $env:CI_WORKSPACE 'deploy\windows\maintain.toml') $share
Run (Join-Path $stage 'zeughaus-runner.exe') @('--help')

# Windows' bsdtar rather than Compress-Archive: Windows PowerShell 5.1 writes
# backslashes into zip entry names, which unzip elsewhere reads as file names.
New-Item -ItemType Directory -Force -Path $env:CI_OUTPUT | Out-Null
Run 'tar.exe' @('-a', '-c', '-f', (Join-Path $env:CI_OUTPUT "$name.zip"), '-C', (Join-Path $cache 'stage'), $name)
