#!/bin/sh
# Installs this checkout as the running Zeughaus of this macOS user:
#
#   1. the editor as ~/Applications/Zeughaus.app (deploy/macos-app.sh);
#   2. `cargo install`s the runner into ~/.cargo/bin;
#   3. the runner's launchd agent, started, or reloaded with SIGUSR1 (same
#      PID, terminals stay) when it is loaded already.
#
#   deploy/install-macos.sh [--feed-addr <host:port>]   default 10.8.0.7:7443
#
# Logs: ~/Library/Logs/zeughaus-runner.log.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
bindir=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin
label=net.doodleshnookie.zeughaus-runner
agents=$HOME/Library/LaunchAgents
plist=$agents/$label.plist
logs=$HOME/Library/Logs
domain=gui/$(id -u)
feed_addr=10.8.0.7:7443

say() { printf '[deploy] %s\n' "$*"; }

case "${1:-}" in
--feed-addr)
    [ $# -eq 2 ] || {
        printf 'usage: %s [--feed-addr <host:port>]\n' "$0" >&2
        exit 2
    }
    feed_addr=$2
    ;;
"") ;;
*)
    printf 'usage: %s [--feed-addr <host:port>]\n' "$0" >&2
    exit 2
    ;;
esac

say "editor"
sh "$root/deploy/macos-app.sh"

say "runner into $bindir"
cargo install --locked --path "$root/zeughaus-runner"

say "launchd agent"
mkdir -p "$agents" "$logs"
sed -e "s|@BINDIR@|$bindir|g" -e "s|@FEED_ADDR@|$feed_addr|g" -e "s|@LOGS@|$logs|g" \
    "$root/deploy/macos/$label.plist" >"$plist"
if launchctl print "$domain/$label" >/dev/null 2>&1; then
    say "reloading the runner"
    launchctl kill SIGUSR1 "$domain/$label"
else
    say "starting the runner"
    launchctl bootstrap "$domain" "$plist"
fi
say "done"
