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

# cargo-nextest from its prebuilt release into the workspace's cargo home,
# where cargo finds subcommands; kept between runs by the warm workspace.
nextest_env() {
    [ -x "$CARGO_HOME/bin/cargo-nextest" ] && return
    case "$CI_PLATFORM" in
        linux-amd64) asset=linux ;;
        linux-arm64) asset=linux-arm ;;
        darwin-arm64) asset=mac ;;
        *) echo "no cargo-nextest for $CI_PLATFORM" >&2; return 1 ;;
    esac
    mkdir -p "$CARGO_HOME/bin"
    curl -LsSf "https://get.nexte.st/0.9/$asset" | tar zxf - -C "$CARGO_HOME/bin"
}

# clippy, the tests (`.config/nextest.toml`) and the doctests, which nextest
# does not run: what every platform checks. A push or manual run takes the
# unit tier; the nightly cron and a release tag take both tiers.
check_platform() {
    step cargo clippy --locked --workspace --all-targets -- -D warnings
    nextest_env
    case "$CI_EVENT" in
        cron | tag) tier=ci ;;
        *) tier=default ;;
    esac
    step cargo nextest run --locked --workspace --profile "$tier"
    step cargo test --locked --workspace --doc
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

# Copies repository files into the archive's share/, under their paths in
# the repository; `install` and every update read them from there.
stage_share() {
    for f in "$@"; do
        mkdir -p "$stage/share/$(dirname "$f")"
        cp "$CI_WORKSPACE/$f" "$stage/share/$f"
    done
}

# What a Linux host installs besides the binaries: the desktop profile's
# unit, menu entry and icon, the service runner's unit, ssh config and VM
# scripts, and the CI's unit.
stage_linux_share() {
    stage_share \
        deploy/zeughaus-runner.service \
        deploy/net.doodleshnookie.Zeughaus.desktop \
        zeughaus/assets/icon/zeughaus.svg \
        deploy/runner/zeughaus-runner.service \
        deploy/runner/ssh_config \
        deploy/ci/zeughaus-ci.service \
        vm/win11/vm.sh \
        vm/win11/autounattend.xml \
        vm/win11/setup.ps1 \
        vm/win11/firstboot.ps1 \
        vm/win11/toolchain.ps1 \
        vm/win11/prepare.ps1
}

# The archive publish.sh bundles: `zeughaus-<version>-<platform>.tar.gz`.
# Without extended attributes and, on macOS, without the `._` AppleDouble
# files bsdtar adds for them: both land as junk on any other system.
pack_tar() {
    COPYFILE_DISABLE=1 tar --no-xattrs -C "$cache/stage" \
        -czf "$CI_OUTPUT/zeughaus-$version-$CI_PLATFORM.tar.gz" \
        "zeughaus-$version-$CI_PLATFORM"
}
