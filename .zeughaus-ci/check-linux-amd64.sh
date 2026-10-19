#!/bin/sh
# /// ci
# needs = ["build-linux-amd64"]
# platform = "linux-amd64"
# image = "trixie"
# secrets = ["FORGEJO_READ_TOKEN"]
# when_busy = "freeze"
# timeout_minutes = 120
# ///
# The gate from AGENTS.md (formatting) and what every platform checks:
# clippy and the tests.
set -eu

. "$CI_WORKSPACE/.zeughaus-ci/build.sh"
cargo_env
. "$CI_WORKSPACE/.zeughaus-ci/weida.sh"

step cargo fmt --check
check_platform
