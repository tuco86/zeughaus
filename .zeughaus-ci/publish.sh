#!/bin/bash
# /// ci
# needs = ["gate", "linux-amd64", "linux-arm64", "windows-amd64", "darwin-arm64", "darwin-amd64"]
# machine = "sadala"
# cache = [".ci-cache"]
# secrets = ["ZEUGHAUS_UPDATE_KEY", "ZEUGHAUS_GITHUB_TOKEN"]
# when_busy = "run"
# timeout_minutes = 60
# [env]
# GITHUB_REPO = "tuco86/zeughaus"
# UPDATES_DIR = "/var/www/zeughaus-updates"
# ///
# Collects the five platform archives into one signed release bundle and
# publishes it: to the update channel the installed runners poll (Caddy
# serves $UPDATES_DIR under /updates/), and for cron and tag events also as a
# GitHub release. Runs no cargo: the bundling tool is the zeughaus-runner of
# the linux-arm64 archive, which is why this job needs sadala (aarch64).
set -euo pipefail

. "$CI_WORKSPACE/.zeughaus-ci/build.sh"
version=$(version)

case "$CI_EVENT" in
push) channel=dev ;;
cron) channel=nightly ;;
tag) channel=release ;;
*)
    echo "unknown CI_EVENT '$CI_EVENT'"
    exit 1
    ;;
esac
if [ "$CI_EVENT" = tag ] && [ "$CI_REF" != "v$version" ]; then
    echo "tag $CI_REF does not match workspace version $version"
    exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

platforms="linux-amd64 linux-arm64 windows-amd64 darwin-arm64 darwin-amd64"
mkdir "$work/files"
for p in $platforms; do
    dir="$CI_INPUTS/$p"
    if [ ! -d "$dir" ]; then
        echo "no output of job $p in $CI_INPUTS"
        exit 1
    fi
    cp -L "$dir"/* "$work/files/"
done

if [ "$(uname -m)" != aarch64 ]; then
    echo "publish needs an aarch64 host to run the linux-arm64 zeughaus-runner, this is $(uname -m)"
    exit 1
fi
mkdir "$work/tool"
tar -C "$work/tool" -xzf "$work/files/zeughaus-$version-linux-arm64.tar.gz" \
    --strip-components=1 "zeughaus-$version-linux-arm64/zeughaus-runner"

"$work/tool/zeughaus-runner" release bundle --channel "$channel" --build "$CI_PIPELINE" \
    --sha "$CI_SHA" --version "$version" --out "$work/bundle" "$work"/files/*
unset ZEUGHAUS_UPDATE_KEY
"$work/tool/zeughaus-runner" release verify "$work/bundle"
# Caddy runs as another user.
chmod -R a+rX "$work/bundle"

# The update channel: one directory per build and `latest` naming the newest.
# A rerun of an older pipeline never moves `latest` backwards.
dest="$UPDATES_DIR/$channel"
mkdir -p "$dest"
if [ -f "$dest/latest" ] && [ "$(cat "$dest/latest")" -ge "$CI_PIPELINE" ]; then
    echo "newer build already published"
else
    rm -rf "$dest/$CI_PIPELINE.tmp" "$dest/$CI_PIPELINE"
    cp -R "$work/bundle" "$dest/$CI_PIPELINE.tmp"
    mv "$dest/$CI_PIPELINE.tmp" "$dest/$CI_PIPELINE"
    printf '%s\n' "$CI_PIPELINE" >"$dest/latest.tmp"
    chmod a+r "$dest/latest.tmp"
    mv -f "$dest/latest.tmp" "$dest/latest"
    echo "published build $CI_PIPELINE to $dest"
    # Keep the two newest builds.
    for d in "$dest"/*/; do
        n=$(basename "$d")
        case "$n" in
        '' | *[!0-9]*) continue ;;
        esac
        echo "$n"
    done | sort -rn | tail -n +3 | while read -r n; do
        rm -rf "${dest:?}/$n"
    done
fi

[ "$CI_EVENT" = push ] && exit 0

# GitHub. The token goes to curl through a header file, never through argv.
: "${ZEUGHAUS_GITHUB_TOKEN:?secret ZEUGHAUS_GITHUB_TOKEN is not available to this event}"
hdr="$work/github.hdr"
(
    umask 077
    printf 'Authorization: Bearer %s\n' "$ZEUGHAUS_GITHUB_TOKEN" >"$hdr"
)
unset ZEUGHAUS_GITHUB_TOKEN

api="https://api.github.com/repos/$GITHUB_REPO"
gh() {
    curl -fsS -H "@$hdr" -H 'Accept: application/vnd.github+json' \
        -H 'X-GitHub-Api-Version: 2022-11-28' "$@"
}
# The HTTP status of a request whose failure is an answer, not an error.
gh_status() {
    curl -sS -o "$work/response" -w '%{http_code}' -H "@$hdr" \
        -H 'Accept: application/vnd.github+json' \
        -H 'X-GitHub-Api-Version: 2022-11-28' "$@"
}
# A release and its tag; the tag outlives the release on GitHub.
delete_release() {
    gh -X DELETE "$api/releases/$1" >/dev/null
    delete_tag "$2"
}
delete_tag() {
    code=$(gh_status -X DELETE "$api/git/refs/tags/$1")
    case "$code" in
    204 | 404 | 422) ;;
    *)
        echo "deleting tag $1: HTTP $code"
        exit 1
        ;;
    esac
}

if [ "$CI_EVENT" = cron ]; then
    tag="nightly-$(date -u +%Y-%m-%d)"
    name="$tag"
    prerelease=true
    body="Nightly build $CI_PIPELINE of $CI_SHA."
else
    tag="$CI_REF"
    name="$CI_REF"
    case "$version" in
    *-*) prerelease=true ;;
    *) prerelease=false ;;
    esac
    body=$(awk -v head="## [$version]" '
        substr($0, 1, length(head)) == head { on = 1; next }
        on && substr($0, 1, 4) == "## [" { exit }
        on' CHANGELOG.md)
fi

# The same tag again (a rerun, a second nightly of the day) replaces the
# release. A nightly's tag goes with it, so the new one points at this
# commit; a version tag is the one pushed by hand and stays.
code=$(gh_status "$api/releases/tags/$tag")
case "$code" in
200)
    old_id=$(jq -r .id "$work/response")
    if [ "$CI_EVENT" = cron ]; then
        delete_release "$old_id" "$tag"
    else
        gh -X DELETE "$api/releases/$old_id" >/dev/null
    fi
    ;;
404) ;;
*)
    echo "looking up release $tag: HTTP $code"
    exit 1
    ;;
esac

jq -n --arg tag "$tag" --arg sha "$CI_SHA" --arg name "$name" --arg body "$body" \
    --argjson prerelease "$prerelease" \
    '{tag_name: $tag, target_commitish: $sha, name: $name, prerelease: $prerelease, body: $body}' \
    >"$work/release.json"
id=$(gh -X POST --data-binary "@$work/release.json" "$api/releases" | jq -r .id)
echo "created release $tag ($id)"

for f in "$work"/bundle/*; do
    gh -X POST -H 'Content-Type: application/octet-stream' --data-binary "@$f" \
        "https://uploads.github.com/repos/$GITHUB_REPO/releases/$id/assets?name=$(basename "$f")" \
        >/dev/null
    echo "uploaded $(basename "$f")"
done

# Keep the seven newest nightlies.
gh "$api/releases?per_page=100" |
    jq -r '[.[] | select(.tag_name | startswith("nightly-"))] | sort_by(.tag_name) | reverse | .[7:][] | "\(.id) \(.tag_name)"' |
    while read -r old_id old_tag; do
        delete_release "$old_id" "$old_tag"
        echo "pruned $old_tag"
    done
