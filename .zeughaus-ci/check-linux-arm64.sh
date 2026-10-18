#!/bin/sh
# /// ci
# needs = ["build-linux-arm64"]
# platform = "linux-arm64"
# image = "trixie"
# secrets = ["FORGEJO_READ_TOKEN"]
# timeout_minutes = 120
# ///
# clippy and the tests on linux-arm64, in the workspace the build left warm.
set -eu

. "$CI_WORKSPACE/.zeughaus-ci/build.sh"
cargo_env
. "$CI_WORKSPACE/.zeughaus-ci/weida.sh"

check_platform
