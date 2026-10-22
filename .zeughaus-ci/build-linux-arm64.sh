#!/bin/sh
# /// ci
# on = ["push main", "tag v*", "cron 0 3 * * *"]
# platform = "linux-arm64"
# image = "trixie"
# timeout_minutes = 120
# ///
# The linux-arm64 release archive, built natively in an Apple `container` on
# atik. Its zeughaus-runner is also the tool publish.sh bundles the release
# with, on sadala (aarch64).
set -eu

. "$CI_WORKSPACE/.zeughaus-ci/build.sh"
cargo_env
export ZEUGHAUS_BUILD="$CI_PIPELINE"

step cargo build --locked --release -p zeughaus -p zeughaus-runner -p zeughaus-ci
stage_dir
stage_linux_share
for bin in zeughaus zeughaus-runner zeughaus-ci; do
    cp "$CARGO_TARGET_DIR/release/$bin" "$stage/"
done
"$stage/zeughaus-runner" --help >/dev/null
step pack_tar
