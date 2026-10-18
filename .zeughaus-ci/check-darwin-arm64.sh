#!/bin/sh
# /// ci
# needs = ["build-darwin-arm64"]
# platform = "darwin-arm64"
# secrets = ["FORGEJO_READ_TOKEN"]
# timeout_minutes = 120
# ///
# clippy and the tests on atik's host, in the workspace the build left warm.
set -eu

. "$CI_WORKSPACE/.zeughaus-ci/build.sh"
cargo_env
. "$CI_WORKSPACE/.zeughaus-ci/weida.sh"

check_platform
