#!/bin/sh
# Sets up the service runner of this workstation: a second zeughaus-runner
# that runs as its own Unix user `zeughaus` and executes CI jobs in rootless
# containers and the Windows VM once it is enlisted into a CI
# (`<state>/enlist.toml`, written by hand: it needs the CI's URL). Run as the
# invoking user; everything that needs root goes through sudo. Idempotent.
#
#   deploy/setup-service-runner.sh --feed-addr <host:port>
#
#   1. packages (podman, passt, qemu-user-static);
#   2. /var/lib/zeughaus as its own btrfs subvolume, so snapper's snapshots
#      of `@` do not pin the job caches;
#   3. the user, its subordinate ids and linger;
#   4. resource limits of the user's slice;
#   5. the user's directories and ssh key;
#   6. trust for the invoking user's editor certificate;
#   7. the user's bin and lib directories; a runner binary an older install
#      left in /usr/local/lib/zeughaus moves to the user's bin, and the old
#      path becomes a symlink to it, so the running runner's reload still
#      finds an executable;
#   8. deploy/bootstrap.sh as the user: the latest dev build, its unit, the
#      VM scripts and the ssh config of the VM guest, and the service started
#      or reloaded. From then on the runner updates itself.
#
# Nothing is built or copied from this checkout except bootstrap.sh, which is
# fed to the user's shell on stdin: the checkout is not readable by that user.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
run_user=zeughaus
run_home=/var/lib/zeughaus
old_lib=/usr/local/lib/zeughaus
client_pem=${XDG_STATE_HOME:-$HOME/.local/state}/zeughaus/client.pem

say() { printf '[service-runner] %s\n' "$*"; }
usage() {
    printf 'usage: %s --feed-addr <host:port>\n' "$0" >&2
    exit 2
}

feed_addr=
while [ $# -gt 0 ]; do
    case "$1" in
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

SUDO="sudo"

# Runs a command as the runner user with its own HOME, from `/`: the
# invoking user's checkout is not readable by that user.
as_user() { (cd / && $SUDO -u "$run_user" -H "$@"); }

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

say "6/8 trust"
if [ -f "$client_pem" ]; then
    as_user install -d -m700 "$run_home/state/clients"
    openssl x509 -in "$client_pem" |
        $SUDO install -o "$run_user" -g "$run_user" -m600 /dev/stdin "$run_home/state/clients/$(id -un).pem"
else
    say "no $client_pem; start the editor once and run this script again, or the editor is refused"
fi

say "7/8 bin and lib directories"
$SUDO install -d -o "$run_user" -g "$run_user" -m755 "$run_home/bin" "$run_home/lib"
# Only a regular file is an old install; a symlink is a finished migration.
if $SUDO test -f "$old_lib/bin/zeughaus-runner" && ! $SUDO test -L "$old_lib/bin/zeughaus-runner"; then
    $SUDO install -o "$run_user" -g "$run_user" -m755 "$old_lib/bin/zeughaus-runner" "$run_home/bin/zeughaus-runner"
    # Renamed over the old path: the rename is atomic, so a reload that
    # re-executes the path never finds it missing.
    $SUDO ln -sfn "$run_home/bin/zeughaus-runner" "$old_lib/bin/zeughaus-runner.new"
    $SUDO mv -f "$old_lib/bin/zeughaus-runner.new" "$old_lib/bin/zeughaus-runner"
    if $SUDO test -f "$run_home/state/enlist.toml"; then
        $SUDO sed -i 's|/usr/local/lib/zeughaus/vm/win11/vm.sh|/var/lib/zeughaus/lib/vm/win11/vm.sh|' \
            "$run_home/state/enlist.toml"
    fi
fi

say "8/8 install from the dev channel"
endpoint=$run_home/state/endpoint
mtime() { $SUDO stat -c %.9Y "$endpoint" 2>/dev/null || echo none; }
before=$(mtime)
# `-s -- ...` makes sh read the script from stdin, which stays this user's
# own file descriptor, so the user needs no access to the checkout.
(cd / && $SUDO -u "$run_user" -H env "XDG_RUNTIME_DIR=/run/user/$(id -u "$run_user")" \
    sh -s -- service --feed-addr "$feed_addr" <"$root/deploy/bootstrap.sh")

i=0
while [ "$(mtime)" = "$before" ]; do
    i=$((i + 1))
    if [ "$i" -gt 60 ]; then
        say "the runner wrote no endpoint within 60 s" >&2
        $SUDO journalctl "_UID=$(id -u "$run_user")" -n 40 --no-pager >&2 || true
        exit 1
    fi
    sleep 1
done
if $SUDO test -e "$old_lib"; then
    $SUDO rm -rf "$old_lib"
fi

# Editors find remote runners through `remotes` in their zeughaus.toml.
editor_config=${XDG_STATE_HOME:-$HOME/.local/state}/zeughaus/zeughaus.toml
url=$($SUDO cat "$endpoint" 2>/dev/null || true)
if [ -n "$url" ] && ! grep -qF "$url" "$editor_config" 2>/dev/null; then
    say "add to $editor_config: remotes = [\"$url\"]"
fi
say "done"
