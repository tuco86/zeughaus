# Sourced by the jobs that build the workspace; no header, so not a job.
# The caller sets `cache` and holds FORGEJO_READ_TOKEN, which this unsets.
#
# The workspace builds weida from the sibling checkout `../weida`, which is
# always a link to `$cache/weida`: one stable path, so cargo's fingerprints
# do not see the sources move between trees of different mtimes. A manual
# run fetches the weida it brings in CI_SIBLINGS into that checkout;
# otherwise it is weida's main branch, as on the workstation, fetched with
# the read-only token, which travels in the environment, not in argv.
weida="$cache/weida"
if [ -n "${CI_SIBLINGS:-}" ] && [ -d "$CI_SIBLINGS/weida/.git" ]; then
    [ -d "$weida/.git" ] || git init -q "$weida"
    git -C "$weida" fetch -q "$CI_SIBLINGS/weida" HEAD
    git -C "$weida" checkout -q --force --detach FETCH_HEAD
else
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
    unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0 GIT_TERMINAL_PROMPT
fi
unset FORGEJO_READ_TOKEN
# A repository's jobs at one place share one workspace and run one at a
# time, so nothing else writes this link meanwhile. The host workspace and a
# container's never meet here: a container's `..` is its own `/`.
link="$CI_WORKSPACE/../weida"
ln -sfn "$weida" "$link"
echo "weida $(git -C "$link" log --oneline -1)"
