#!/bin/sh
# /// ci
# on = ["push main", "tag v*", "nightly"]
# platform = "darwin-arm64"
# timeout_minutes = 120
# ///
# The darwin-arm64 release archive, built on atik's host: the editor as
# Zeughaus.app and the runner. publish.sh bundles the archive; the updater
# reads the app from it.
set -eu

. "$CI_WORKSPACE/.zeughaus-ci/build.sh"
cargo_env

triple=aarch64-apple-darwin
stage_dir
stage_share deploy/macos/net.doodleshnookie.zeughaus-runner.plist
step sh deploy/macos-app.sh "$stage" "$triple"
cp "$CARGO_TARGET_DIR/$triple/release/zeughaus-runner" "$stage/"
step stamp_build "$CARGO_TARGET_DIR/$triple/release/zeughaus-runner" "$stage/zeughaus-runner"
step pack_tar
