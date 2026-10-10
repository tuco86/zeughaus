#!/bin/sh
# /// ci
# on = ["push *", "tag v*"]
# image = "arch"
# cache = [".ci-cache"]
# secrets = ["FORGEJO_READ_TOKEN"]
# when_busy = "freeze"
# timeout_minutes = 120
# ///
# The gate from CLAUDE.md: formatting, clippy, the tests, and the editor's
# wasm build.
set -eu

cache="$CI_WORKSPACE/.ci-cache"
export CARGO_HOME="$cache/cargo"
export CARGO_TARGET_DIR="$cache/target"
export CARGO_TERM_COLOR=always

. "$CI_WORKSPACE/.zeughaus-ci/weida.sh"

step() { printf '\n== %s\n' "$*"; "$@"; }
step cargo fmt --check
step cargo clippy --workspace --all-targets -- -D warnings
step cargo test --workspace
step cargo check --target wasm32-unknown-unknown -p zeughaus
