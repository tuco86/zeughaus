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

# The workspace builds weida from the sibling checkout `../weida`, at its
# main branch, as on the workstation. It is kept in the cache and fetched
# with the read-only token, which travels in the environment, not in argv.
weida="$cache/weida"
url=https://git.doodleshnookie.net/tuco86/weida.git
export GIT_CONFIG_COUNT=1
export GIT_CONFIG_KEY_0=http.extraHeader
GIT_CONFIG_VALUE_0="Authorization: Basic $(printf 'zeughaus-ci:%s' "$FORGEJO_READ_TOKEN" | base64 -w0)"
export GIT_CONFIG_VALUE_0
if [ -d "$weida/.git" ]; then
    git -C "$weida" fetch -q "$url" main
    git -C "$weida" checkout -q --force --detach FETCH_HEAD
else
    git clone -q --branch main "$url" "$weida"
fi
unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0 FORGEJO_READ_TOKEN
ln -sfn "$weida" "$CI_WORKSPACE/../weida"
echo "weida $(git -C "$weida" log --oneline -1)"

step() { printf '\n== %s\n' "$*"; "$@"; }
step cargo fmt --check
step cargo clippy --workspace --all-targets -- -D warnings
step cargo test --workspace
step cargo check --target wasm32-unknown-unknown -p zeughaus
