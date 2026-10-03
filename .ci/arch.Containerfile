# The image the gate runs in: Arch, like the workstation the stack runs on,
# with the libraries the workspace links (pipewire through bindgen, xcb for
# scrap, wayland and xkbcommon for winit) and a stable toolchain with the
# wasm target. A changed file is a new image tag; `stable` is resolved when
# the image is built.
FROM docker.io/library/archlinux:latest

RUN pacman -Syu --noconfirm --needed \
        base-devel git clang pkgconf rustup \
        openssl sqlite libpipewire wayland libxkbcommon libxcb fontconfig \
    && pacman -Scc --noconfirm

RUN rustup default stable \
    && rustup component add clippy rustfmt \
    && rustup target add wasm32-unknown-unknown
