#!/bin/sh
# Sets up the CI and a runner on sadala (aarch64 Ubuntu) over ssh, from a
# workstation. Each runs as its own system user with its own state, so the
# CI's secrets and the runner's jobs and shells never share a uid:
#
#   zeughaus-ci  /var/lib/zeughaus-ci  `zeughaus-ci serve` (weida 10.8.0.1:7444,
#                                      webhooks 127.0.0.1:8686 behind Caddy)
#   zeughaus     /var/lib/zeughaus     zeughaus-runner (10.8.0.1:7443),
#                                      enlisted as `sadala`
#
#   deploy/setup-sadala.sh --ci-toml <file>   first setup
#   deploy/setup-sadala.sh                    repeat: units, files, wrapper
#
# Both binaries come from the dev channel through deploy/bootstrap.sh, run
# as their own user, and update themselves from then on; nothing is built or
# copied from this checkout. Needs `sudo -n` on sadala. Idempotent.
#
#   1. with --ci-toml: users, linger, state directories, trust for this
#      workstation's editor certificate, ci.toml, and the CI's OpenBao
#      credential;
#   2. each user's bin directory (and lib, /var/www/zeughaus-updates); a
#      binary an older install left in /usr/local/lib/zeughaus moves to its
#      owner's bin, and the old path becomes a symlink to it, so a process
#      still running from there finds an executable;
#   3. the CI: bootstrap `ci`, then the wrapper /usr/local/bin/zeughaus-ci;
#   4. the runner: enlist.toml when --ci-toml and none exists, then bootstrap
#      `service`;
#   5. the old /usr/local/lib/zeughaus removed, both endpoints printed.
#
# Needs a client certificate of the editor in
# ${XDG_STATE_HOME:-~/.local/state}/zeughaus/client.pem with --ci-toml.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
host=sadala
old_lib=/usr/local/lib/zeughaus
ci_home=/var/lib/zeughaus-ci
run_home=/var/lib/zeughaus
client_pem=${XDG_STATE_HOME:-$HOME/.local/state}/zeughaus/client.pem

say() { printf '[sadala] %s\n' "$*"; }
usage() {
    printf 'usage: %s [--ci-toml <file>]\n' "$0" >&2
    exit 2
}

ci_toml=
while [ $# -gt 0 ]; do
    case "$1" in
    --ci-toml)
        [ $# -ge 2 ] || usage
        ci_toml=$2
        shift
        ;;
    *) usage ;;
    esac
    shift
done

remote() { ssh -o BatchMode=yes "$host" "$@"; }
upload() { scp -q -o BatchMode=yes "$1" "$host:$2"; }

# The modification time of a remote file, `none` while it does not exist.
mtime() { remote "sudo -n stat -c %.9Y $1 2>/dev/null || echo none"; }

# Waits up to 60 s until the endpoint file $1 is written again after $2.
wait_endpoint() {
    i=0
    while [ "$(mtime "$1")" = "$2" ]; do
        i=$((i + 1))
        if [ "$i" -gt 60 ]; then
            say "$1 was not written within 60 s" >&2
            remote "sudo -n journalctl _UID=\$(id -u $3) -n 40 --no-pager" >&2 || true
            exit 1
        fi
        sleep 1
    done
}

if [ -n "$ci_toml" ]; then
    say "1/5 users, state, trust"
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

    say "OpenBao credential"
    upload "$root/deploy/ci/openbao.sh" /tmp/zh-openbao.sh
    remote 'sudo -n sh /tmp/zh-openbao.sh; rc=$?; rm -f /tmp/zh-openbao.sh; exit $rc'
else
    say "1/5 users, OpenBao credential: kept"
fi

say "2/5 directories, old binaries"
remote 'sudo -n sh -s' <<EOF
set -eu
install -d -o zeughaus-ci -g zeughaus-ci -m755 $ci_home/bin
install -d -o zeughaus -g zeughaus -m755 $run_home/bin $run_home/lib /var/www/zeughaus-updates
# Only a regular file is an old install; a symlink is a finished migration.
# The symlink is renamed over the old path: the rename is atomic.
for entry in "zeughaus-ci zeughaus-ci $ci_home" "zeughaus-runner zeughaus $run_home"; do
    set -- \$entry
    old=$old_lib/bin/\$1
    if test -f "\$old" && ! test -L "\$old"; then
        install -o \$2 -g \$2 -m755 "\$old" "\$3/bin/\$1"
        ln -sfn "\$3/bin/\$1" "\$old.new"
        mv -f "\$old.new" "\$old"
    fi
done
EOF

say "3/5 CI"
upload "$root/deploy/bootstrap.sh" /tmp/zh-bootstrap.sh
ci_before=$(mtime $ci_home/state/endpoint)
remote 'sudo -n sh -s' <<EOF
set -eu
cd /
sudo -u zeughaus-ci -H env XDG_RUNTIME_DIR=/run/user/\$(id -u zeughaus-ci) sh /tmp/zh-bootstrap.sh ci
EOF
wait_endpoint $ci_home/state/endpoint "$ci_before" zeughaus-ci
remote 'sudo -n sh -s' <<EOF
set -eu
wrapper=\$(mktemp)
printf '#!/bin/sh\nexec sudo -u zeughaus-ci $ci_home/bin/zeughaus-ci --state-dir $ci_home/state "\$@"\n' >"\$wrapper"
install -m755 "\$wrapper" /usr/local/bin/zeughaus-ci
rm -f "\$wrapper"
EOF

say "4/5 runner"
if [ -n "$ci_toml" ]; then
    remote 'sudo -n sh -s' <<EOF
set -eu
if ! test -e $run_home/state/enlist.toml; then
    enlist=\$(mktemp)
    printf 'ci = "%s"\nname = "sadala"\n' "\$(cat $ci_home/state/endpoint)" >"\$enlist"
    install -o zeughaus -g zeughaus -m600 "\$enlist" $run_home/state/enlist.toml
    rm -f "\$enlist"
fi
EOF
fi
run_before=$(mtime $run_home/state/endpoint)
remote 'sudo -n sh -s' <<EOF
set -eu
cd /
sudo -u zeughaus -H env XDG_RUNTIME_DIR=/run/user/\$(id -u zeughaus) sh /tmp/zh-bootstrap.sh service --feed-addr 10.8.0.1:7443
EOF
wait_endpoint $run_home/state/endpoint "$run_before" zeughaus

say "5/5 endpoints"
remote "sudo -n sh -c 'rm -rf $old_lib /tmp/zh-bootstrap.sh; printf \"ci:     %s\n\" \"\$(cat $ci_home/state/endpoint)\"; printf \"runner: %s\n\" \"\$(cat $run_home/state/endpoint)\"'"
say "done"
