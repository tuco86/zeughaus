#!/bin/sh
# /// ci
# on = ["push main", "tag v*", "cron 0 3 * * *"]
# platform = "darwin-amd64"
# cache = [".ci-cache"]
# secrets = ["FORGEJO_READ_TOKEN"]
# timeout_minutes = 120
# ///
# The darwin-amd64 release archive, cross built on atik's host (Apple
# silicon): the editor as Zeughaus.app and the runner. The smoke run needs
# Rosetta.
set -eu

. "$CI_WORKSPACE/.zeughaus-ci/build.sh"
cargo_env
. "$CI_WORKSPACE/.zeughaus-ci/weida.sh"

triple=x86_64-apple-darwin
stage_dir
sh deploy/macos-app.sh "$stage" "$triple"
cargo build --locked --release --target "$triple" -p zeughaus-runner
cp "$CARGO_TARGET_DIR/$triple/release/zeughaus-runner" "$stage/"
arch -x86_64 "$stage/zeughaus-runner" --help >/dev/null
pack_tar
