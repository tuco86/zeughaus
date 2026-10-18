#!/bin/sh
# /// ci
# on = ["push main", "tag v*", "cron 0 3 * * *"]
# platform = "linux-amd64"
# image = "trixie"
# secrets = ["FORGEJO_READ_TOKEN"]
# when_busy = "freeze"
# timeout_minutes = 120
# ///
# The linux-amd64 release archive: editor, runner and CI binaries built in
# the trixie image, packed into the job's output for publish.sh.
set -eu

. "$CI_WORKSPACE/.zeughaus-ci/build.sh"
cargo_env
export ZEUGHAUS_BUILD="$CI_PIPELINE"
. "$CI_WORKSPACE/.zeughaus-ci/weida.sh"

step cargo build --locked --release -p zeughaus -p zeughaus-runner -p zeughaus-ci
stage_dir
for bin in zeughaus zeughaus-runner zeughaus-ci; do
    cp "$CARGO_TARGET_DIR/release/$bin" "$stage/"
done
"$stage/zeughaus-runner" --help >/dev/null
step pack_tar
