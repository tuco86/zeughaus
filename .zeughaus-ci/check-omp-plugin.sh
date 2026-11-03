#!/bin/sh
# /// ci
# on = ["push main", "tag omp-plugin-v*"]
# platform = "linux-arm64"
# image = "js"
# timeout_minutes = 10
# ///
# The omp plugin: its locked dependencies, its unit tests (the shared
# fixtures included), and that the prebuilt weida addon loads.
set -eu

. "$CI_WORKSPACE/.zeughaus-ci/build.sh"
cd omp-plugin
step bun install --frozen-lockfile
step bun test
step bun -e 'const w = await import("@tuco86/weida"); console.log(w.version())'
