# The image the gate and the Linux release builds run in: Debian 13, whose
# glibc is the floor of the shipped binaries, with the libraries the workspace
# links (pipewire through bindgen, xcb for scrap, wayland and xkbcommon for
# winit) and the gate's toolchain parts (clippy, rustfmt, the wasm target).
# The same file serves amd64 under podman and arm64 under Apple `container`.
# A changed file is a new image tag. The rust tag equals the channel of
# rust-toolchain.toml; change both together.
FROM docker.io/library/rust:1.99.1-trixie

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        clang pkg-config libpipewire-0.3-dev libxcb1-dev libxcb-randr0-dev \
        libxcb-shm0-dev libdbus-1-dev libfontconfig-dev libsqlite3-dev \
        libssl-dev libwayland-dev libxkbcommon-dev \
    && rm -rf /var/lib/apt/lists/*

RUN rustup component add clippy rustfmt \
    && rustup target add wasm32-unknown-unknown
