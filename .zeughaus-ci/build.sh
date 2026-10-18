# Sourced by the platform build and check jobs; no header, so not a job.
# Run from the workspace root, with CI_PLATFORM set by the job's `platform`.

# Cargo's home and target directory live in the workspace, which the jobs of
# one place share and keep warm. The build number is not set here: it
# changes every pipeline and would rebuild every unit that reads it; the
# build jobs export ZEUGHAUS_BUILD themselves.
cargo_env() {
    cache="$CI_WORKSPACE/.ci-cache"
    export CARGO_HOME="$cache/cargo"
    export CARGO_TARGET_DIR="$cache/target"
    export CARGO_TERM_COLOR=always
}

# Runs one command as a visible, timed step. Its failure returns the
# command's code, so under `set -e` the job stops there; the `if` only
# switches `set -e` off for the command's own body.
step() {
    printf '\n== %s\n' "$*"
    step_start=$(date +%s)
    if "$@"; then
        printf '== %s: %s s\n' "$*" "$(( $(date +%s) - step_start ))"
    else
        step_code=$?
        printf '== %s failed after %s s\n' "$*" "$(( $(date +%s) - step_start ))"
        return "$step_code"
    fi
}

# clippy and the tests: what every platform checks.
check_platform() {
    step cargo clippy --locked --workspace --all-targets -- -D warnings
    step cargo test --locked --workspace
}

# The workspace version, which names the archive and its top directory.
version() {
    sed -n 's/^version = "\(.*\)"/\1/p' "$CI_WORKSPACE/Cargo.toml" | head -1
}

# Sets `version` and `stage`: an empty directory that becomes the archive's
# top directory, with the licences already in it.
stage_dir() {
    version=$(version)
    stage="$cache/stage/zeughaus-$version-$CI_PLATFORM"
    rm -rf "$stage"
    mkdir -p "$stage"
    cp LICENSE-MIT LICENSE-APACHE "$stage/"
}

# The archive publish.sh bundles: `zeughaus-<version>-<platform>.tar.gz`.
# Without extended attributes and, on macOS, without the `._` AppleDouble
# files bsdtar adds for them: both land as junk on any other system.
pack_tar() {
    COPYFILE_DISABLE=1 tar --no-xattrs -C "$cache/stage" \
        -czf "$CI_OUTPUT/zeughaus-$version-$CI_PLATFORM.tar.gz" \
        "zeughaus-$version-$CI_PLATFORM"
}
