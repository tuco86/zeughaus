# The image the Linux release builds run in: Debian 13, whose glibc is the
# floor of the shipped binaries, with the libraries the workspace links
# (pipewire through bindgen, xcb for scrap, wayland and xkbcommon for winit).
# The same file serves amd64 under podman and arm64 under Apple `container`.
# A changed file is a new image tag; the rust image tag is resolved when the
# image is built.
FROM docker.io/library/rust:1-trixie

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        clang pkg-config libpipewire-0.3-dev libxcb1-dev libxcb-randr0-dev \
        libxcb-shm0-dev libdbus-1-dev libfontconfig-dev libsqlite3-dev \
        libssl-dev libwayland-dev libxkbcommon-dev \
    && rm -rf /var/lib/apt/lists/*
