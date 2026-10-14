# Sourced by the platform build jobs; no header, so not a job.
# Run from the workspace root, with CI_PLATFORM set by the job's `platform`.

# Cargo's home and target directory live in the job's cache, and the build
# number is compiled into the binaries (`option_env!("ZEUGHAUS_BUILD")`), which
# is how an installed runner knows whether a published build is newer.
cargo_env() {
    cache="$CI_WORKSPACE/.ci-cache"
    export CARGO_HOME="$cache/cargo"
    export CARGO_TARGET_DIR="$cache/target"
    export CARGO_TERM_COLOR=always
    export ZEUGHAUS_BUILD="$CI_PIPELINE"
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
pack_tar() {
    tar -C "$cache/stage" -czf "$CI_OUTPUT/zeughaus-$version-$CI_PLATFORM.tar.gz" \
        "zeughaus-$version-$CI_PLATFORM"
}
