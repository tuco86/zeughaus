#!/bin/sh
# Sets up the service runner of this workstation: a second zeughaus-runner
# that runs as its own Unix user `zeughaus` and executes CI jobs in rootless
# containers and the Windows VM once it is enlisted into a CI
# (`<state>/enlist.toml`, written by hand: it needs the CI's URL). Run as the
# invoking user; everything that needs root goes through sudo. Idempotent.
#
#   deploy/install-runner-user.sh [--update] --feed-addr <host:port>
#
#   1. packages (podman, passt, qemu-user-static);
#   2. /var/lib/zeughaus as its own btrfs subvolume, so snapper's snapshots
#      of `@` do not pin the job caches;
#   3. the user, its subordinate ids and linger;
#   4. resource limits of the user's slice;
#   5. the user's directories and ssh key;
#   6. the runner binary, the VM scripts and the ssh config of the VM guest;
#   7. trust for the invoking user's editor certificate;
#   8. the user unit, started or reloaded.
#
# `--update` does only 6 and 8, with `sudo -n`: deploy/install.sh calls it
# after installing the binaries, and it must never wait for a password.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
run_user=zeughaus
run_home=/var/lib/zeughaus
lib=/usr/local/lib/zeughaus
bindir=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin
client_pem=${XDG_STATE_HOME:-$HOME/.local/state}/zeughaus/client.pem

say() { printf '[runner-install] %s\n' "$*"; }
usage() {
    printf 'usage: %s [--update] --feed-addr <host:port>\n' "$0" >&2
    exit 2
}

update=0
feed_addr=
while [ $# -gt 0 ]; do
    case "$1" in
    --update) update=1 ;;
    --feed-addr)
        [ $# -ge 2 ] || usage
        feed_addr=$2
        shift
        ;;
    *) usage ;;
    esac
    shift
done
[ -n "$feed_addr" ] || usage

if [ "$update" -eq 1 ]; then
    if ! sudo -n true 2>/dev/null; then
        printf '[deploy] service runner not updated: sudo needs a password\n'
        exit 0
    fi
    SUDO="sudo -n"
else
    SUDO="sudo"
fi

# Runs a command as the runner user with its own HOME, from `/`: the
# invoking user's checkout is not readable by that user.
as_user() { (cd / && $SUDO -u "$run_user" -H "$@"); }
# systemctl against the runner user's manager.
user_systemctl() { $SUDO systemctl --user -M "$run_user@" "$@"; }

if [ "$update" -eq 0 ]; then
    say "1/8 packages"
    $SUDO pacman -S --needed --noconfirm podman passt qemu-user-static qemu-user-static-binfmt

    say "2/8 storage"
    if [ ! -d "$run_home" ]; then
        $SUDO btrfs subvolume create "$run_home"
    fi

    say "3/8 user"
    if ! id "$run_user" >/dev/null 2>&1; then
        $SUDO useradd --system --home-dir "$run_home" --no-create-home \
            --shell /bin/bash --user-group "$run_user"
        $SUDO usermod --add-subuids 200000-265535 --add-subgids 200000-265535 "$run_user"
        $SUDO chown "$run_user:$run_user" "$run_home"
        $SUDO chmod 0750 "$run_home"
    fi
    $SUDO loginctl enable-linger "$run_user"

    say "4/8 resource limits"
    # QEMU, podman and every job live in this slice: the desktop keeps
    # priority even when no game is running.
    $SUDO systemctl set-property "user-$(id -u "$run_user").slice" \
        CPUWeight=20 IOWeight=20 MemoryHigh=32G

    say "5/8 directories, ssh"
    as_user mkdir -p "$run_home/.config/systemd/user" "$run_home/vm/win11"
    as_user install -d -m700 "$run_home/state" "$run_home/.ssh"
    # qcow2 files on btrfs need nodatacow (vm/win11/vm.sh); the flag only
    # takes effect on a directory that holds no image yet.
    if [ -z "$($SUDO ls -A "$run_home/vm/win11")" ]; then
        $SUDO chattr +C "$run_home/vm/win11"
    fi
    if ! $SUDO test -f "$run_home/.ssh/id_ed25519"; then
        as_user ssh-keygen -q -t ed25519 -N '' -f "$run_home/.ssh/id_ed25519"
    fi
fi

say "6/8 binary and scripts"
# Installed under a new name and renamed: the rename is atomic, so a reload
# that re-executes the path never finds a half-written file.
$SUDO install -Dm755 "$bindir/zeughaus-runner" "$lib/bin/zeughaus-runner.new"
$SUDO mv "$lib/bin/zeughaus-runner.new" "$lib/bin/zeughaus-runner"
for f in autounattend.xml setup.ps1 firstboot.ps1 toolchain.ps1 prepare.ps1; do
    $SUDO install -Dm644 "$root/vm/win11/$f" "$lib/vm/win11/$f"
done
$SUDO install -Dm755 "$root/vm/win11/vm.sh" "$lib/vm/win11/vm.sh"
$SUDO install -o "$run_user" -g "$run_user" -m600 "$root/deploy/runner/ssh_config" "$run_home/.ssh/config"

if [ "$update" -eq 0 ]; then
    say "7/8 trust"
    if [ -f "$client_pem" ]; then
        as_user install -d -m700 "$run_home/state/clients"
        openssl x509 -in "$client_pem" |
            $SUDO install -o "$run_user" -g "$run_user" -m600 /dev/stdin "$run_home/state/clients/$(id -un).pem"
    else
        say "no $client_pem; start the editor once and run this script again, or the editor is refused"
    fi
fi

say "8/8 unit"
units=$run_home/.config/systemd/user
unit=$(mktemp)
sed "s|@FEED_ADDR@|$feed_addr|" "$root/deploy/runner/zeughaus-runner.service" >"$unit"
$SUDO install -o "$run_user" -g "$run_user" -Dm644 "$unit" "$units/zeughaus-runner.service"
rm -f "$unit"
user_systemctl daemon-reload
if user_systemctl is-active --quiet zeughaus-runner.service; then
    # SIGUSR1: same PID, terminals stay.
    user_systemctl reload zeughaus-runner.service
else
    user_systemctl enable --now zeughaus-runner.service
fi

# Editors find remote runners through `remotes` in their zeughaus.toml. The
# runner writes its pinned URL at start; a reload that is still re-executing
# has not written it yet, and the next run of this script prints it.
editor_config=${XDG_STATE_HOME:-$HOME/.local/state}/zeughaus/zeughaus.toml
url=$($SUDO cat "$run_home/state/endpoint" 2>/dev/null || true)
if [ -n "$url" ] && ! grep -qF "$url" "$editor_config" 2>/dev/null; then
    say "add to $editor_config: remotes = [\"$url\"]"
fi
say "done"
