#!/bin/bash
# /// ci
# needs = ["check-omp-plugin"]
# channels = ["dev"]
# platform = "linux-arm64"
# image = "js"
# secrets = ["FORGEJO_NPM_TOKEN"]
# timeout_minutes = 10
# ///
# Publishes @tuco86/zeughaus-omp to the Forgejo npm registry from a push
# to main whose omp-plugin/package.json names a version the registry does
# not have yet; any other push leaves the registry alone. A manual run
# (`run_jobs`) runs `npm publish --dry-run`.
set -euo pipefail

. "$CI_WORKSPACE/.zeughaus-ci/build.sh"
registry=https://git.doodleshnookie.net/api/packages/tuco86/npm/
cd omp-plugin
version=$(node -p 'require("./package.json").version')

if [ "$CI_EVENT" = manual ]; then
    step npm publish --dry-run --tag latest --registry "$registry"
    exit 0
fi
if [ "$CI_EVENT" != push ]; then
    echo "publish runs for a push to main only"
    exit 1
fi
: "${FORGEJO_NPM_TOKEN:?secret FORGEJO_NPM_TOKEN is not available to this event}"

if npm view "@tuco86/zeughaus-omp@$version" version --registry "$registry" 2>/dev/null | grep -qx "$version"; then
    echo "@tuco86/zeughaus-omp $version is already in the registry"
    exit 0
fi

npmrc=$(mktemp)
trap 'rm -f "$npmrc"' EXIT
printf '//git.doodleshnookie.net/api/packages/tuco86/npm/:_authToken=%s\n' "$FORGEJO_NPM_TOKEN" >"$npmrc"
step env NPM_CONFIG_USERCONFIG="$npmrc" npm publish --tag latest --registry "$registry"
