#!/bin/bash
# /// ci
# needs = ["check-omp-plugin"]
# channels = ["omp-plugin"]
# platform = "linux-arm64"
# image = "js"
# secrets = ["FORGEJO_NPM_TOKEN"]
# timeout_minutes = 10
# ///
# Publishes @tuco86/zeughaus-omp to the Forgejo npm registry for an
# omp-plugin-v<version> tag. A version the registry already has is left
# alone, so a rerun is safe. A manual run (`run_jobs`) runs
# `npm publish --dry-run`.
set -euo pipefail

. "$CI_WORKSPACE/.zeughaus-ci/build.sh"
registry=https://git.doodleshnookie.net/api/packages/tuco86/npm/
cd omp-plugin
version=$(node -p 'require("./package.json").version')

if [ "$CI_EVENT" = manual ]; then
    step npm publish --dry-run --tag latest --registry "$registry"
    exit 0
fi
if [ "$CI_EVENT" != tag ]; then
    echo "publish runs for an omp-plugin-v* tag only"
    exit 1
fi
if [ "$CI_REF" != "omp-plugin-v$version" ]; then
    echo "tag $CI_REF does not match omp-plugin/package.json version $version"
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
