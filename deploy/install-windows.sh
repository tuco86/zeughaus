#!/bin/sh
# Installs this checkout's commit on a Windows runner host, over ssh from a
# unix checkout that has weida next to it (`../weida`):
#
#   1. with --setup, once per host: deploy/windows/setup.ps1 (the root
#      C:\zeughaus readable by the runner's account only, PowerShell as
#      sshd's shell, the firewall rule, the boot task) and
#      vm/win11/toolchain.ps1 -CacheRoot C:\zeughaus -Shared (MSVC Build
#      Tools, Git, CMake, LLVM, Rust);
#   2. HEAD of this checkout and of ../weida as git bundles into
#      C:\zeughaus\src, built there (release);
#   3. deploy/windows/install.ps1: stops the runner, installs zeughaus.exe and
#      zeughaus-runner.exe into C:\zeughaus\bin and starts the boot task.
#
#   deploy/install-windows.sh [--setup] [--feed-addr <host:port>] <ssh-host>
#
# --feed-addr is the runner's endpoint on the host's WireGuard address
# (default 10.8.0.12:7443, the `wohnzimmer` runner); only --setup writes it.
# The ssh host logs in as the runner's account, an administrator.
#
# Windows has no shims: the restart ends the runner's terminals and the CI
# job it is running. Log: C:\zeughaus\state\runner.log on the host.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
weida=$(cd "$root/../weida" && pwd)
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

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if [ -n "$setup" ]; then
    say "setting up $host"
    scp -q -o BatchMode=yes "$root/deploy/windows/setup.ps1" "$root/vm/win11/toolchain.ps1" \
        "$host:/C:/Windows/Temp/"
    run_ps1 "C:\\Windows\\Temp\\setup.ps1 -FeedAddr $feed_addr"
    say "toolchain (minutes)"
    run_ps1 'C:\Windows\Temp\toolchain.ps1 -CacheRoot C:\zeughaus -Shared'
fi

say "bundling $(git -C "$root" rev-parse --short HEAD) and weida $(git -C "$weida" rev-parse --short HEAD)"
git -C "$root" bundle create -q "$tmp/zeughaus.bundle" HEAD
git -C "$weida" bundle create -q "$tmp/weida.bundle" HEAD
scp -q -o BatchMode=yes "$tmp/zeughaus.bundle" "$tmp/weida.bundle" "$root/deploy/windows/install.ps1" \
    "$host:/C:/zeughaus/src/"
run_ps1 'C:\zeughaus\src\install.ps1'
say "done"
