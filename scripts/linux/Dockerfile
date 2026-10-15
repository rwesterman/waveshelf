# Build environment for scripts/package-linux.sh. Ubuntu 22.04's glibc (2.35)
# sets the oldest distribution the resulting binary runs on.
FROM ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates clang curl libclang-dev nasm pkg-config xz-utils \
        zlib1g-dev libasound2-dev \
        libxkbcommon-dev libwayland-dev libx11-dev libxcursor-dev libxrandr-dev libxi-dev libgl1-mesa-dev \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable \
    && chmod -R a+w $RUSTUP_HOME $CARGO_HOME
