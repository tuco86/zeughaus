#!/bin/sh
# Sets up the CI runner of this workstation: a second zeughaus-runner that
# runs as its own Unix user `zeughaus-ci`, receives the webhooks and executes
# the pipelines of the configured repositories. Run as the invoking user;
# everything that needs root goes through sudo. Idempotent.
#
#   1. packages (podman, passt, qemu-user-static);
#   2. /var/lib/zeughaus-ci as its own btrfs subvolume, so snapper's
#      snapshots of `@` do not pin the CI caches;
#   3. the user, its subordinate ids and linger;
#   4. resource limits of the user's slice;
#   5. the user's directories, ssh key and ssh config;
#   6. the runner binary, the VM scripts and the `zeughaus-ci` wrapper;
#   7. trust for the invoking user's editor certificate;
#   8. a ci.toml from the example, when none exists;
#   9. the user units, started or reloaded.
#
# `--update` does only 6 and 9, with `sudo -n`: deploy/install.sh calls it
# after installing the binaries, and it must never wait for a password.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
ci_user=zeughaus-ci
ci_home=/var/lib/zeughaus-ci
lib=/usr/local/lib/zeughaus-ci
bindir=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin
client_pem=${XDG_STATE_HOME:-$HOME/.local/state}/zeughaus/client.pem

say() { printf '[ci-install] %s\n' "$*"; }

update=0
case "${1:-}" in
--update) update=1 ;;
"") ;;
*)
    printf 'usage: %s [--update]\n' "$0" >&2
    exit 2
    ;;
esac

if [ "$update" -eq 1 ]; then
    if ! sudo -n true 2>/dev/null; then
        printf '[deploy] CI runner not updated: sudo needs a password\n'
        exit 0
    fi
    SUDO="sudo -n"
else
    SUDO="sudo"
fi

# Runs a command as the CI user with its own HOME, from `/`: the invoking
# user's checkout is not readable by the CI user.
as_ci() { (cd / && $SUDO -u "$ci_user" -H "$@"); }
# systemctl against the CI user's manager.
ci_systemctl() { $SUDO systemctl --user -M "$ci_user@" "$@"; }

if [ "$update" -eq 0 ]; then
    say "1/9 packages"
    $SUDO pacman -S --needed --noconfirm podman passt qemu-user-static qemu-user-static-binfmt

    say "2/9 storage"
    if [ ! -d "$ci_home" ]; then
        $SUDO btrfs subvolume create "$ci_home"
    fi

    say "3/9 user"
    if ! id "$ci_user" >/dev/null 2>&1; then
        $SUDO useradd --system --home-dir "$ci_home" --no-create-home \
            --shell /bin/bash --user-group "$ci_user"
        $SUDO usermod --add-subuids 200000-265535 --add-subgids 200000-265535 "$ci_user"
        $SUDO chown "$ci_user:$ci_user" "$ci_home"
        $SUDO chmod 0750 "$ci_home"
        $SUDO loginctl enable-linger "$ci_user"
    fi

    say "4/9 resource limits"
    # QEMU, podman and every job live in this slice: the desktop keeps
    # priority even when no game is running.
    $SUDO systemctl set-property "user-$(id -u "$ci_user").slice" \
        CPUWeight=20 IOWeight=20 MemoryHigh=32G

    say "5/9 directories, ssh"
    as_ci mkdir -p "$ci_home/.config/systemd/user" "$ci_home/vm/win11"
    as_ci install -d -m700 "$ci_home/state" "$ci_home/state/secrets" "$ci_home/.ssh"
    # qcow2 files on btrfs need nodatacow (vm/win11/vm.sh); the flag only
    # takes effect on a directory that holds no image yet.
    if [ -z "$($SUDO ls -A "$ci_home/vm/win11")" ]; then
        $SUDO chattr +C "$ci_home/vm/win11"
    fi
    if [ ! -f "$ci_home/.ssh/id_ed25519" ]; then
        as_ci ssh-keygen -q -t ed25519 -N '' -f "$ci_home/.ssh/id_ed25519"
    fi
    $SUDO install -o "$ci_user" -g "$ci_user" -m600 "$root/deploy/ci/ssh_config" "$ci_home/.ssh/config"
fi

say "6/9 binary and scripts"
$SUDO install -Dm755 "$bindir/zeughaus-runner" "$lib/bin/zeughaus-runner"
for f in autounattend.xml setup.ps1 firstboot.ps1 toolchain.ps1 prepare.ps1; do
    $SUDO install -Dm644 "$root/vm/win11/$f" "$lib/vm/win11/$f"
done
$SUDO install -Dm755 "$root/vm/win11/vm.sh" "$lib/vm/win11/vm.sh"
wrapper=$(mktemp)
cat >"$wrapper" <<EOF
#!/bin/sh
exec sudo -u $ci_user $lib/bin/zeughaus-runner --state-dir $ci_home/state ci "\$@"
EOF
$SUDO install -Dm755 "$wrapper" /usr/local/bin/zeughaus-ci
rm -f "$wrapper"

if [ "$update" -eq 0 ]; then
    say "7/9 trust"
    if [ -f "$client_pem" ]; then
        as_ci install -d -m700 "$ci_home/state/clients"
        openssl x509 -in "$client_pem" |
            $SUDO install -o "$ci_user" -g "$ci_user" -m600 /dev/stdin "$ci_home/state/clients/$(id -un).pem"
    else
        say "no $client_pem; start the editor once and run this script again, or the editor is refused"
    fi
fi

# Before the units: the runner reads ci.toml at start, and one started
# without it runs no CI until its next restart.
if [ "$update" -eq 0 ]; then
    say "8/9 config"
    if ! $SUDO test -e "$ci_home/state/ci.toml"; then
        $SUDO install -o "$ci_user" -g "$ci_user" -m600 "$root/deploy/ci/ci.toml.example" "$ci_home/state/ci.toml"
    fi
fi

say "9/9 units"
units=$ci_home/.config/systemd/user
$SUDO install -o "$ci_user" -g "$ci_user" -Dm644 "$root/deploy/ci/zeughaus-ci-runner.service" "$units/zeughaus-ci-runner.service"
$SUDO install -o "$ci_user" -g "$ci_user" -Dm644 "$root/deploy/ci/zeughaus-ci-hook.service" "$units/zeughaus-ci-hook.service"
ci_systemctl daemon-reload
if ci_systemctl is-active --quiet zeughaus-ci-runner.service; then
    # SIGUSR1: same PID, terminals stay.
    ci_systemctl reload zeughaus-ci-runner.service
else
    ci_systemctl enable --now zeughaus-ci-runner.service
fi
if ci_systemctl is-active --quiet zeughaus-ci-hook.service; then
    ci_systemctl restart zeughaus-ci-hook.service
else
    ci_systemctl enable --now zeughaus-ci-hook.service
fi
say "done"
