# /// ci
# needs = ["build-windows-amd64"]
# machine = "wohnzimmer"
# timeout_minutes = 180
# ///
# clippy, the tests and the doctests on the Windows runner's host, in the
# workspace the build left warm: the unit tier on a push or manual run, both
# tiers on the nightly cron and a release tag.
. (Join-Path $env:CI_WORKSPACE '.zeughaus-ci\windows.ps1')

Install-Nextest
Run 'cargo' @('clippy', '--locked', '--workspace', '--all-targets', '--', '-D', 'warnings')
$tier = if ($env:CI_EVENT -eq 'cron' -or $env:CI_EVENT -eq 'tag') { 'ci' } else { 'default' }
Run 'cargo' @('nextest', 'run', '--locked', '--workspace', '--profile', $tier)
Run 'cargo' @('test', '--locked', '--workspace', '--doc')
