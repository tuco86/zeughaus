#!/bin/sh
# Sets up a Windows runner host and installs the latest dev build on it, over
# ssh from a unix workstation. The host updates itself from the dev channel
# afterwards; nothing is built from this checkout.
#
#   1. with --setup, once per host: deploy/windows/setup.ps1 (the root
#      C:\zeughaus readable by the runner's account only, PowerShell as
#      sshd's shell, the firewall rule, the boot task) and
#      vm/win11/toolchain.ps1 -CacheRoot C:\zeughaus -Shared (MSVC Build
#      Tools, Git, CMake, LLVM, Rust), which the CI jobs the host builds
#      need;
#   2. deploy/windows/bootstrap.ps1 into C:\zeughaus\src and run there: it
#      downloads the latest dev build, stops the runner, has the new
#      zeughaus-runner.exe install itself and zeughaus.exe into
#      C:\zeughaus\bin (`install windows`), and starts the boot task.
#
#   deploy/setup-windows.sh [--setup] [--feed-addr <host:port>] <ssh-host>
#
# --feed-addr is the runner's endpoint on the host's WireGuard address
# (default 10.8.0.12:7443, the `wohnzimmer` runner); only --setup writes it.
# The ssh host logs in as the runner's account, an administrator.
#
# Windows has no shims: every update restarts the runner, which ends its
# terminals and the CI job it is running, so it installs one only while the
# runner is idle. Log: C:\zeughaus\state\runner.log on the host.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
feed_addr=10.8.0.12:7443
setup=
host=

usage() {
    printf 'usage: %s [--setup] [--feed-addr <host:port>] <ssh-host>\n' "$0" >&2
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
    --setup) setup=1 ;;
    --feed-addr)
        [ $# -ge 2 ] || usage
        feed_addr=$2
        shift
        ;;
    -*) usage ;;
    *)
        [ -z "$host" ] || usage
        host=$1
        ;;
    esac
    shift
done
[ -n "$host" ] || usage

say() { printf '[deploy] %s\n' "$*"; }
# Windows OpenSSH's scp addresses drives as /C:/...; its shell is PowerShell
# once setup.ps1 ran.
run_ps1() { ssh -o BatchMode=yes "$host" "powershell -NoLogo -NoProfile -ExecutionPolicy Bypass -File $*"; }

if [ -n "$setup" ]; then
    say "setting up $host"
    scp -q -o BatchMode=yes "$root/deploy/windows/setup.ps1" "$root/vm/win11/toolchain.ps1" \
        "$host:/C:/Windows/Temp/"
    run_ps1 "C:\\Windows\\Temp\\setup.ps1 -FeedAddr $feed_addr"
    say "toolchain (minutes)"
    run_ps1 'C:\Windows\Temp\toolchain.ps1 -CacheRoot C:\zeughaus -Shared'
fi

say "installing the latest dev build"
scp -q -o BatchMode=yes "$root/deploy/windows/bootstrap.ps1" "$host:/C:/zeughaus/src/"
run_ps1 'C:\zeughaus\src\bootstrap.ps1'
say "done"
