#!/bin/sh
# Builds zeughaus-runner and zeughaus-ci for aarch64 Linux (sadala) in an
# arm64 container under qemu-user. Run on the x86_64 workstation, with
# weida checked out next to this repository.
#
# The binaries land in target/aarch64-linux/release/. The container's cargo
# registry is a named volume, so a second build does not download again.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)

podman run --rm --arch arm64 \
    -v "$root":/src/zeughaus -v "$root/../weida":/src/weida \
    -v zeughaus-aarch64-cargo:/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/src/zeughaus/target/aarch64-linux -w /src/zeughaus \
    docker.io/library/rust:1-trixie sh -c \
    'apt-get update -qq && apt-get install -y -qq clang pkg-config libpipewire-0.3-dev libxcb1-dev libxcb-randr0-dev libxcb-shm0-dev libdbus-1-dev libfontconfig-dev libsqlite3-dev libssl-dev >/dev/null && cargo build --release --locked -p zeughaus-runner -p zeughaus-ci'
