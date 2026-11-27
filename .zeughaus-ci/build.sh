# Sourced by the platform build and check jobs; no header, so not a job.
# Run from the workspace root, with CI_PLATFORM set by the job's `platform`.

# Cargo's home and target directory live in the workspace, which the jobs of
# one place share and keep warm. The build number is not compiled in: it
# changes every pipeline and would rebuild every unit that read it, so the
# build jobs stamp it into the staged binaries after the build.
cargo_env() {
    cache="$CI_WORKSPACE/.ci-cache"
    export CARGO_HOME="$cache/cargo"
    export CARGO_TARGET_DIR="$cache/target"
    export CARGO_TERM_COLOR=always
    # Test and clippy builds keep file:line for panics and backtraces, while
    # target directories shrink and links get faster. The test profile
    # inherits it; release builds are unaffected; local builds keep full
    # debug info.
    export CARGO_PROFILE_DEV_DEBUG=line-tables-only
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
# unit tier; the nightly and a release tag take both tiers.
check_platform() {
    step cargo clippy --locked --workspace --all-targets -- -D warnings
    nextest_env
    case "$CI_EVENT" in
        nightly | tag) tier=ci ;;
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

# macOS keys a grant (Local Network, TCC) to a program's designated
# requirement. An ad-hoc signature's requirement is one build's cdhash, so
# every update would be a new program; a fixed identifier and certificate
# make it the same in every build. The certificate is a self-signed one from
# the job's secrets, imported into a keychain of its own for this job only.
# Without the secrets (a manual run) the binaries are signed ad-hoc. Runs in
# the job's shell, not in a subshell or `step`, so its exports and traps
# stay.
macos_signing_begin() {
    if [ -z "${ZEUGHAUS_MACOS_SIGNING_P12:-}" ]; then
        echo "no ZEUGHAUS_MACOS_SIGNING_P12 (${CI_EVENT:-}): signing ad-hoc"
        return 0
    fi
    signing_dir=$(mktemp -d "${TMPDIR:-/tmp}/zeughaus-signing.XXXXXX")
    signing_kc="$signing_dir/signing.keychain-db"
    # Drops a keychain an earlier job left in the search list when it was
    # killed before its EXIT trap ran.
    signing_saved=$(security list-keychains -d user | sed 's/^ *"//; s/"$//' | grep -v '/zeughaus-signing\.' || true)
    # The CI stops a job with SIGTERM, and sh runs the EXIT trap only on exit.
    trap macos_signing_end EXIT
    trap 'exit 143' TERM
    trap 'exit 130' INT
    kc_pw=$(openssl rand -hex 16)
    printf %s "$ZEUGHAUS_MACOS_SIGNING_P12" | base64 -D >"$signing_dir/signing.p12"
    security create-keychain -p "$kc_pw" "$signing_kc"
    security set-keychain-settings "$signing_kc"
    security unlock-keychain -p "$kc_pw" "$signing_kc"
    security import "$signing_dir/signing.p12" -k "$signing_kc" -f pkcs12 \
        -P "$ZEUGHAUS_MACOS_SIGNING_PASSWORD" -T /usr/bin/codesign >/dev/null
    # Without it codesign blocks on a keychain access prompt.
    security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$kc_pw" "$signing_kc" >/dev/null
    rm -f "$signing_dir/signing.p12"
    # cargo and build scripts never see the secrets.
    unset ZEUGHAUS_MACOS_SIGNING_P12 ZEUGHAUS_MACOS_SIGNING_PASSWORD kc_pw
    # One path per field, so $signing_saved stays unquoted.
    security list-keychains -d user -s "$signing_kc" $signing_saved
    # All identities, not only valid ones: the self-signed certificate is
    # untrusted, so `-v` would hide it.
    ZEUGHAUS_CODESIGN_IDENTITY=$(security find-identity -p codesigning "$signing_kc" | awk '$1 == "1)" {print $2; exit}')
    if [ -z "$ZEUGHAUS_CODESIGN_IDENTITY" ]; then
        echo "no code-signing identity in ZEUGHAUS_MACOS_SIGNING_P12" >&2
        return 1
    fi
    export ZEUGHAUS_CODESIGN_IDENTITY
    export ZEUGHAUS_CODESIGN_KEYCHAIN="$signing_kc"
    echo "signing with certificate $ZEUGHAUS_CODESIGN_IDENTITY"
}

# Puts the keychain search list back and deletes the job's keychain.
macos_signing_end() {
    security list-keychains -d user -s $signing_saved
    security delete-keychain "$signing_kc" 2>/dev/null || true
    rm -rf "$signing_dir"
}

# Signs one Mach-O or bundle with a fixed identifier: with the job's
# certificate after macos_signing_begin found one, else ad-hoc. Prints the
# designated requirement into the log.
macos_sign() {
    if [ -n "${ZEUGHAUS_CODESIGN_IDENTITY:-}" ]; then
        codesign --force --timestamp=none --identifier "$1" \
            --keychain "$ZEUGHAUS_CODESIGN_KEYCHAIN" --sign "$ZEUGHAUS_CODESIGN_IDENTITY" "$2" || return
    else
        codesign --force --identifier "$1" --sign - "$2" || return
    fi
    codesign --verify --strict "$2" || return
    codesign -dv -r- "$2" 2>&1
}

# Writes the pipeline number into staged binaries with the runner the job
# built (`release stamp`), signs them again on macOS with a fixed identifier
# (macos_sign; the stamp breaks the linker's ad-hoc signature, and macOS
# kills a binary whose signature does not match), and checks that each
# answers `version` with it.
stamp_build() {
    tool=$1
    shift
    "$tool" release stamp "$CI_PIPELINE" "$@"
    for bin in "$@"; do
        case "$CI_PLATFORM" in
            darwin-*) macos_sign "net.doodleshnookie.${bin##*/}" "$bin" || return ;;
        esac
        answer=$("$bin" version)
        if [ "$answer" != "$version build $CI_PIPELINE" ]; then
            echo "$bin version: $answer, expected $version build $CI_PIPELINE" >&2
            return 1
        fi
    done
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
