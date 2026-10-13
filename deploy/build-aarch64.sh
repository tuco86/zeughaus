#!/bin/sh
# Builds zeughaus-runner and zeughaus-ci for aarch64 Linux (sadala) in an
# arm64 container. Meant for an arm64 host (atik: colima's docker, or podman),
# where the container runs natively; an x86_64 host would emulate it through
# qemu-user, which works but is several times slower. weida must be checked
# out next to this repository.
#
# The build image (Debian's Rust plus the -dev packages of the native
# libraries) is built once and cached by the engine; the cargo registry is a
# named volume. The binaries land in target/aarch64-linux/release/.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
image=localhost/zeughaus-aarch64-build

if command -v podman >/dev/null 2>&1; then
    engine=podman
elif command -v docker >/dev/null 2>&1; then
    engine=docker
else
    echo "build-aarch64: neither podman nor docker found" >&2
    exit 1
fi

$engine build -q --platform linux/arm64 -t "$image" - >/dev/null <<'EOF'
FROM docker.io/library/rust:1-trixie
RUN apt-get update -qq && apt-get install -y -qq clang pkg-config \
    libpipewire-0.3-dev libxcb1-dev libxcb-randr0-dev libxcb-shm0-dev \
    libdbus-1-dev libfontconfig-dev libsqlite3-dev libssl-dev >/dev/null \
    && rm -rf /var/lib/apt/lists/*
EOF

$engine run --rm --platform linux/arm64 \
    -v "$root":/src/zeughaus -v "$root/../weida":/src/weida \
    -v zeughaus-aarch64-cargo:/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/src/zeughaus/target/aarch64-linux -w /src/zeughaus \
    "$image" cargo build --release --locked -p zeughaus-runner -p zeughaus-ci
