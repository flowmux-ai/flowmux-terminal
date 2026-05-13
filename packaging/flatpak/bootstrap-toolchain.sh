#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

rust_version="${FLOWMUX_FLATPAK_RUST_VERSION:-1.95.0}"
zig_version="${FLOWMUX_FLATPAK_ZIG_VERSION:-0.15.2}"
toolchain_dir="${FLOWMUX_FLATPAK_TOOLCHAIN_DIR:-$PWD/.flatpak-toolchain}"

export RUSTUP_HOME="$toolchain_dir/rustup"
export CARGO_HOME="$toolchain_dir/cargo"

need_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "missing required command: $1" >&2
        exit 127
    fi
}

need_command curl
need_command tar
need_command xz

mkdir -p "$toolchain_dir"

if [ ! -x "$CARGO_HOME/bin/rustc" ] ||
    ! "$CARGO_HOME/bin/rustc" --version | grep -q "^rustc ${rust_version} "; then
    rm -rf "$RUSTUP_HOME" "$CARGO_HOME"
    mkdir -p "$RUSTUP_HOME" "$CARGO_HOME"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --profile minimal --default-toolchain "$rust_version" --no-modify-path
fi

case "$(uname -m)" in
    x86_64 | amd64)
        zig_arch="x86_64-linux"
        ;;
    aarch64 | arm64)
        zig_arch="aarch64-linux"
        ;;
    *)
        echo "unsupported Flatpak build architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

zig_dir="$toolchain_dir/zig-${zig_arch}-${zig_version}"
if [ ! -x "$zig_dir/zig" ]; then
    tmp_dir="$(mktemp -d)"
    trap 'rm -rf "$tmp_dir"' EXIT
    zig_name="zig-${zig_arch}-${zig_version}"
    curl -L --fail "https://ziglang.org/download/${zig_version}/${zig_name}.tar.xz" \
        -o "$tmp_dir/zig.tar.xz"
    tar -C "$tmp_dir" -xJf "$tmp_dir/zig.tar.xz"
    rm -rf "$zig_dir"
    mv "$tmp_dir/$zig_name" "$zig_dir"
fi

export PATH="$CARGO_HOME/bin:$zig_dir:$PATH"

rustc --version
zig version

if [ "$#" -eq 0 ]; then
    exit 0
fi

exec "$@"
