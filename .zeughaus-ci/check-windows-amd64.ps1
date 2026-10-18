# /// ci
# needs = ["build-windows-amd64"]
# machine = "wohnzimmer"
# secrets = ["FORGEJO_READ_TOKEN"]
# timeout_minutes = 180
# ///
# clippy and the tests on the Windows runner's host, in the workspace the
# build left warm.
. (Join-Path $env:CI_WORKSPACE '.zeughaus-ci\windows.ps1')

Run 'cargo' @('clippy', '--locked', '--workspace', '--all-targets', '--', '-D', 'warnings')
Run 'cargo' @('test', '--locked', '--workspace')
