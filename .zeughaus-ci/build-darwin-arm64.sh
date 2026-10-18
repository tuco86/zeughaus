#!/bin/sh
# /// ci
# on = ["push main", "tag v*", "cron 0 3 * * *"]
# platform = "darwin-arm64"
# secrets = ["FORGEJO_READ_TOKEN"]
# timeout_minutes = 120
# ///
# The darwin-arm64 release archive, built on atik's host: the editor as
# Zeughaus.app and the runner. publish.sh bundles the archive; the updater
# reads the app from it.
set -eu

. "$CI_WORKSPACE/.zeughaus-ci/build.sh"
cargo_env
export ZEUGHAUS_BUILD="$CI_PIPELINE"
. "$CI_WORKSPACE/.zeughaus-ci/weida.sh"

triple=aarch64-apple-darwin
stage_dir
stage_share deploy/macos/net.doodleshnookie.zeughaus-runner.plist
step sh deploy/macos-app.sh "$stage" "$triple"
step cargo build --locked --release --target "$triple" -p zeughaus-runner
cp "$CARGO_TARGET_DIR/$triple/release/zeughaus-runner" "$stage/"
"$stage/zeughaus-runner" --help >/dev/null
step pack_tar
