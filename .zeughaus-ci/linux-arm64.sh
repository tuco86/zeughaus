#!/bin/sh
# /// ci
# on = ["push main", "tag v*", "cron 0 3 * * *"]
# platform = "linux-arm64"
# image = "trixie"
# cache = [".ci-cache"]
# secrets = ["FORGEJO_READ_TOKEN"]
# timeout_minutes = 120
# ///
# The linux-arm64 release archive, built natively in an Apple `container` on
# atik. Its zeughaus-runner is also the tool publish.sh bundles the release
# with, on sadala (aarch64).
set -eu

. "$CI_WORKSPACE/.zeughaus-ci/build.sh"
cargo_env
. "$CI_WORKSPACE/.zeughaus-ci/weida.sh"

cargo build --locked --release -p zeughaus -p zeughaus-runner -p zeughaus-ci
stage_dir
for bin in zeughaus zeughaus-runner zeughaus-ci; do
    cp "$CARGO_TARGET_DIR/release/$bin" "$stage/"
done
"$stage/zeughaus-runner" --help >/dev/null
pack_tar
