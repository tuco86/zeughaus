#!/bin/sh
# Installs this checkout as the running Zeughaus of this user (Linux,
# systemd user manager):
#
#   1. `cargo install`s the editor and the runner into ~/.cargo/bin, first,
#      so the running stack keeps serving while they compile;
#   2. the runner unit, the desktop entry and icon;
#   3. starts the runner, or reloads it (SIGUSR1: same PID, terminals stay);
#   4. sends SIGUSR1 to every editor running the installed binary, which
#      reopens as it was.
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
systemctl --user enable zeughaus-runner.service

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
