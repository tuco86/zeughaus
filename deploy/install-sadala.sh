#!/bin/sh
# Installs the CI and a runner on sadala (aarch64 Ubuntu), from the x86_64
# workstation over ssh. Each runs as its own system user with its own state,
# so the CI's secrets and the runner's jobs and shells never share a uid:
#
#   zeughaus-ci  /var/lib/zeughaus-ci  `zeughaus-ci serve` (weida 10.8.0.1:7444,
#                                      webhooks 127.0.0.1:8686 behind Caddy)
#   zeughaus     /var/lib/zeughaus     zeughaus-runner (10.8.0.1:7443),
#                                      enlisted as `sadala`
#
#   deploy/install-sadala.sh --setup --ci-toml <file>   first install
#   deploy/install-sadala.sh                            update
#
# An update builds, replaces both binaries, restarts the CI and reloads the
# runner. Needs `sudo -n` on sadala.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
host=sadala
lib=/usr/local/lib/zeughaus
ci_home=/var/lib/zeughaus-ci
run_home=/var/lib/zeughaus
client_pem=${XDG_STATE_HOME:-$HOME/.local/state}/zeughaus/client.pem
out=$root/target/aarch64-linux/release

say() { printf '[sadala] %s\n' "$*"; }
usage() {
    printf 'usage: %s [--setup --ci-toml <file>]\n' "$0" >&2
    exit 2
}

setup=0
ci_toml=
while [ $# -gt 0 ]; do
    case "$1" in
    --setup) setup=1 ;;
    --ci-toml)
        [ $# -ge 2 ] || usage
        ci_toml=$2
        shift
        ;;
    *) usage ;;
    esac
    shift
done
if [ "$setup" -eq 1 ] && [ -z "$ci_toml" ]; then
    usage
fi

remote() { ssh -o BatchMode=yes "$host" "$@"; }
upload() { scp -q -o BatchMode=yes "$1" "$host:$2"; }

say "1/7 build"
sh "$root/deploy/build-aarch64.sh"

say "2/7 binaries"
# Installed under a new name and renamed: the rename is atomic, so a runner
# that re-executes its path on SIGUSR1 never finds a half-written file.
for b in zeughaus-runner zeughaus-ci; do
    upload "$out/$b" "/tmp/$b"
    remote "sudo -n install -Dm755 /tmp/$b $lib/bin/$b.new && sudo -n mv $lib/bin/$b.new $lib/bin/$b && rm -f /tmp/$b"
done

if [ "$setup" -eq 1 ]; then
    say "3/7 users, state, trust"
    [ -f "$client_pem" ] || {
        say "no $client_pem; start the editor once first"
        exit 1
    }
    editor_pem=$(mktemp)
    openssl x509 -in "$client_pem" >"$editor_pem"
    upload "$editor_pem" /tmp/zh-editor.pem
    rm -f "$editor_pem"
    upload "$ci_toml" /tmp/zh-ci.toml
    remote 'sudo -n sh -s' <<EOF
set -eu
id zeughaus-ci >/dev/null 2>&1 ||
    useradd --system --home-dir $ci_home --create-home --shell /usr/sbin/nologin --user-group zeughaus-ci
loginctl enable-linger zeughaus-ci
install -d -o zeughaus-ci -g zeughaus-ci -m700 $ci_home/state $ci_home/state/clients
install -o zeughaus-ci -g zeughaus-ci -m600 /tmp/zh-ci.toml $ci_home/state/ci.toml
install -o zeughaus-ci -g zeughaus-ci -m600 /tmp/zh-editor.pem $ci_home/state/clients/hannes.pem
# bash: the runner's terminals start the user's login shell.
id zeughaus >/dev/null 2>&1 ||
    useradd --system --home-dir $run_home --create-home --shell /bin/bash --user-group zeughaus
loginctl enable-linger zeughaus
install -d -o zeughaus -g zeughaus -m700 $run_home/state $run_home/state/clients
install -o zeughaus -g zeughaus -m600 /tmp/zh-editor.pem $run_home/state/clients/hannes.pem
rm -f /tmp/zh-ci.toml /tmp/zh-editor.pem
EOF

    say "4/7 OpenBao credential"
    upload "$root/deploy/ci/openbao.sh" /tmp/zh-openbao.sh
    remote 'sudo -n sh /tmp/zh-openbao.sh; status=$?; rm -f /tmp/zh-openbao.sh; exit $status'
else
    say "3/7 users: kept"
    say "4/7 OpenBao credential: kept"
fi

say "5/7 CI service"
upload "$root/deploy/ci/zeughaus-ci.service" /tmp/zeughaus-ci.service
remote 'sudo -n sh -s' <<EOF
set -eu
sudo -u zeughaus-ci mkdir -p $ci_home/.config/systemd/user
install -o zeughaus-ci -g zeughaus-ci -m644 /tmp/zeughaus-ci.service $ci_home/.config/systemd/user/zeughaus-ci.service
rm -f /tmp/zeughaus-ci.service
systemctl --user -M zeughaus-ci@ daemon-reload
if systemctl --user -M zeughaus-ci@ is-active --quiet zeughaus-ci.service; then
    systemctl --user -M zeughaus-ci@ restart zeughaus-ci.service
else
    systemctl --user -M zeughaus-ci@ enable --now zeughaus-ci.service
fi
i=0
until test -s $ci_home/state/endpoint; do
    i=\$((i + 1))
    if [ "\$i" -gt 30 ]; then
        echo "[sadala] the CI wrote no endpoint within 30 s" >&2
        journalctl _UID=\$(id -u zeughaus-ci) -n 40 --no-pager >&2
        exit 1
    fi
    sleep 1
done
wrapper=\$(mktemp)
printf '#!/bin/sh\nexec sudo -u zeughaus-ci $lib/bin/zeughaus-ci --state-dir $ci_home/state "\$@"\n' >"\$wrapper"
install -m755 "\$wrapper" /usr/local/bin/zeughaus-ci
rm -f "\$wrapper"
EOF

say "6/7 runner"
unit=$(mktemp)
sed "s|@FEED_ADDR@|10.8.0.1:7443|" "$root/deploy/runner/zeughaus-runner.service" >"$unit"
upload "$unit" /tmp/zeughaus-runner.service
rm -f "$unit"
remote 'sudo -n sh -s' <<EOF
set -eu
if [ "$setup" -eq 1 ] && ! test -e $run_home/state/enlist.toml; then
    enlist=\$(mktemp)
    printf 'ci = "%s"\nname = "sadala"\n' "\$(cat $ci_home/state/endpoint)" >"\$enlist"
    install -o zeughaus -g zeughaus -m600 "\$enlist" $run_home/state/enlist.toml
    rm -f "\$enlist"
fi
sudo -u zeughaus mkdir -p $run_home/.config/systemd/user
install -o zeughaus -g zeughaus -m644 /tmp/zeughaus-runner.service $run_home/.config/systemd/user/zeughaus-runner.service
rm -f /tmp/zeughaus-runner.service
systemctl --user -M zeughaus@ daemon-reload
if systemctl --user -M zeughaus@ is-active --quiet zeughaus-runner.service; then
    systemctl --user -M zeughaus@ reload zeughaus-runner.service
else
    systemctl --user -M zeughaus@ enable --now zeughaus-runner.service
fi
EOF

say "7/7 endpoints"
remote "sudo -n sh -c 'printf \"ci:     %s\n\" \"\$(cat $ci_home/state/endpoint)\"; sleep 2; printf \"runner: %s\n\" \"\$(cat $run_home/state/endpoint 2>/dev/null || echo not written yet)\"'"
say "done"
