# Sourced by the jobs that build the workspace; no header, so not a job.
# The caller sets `cache` and holds FORGEJO_READ_TOKEN, which this unsets.
#
# The workspace builds weida from the sibling checkout `../weida`, at its
# main branch, as on the workstation. It is kept in the cache and fetched
# with the read-only token, which travels in the environment, not in argv.
weida="$cache/weida"
url=https://git.doodleshnookie.net/tuco86/weida.git
export GIT_CONFIG_COUNT=1
export GIT_CONFIG_KEY_0=http.extraHeader
# `tr` rather than `base64 -w0`: macOS' base64 has no -w.
GIT_CONFIG_VALUE_0="Authorization: Basic $(printf 'zeughaus-ci:%s' "$FORGEJO_READ_TOKEN" | base64 | tr -d '\n')"
export GIT_CONFIG_VALUE_0
# A job's terminal is a tty: without this, a rejected token makes git ask
# for a username and the job hangs until its timeout instead of failing.
export GIT_TERMINAL_PROMPT=0
if [ -d "$weida/.git" ]; then
    git -C "$weida" fetch -q "$url" main
    git -C "$weida" checkout -q --force --detach FETCH_HEAD
else
    git clone -q --branch main "$url" "$weida"
fi
unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0 GIT_TERMINAL_PROMPT FORGEJO_READ_TOKEN
# Host jobs of one repo on one runner share the parent directory, so two of
# them (darwin-arm64 and darwin-amd64 on atik) race for this link; BSD `ln
# -sf` then fails with EEXIST. The loser builds against the winner's checkout,
# fetched from the same branch moments apart.
link="$CI_WORKSPACE/../weida"
if ! ln -sfn "$weida" "$link" 2>/dev/null; then
    [ -d "$link/.git" ] || ln -sfn "$weida" "$link"
fi
echo "weida $(git -C "$link" log --oneline -1)"
