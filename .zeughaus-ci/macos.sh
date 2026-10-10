#!/bin/sh
# /// ci
# on = ["push main", "tag v*"]
# machine = "atik"
# cache = [".ci-cache"]
# secrets = ["FORGEJO_READ_TOKEN"]
# timeout_minutes = 90
# ///
# The editor as Zeughaus.app for Apple silicon, built on atik and zipped into
# the job's output, where a job that `needs` it picks it up to publish.
set -eu
cache="$CI_WORKSPACE/.ci-cache"
export CARGO_HOME="$cache/cargo"
export CARGO_TARGET_DIR="$cache/target"
export CARGO_TERM_COLOR=always
. "$CI_WORKSPACE/.zeughaus-ci/weida.sh"
rm -rf "$cache/app"
sh deploy/macos-app.sh "$cache/app"
ditto -c -k --keepParent "$cache/app/Zeughaus.app" "$CI_OUTPUT/Zeughaus-macos-arm64.zip"
