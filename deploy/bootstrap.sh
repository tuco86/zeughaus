#!/bin/sh
# Installs zeughaus on this host from a release channel, once; every later
# update comes from the installed runner (or CI) itself.
#
#   sh deploy/bootstrap.sh <desktop|macos|service|ci> [--channel C] [--feed-addr A] [--url U]
#   sh -s -- service --feed-addr A < deploy/bootstrap.sh
#
# It reads no file of the checkout, so it also runs from stdin, as the user
# the install is for:
#
#   1. platform from `uname`, build number from `<url>/<channel>/latest`;
#   2. SHA256SUMS of that build, and the archive for this platform, checked
#      against its line;
#   3. the archive unpacked into a temporary directory;
#   4. `<top>/zeughaus-runner install <profile> ...` (`zeughaus-ci` for the
#      profile `ci`), which puts the binaries, units and desktop files in
#      place, starts the service and writes update.toml.
#
# This is the only download that is not checked against a signature: it
# trusts HTTPS to our own host. Every later update is checked against the
# key compiled into the binary installed here.
#
# --channel defaults to dev, --url to https://zeughaus.doodleshnookie.net/updates;
# --feed-addr is handed to `install` (required for `service`, see
# `zeughaus-runner install`). The profiles `service` and `ci` use ~/state as
# the state directory, as their units do.
set -eu

url=https://zeughaus.doodleshnookie.net/updates
channel=dev
feed_addr=
profile=

say() { printf '[bootstrap] %s\n' "$*"; }
usage() {
    printf 'usage: sh bootstrap.sh <desktop|macos|service|ci> [--channel C] [--feed-addr A] [--url U]\n' >&2
    exit 2
}

[ $# -ge 1 ] || usage
case "$1" in
desktop | macos | service | ci) profile=$1 ;;
*) usage ;;
esac
shift
while [ $# -gt 0 ]; do
    [ $# -ge 2 ] || usage
    case "$1" in
    --channel) channel=$2 ;;
    --feed-addr) feed_addr=$2 ;;
    --url) url=$2 ;;
    *) usage ;;
    esac
    shift 2
done

case "$(uname -s)/$(uname -m)" in
Linux/x86_64) platform=linux-amd64 ;;
Linux/aarch64) platform=linux-arm64 ;;
Darwin/arm64) platform=darwin-arm64 ;;
*)
    say "no release for $(uname -s) $(uname -m)" >&2
    exit 1
    ;;
esac

if [ "$profile" = ci ]; then
    binary=zeughaus-ci
else
    binary=zeughaus-runner
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

say "channel $channel at $url"
build=$(curl -fsS "$url/$channel/latest")
build=$(printf '%s' "$build" | tr -d '[:space:]')
[ -n "$build" ] || {
    say "$url/$channel/latest is empty" >&2
    exit 1
}
curl -fsS -o "$tmp/SHA256SUMS" "$url/$channel/$build/SHA256SUMS"
line=$(grep -- "-$platform\.tar\.gz\$" "$tmp/SHA256SUMS" || true)
[ -n "$line" ] || {
    say "build $build has no archive for $platform" >&2
    exit 1
}
file=$(printf '%s\n' "$line" | awk 'NR == 1 { print $2 }')
file=${file#\*}
say "build $build: $file"
curl -fsS -o "$tmp/$file" "$url/$channel/$build/$file"
printf '%s\n' "$line" | head -n 1 >"$tmp/line"
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$tmp" && sha256sum -c line)
else
    (cd "$tmp" && shasum -a 256 -c line)
fi

mkdir "$tmp/unpacked"
tar -xf "$tmp/$file" -C "$tmp/unpacked"
top=$tmp/unpacked/${file%.tar.gz}
[ -x "$top/$binary" ] || {
    say "the archive has no $top/$binary" >&2
    exit 1
}

# The units of the service runner and the CI keep their state in ~/state;
# the other profiles use the default state directory.
case "$profile" in
service | ci) set -- --state-dir "$HOME/state" install "$profile" --channel "$channel" ;;
*) set -- install "$profile" --channel "$channel" ;;
esac
if [ -n "$feed_addr" ]; then
    set -- "$@" --feed-addr "$feed_addr"
fi
"$top/$binary" "$@"
