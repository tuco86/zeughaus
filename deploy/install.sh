#!/bin/sh
# Installs this checkout as the running Zeughaus of this user (Linux,
# systemd user manager):
#
#   1. `cargo install`s the editor and the runner into ~/.cargo/bin, first,
#      so the running stack keeps serving while they compile;
#   2. the store (SpacetimeDB) and runner units, the desktop entry and icon;
#   3. starts the store and publishes the module to the `zeughaus` database;
#   4. starts the runner, or reloads it (SIGUSR1: same PID, terminals stay);
#   5. sends SIGUSR1 to every editor running the installed binary, which
#      reopens as it was.
#
# Publishing never passes -y: a migration that has to clear data stops here
# and is the user's decision. Run it again once that is settled.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
bindir=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin
units=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
data=${XDG_DATA_HOME:-$HOME/.local/share}

say() { printf '[deploy] %s\n' "$*"; }

say "binaries into $bindir"
cargo install --locked --path "$root/zeughaus"
cargo install --locked --path "$root/zeughaus-runner"

say "units, desktop entry, icon"
install -Dm644 "$root/deploy/zeughaus-store.service" "$units/zeughaus-store.service"
install -Dm644 "$root/deploy/zeughaus-runner.service" "$units/zeughaus-runner.service"
mkdir -p "$data/applications"
sed "s|@BINDIR@|$bindir|g" "$root/deploy/net.doodleshnookie.Zeughaus.desktop" \
    >"$data/applications/net.doodleshnookie.Zeughaus.desktop"
install -Dm644 "$root/zeughaus/assets/icon/zeughaus.svg" \
    "$data/icons/hicolor/scalable/apps/net.doodleshnookie.Zeughaus.svg"
# A cache some other installer left in the user's hicolor theme is trusted
# by Qt as long as it is valid, and it does not list this icon: the menu and
# the task bar then show a blank one. Refresh it; never create one.
if [ -f "$data/icons/hicolor/icon-theme.cache" ] && command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t -q "$data/icons/hicolor" || say "gtk-update-icon-cache failed"
fi
if command -v kbuildsycoca6 >/dev/null 2>&1; then
    kbuildsycoca6 >/dev/null 2>&1 || say "kbuildsycoca6 failed; the menu updates on next login"
fi
systemctl --user daemon-reload
systemctl --user enable zeughaus-store.service zeughaus-runner.service

say "store"
# Fails when a store started by hand holds the port: stop that one first.
systemctl --user start zeughaus-store.service
tries=0
until spacetime server ping local >/dev/null 2>&1; do
    tries=$((tries + 1))
    if [ "$tries" -ge 50 ]; then
        say "the store does not answer; see: journalctl --user -u zeughaus-store"
        exit 1
    fi
    sleep 0.2
done
spacetime publish --server local zeughaus --module-path "$root/zeughaus-module"

if systemctl --user is-active --quiet zeughaus-runner.service; then
    say "reloading the runner"
    systemctl --user reload zeughaus-runner.service
else
    say "starting the runner"
    systemctl --user start zeughaus-runner.service
fi

# Only editors of this install, and only those that answer SIGUSR1 with a
# restart (bit 9 of SigCgt); an older one would die from it.
for pid in $(pgrep -x zeughaus || true); do
    exe=$(readlink "/proc/$pid/exe" 2>/dev/null || true)
    case "$exe" in
    "$bindir/zeughaus" | "$bindir/zeughaus (deleted)") ;;
    *) continue ;;
    esac
    caught=$(sed -n 's/^SigCgt:[[:space:]]*//p' "/proc/$pid/status")
    if [ $((0x$caught & 0x200)) -ne 0 ]; then
        say "restarting editor $pid"
        kill -USR1 "$pid"
    else
        say "editor $pid cannot restart itself; close and reopen it"
    fi
done
say "done"

# The CI runner (deploy/install-ci.sh) is a separate install; once it exists,
# bring its binary and units up to date. Never waits for a sudo password.
if id zeughaus-ci >/dev/null 2>&1; then
    sh "$root/deploy/install-ci.sh" --update
fi
