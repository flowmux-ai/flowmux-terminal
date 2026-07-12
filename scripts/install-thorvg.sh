#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Build and install ThorVG with every image loader enabled and the C API
# exposed, which is what flowmux's image viewer links against.
#
# flowmux does NOT vendor ThorVG. It links the system ThorVG through the
# `thorvg-sys` crate in pkg-config mode, so ThorVG must be installed first.
# Ubuntu (through 24.04) does not package ThorVG, so this script builds it
# from source with meson/ninja. ThorVG source is cloned into a temporary
# directory outside the repo and removed afterwards.
#
# Usage:
#   scripts/install-thorvg.sh              # build + install to /usr/local (sudo)
#   THORVG_VERSION=v1.0.6 scripts/install-thorvg.sh
#   PREFIX=$HOME/.local scripts/install-thorvg.sh   # no sudo (set PKG_CONFIG_PATH)

set -euo pipefail

# Match the ThorVG version the `thorvg-sys` crate generates its bindings from
# (0.2.1+thorvg-1.0.6) to avoid C API version skew.
THORVG_VERSION="${THORVG_VERSION:-v1.0.6}"
PREFIX="${PREFIX:-/usr/local}"

need() { command -v "$1" >/dev/null 2>&1 || { echo "error: '$1' not found; install it first" >&2; exit 1; }; }
need git
need meson
need ninja

if ! command -v c++ >/dev/null 2>&1 && ! command -v g++ >/dev/null 2>&1; then
    echo "error: no C++ compiler found (install build-essential)" >&2
    exit 1
fi

# sudo only when installing into a system prefix we cannot write to.
SUDO=""
if [ ! -w "$PREFIX" ]; then
    SUDO="sudo"
fi

src_dir="$(mktemp -d)"
trap 'rm -rf "$src_dir"' EXIT

echo "==> cloning ThorVG $THORVG_VERSION"
git clone --depth 1 --branch "$THORVG_VERSION" https://github.com/thorvg/thorvg.git "$src_dir"

echo "==> configuring (all loaders, C API, CPU/software engine)"
meson setup "$src_dir/build" "$src_dir" \
    --prefix="$PREFIX" \
    --buildtype=release \
    -Dloaders=all \
    -Dsavers=all \
    -Dbindings=capi \
    -Dengines=cpu \
    -Dtools="" \
    -Dtests=false

echo "==> building"
ninja -C "$src_dir/build"

echo "==> installing to $PREFIX"
$SUDO ninja -C "$src_dir/build" install

case "$PREFIX" in
    /usr|/usr/local)
        need ldconfig
        if [ "$(id -u)" -eq 0 ]; then
            ldconfig
        else
            need sudo
            sudo ldconfig
        fi
        ;;
esac

echo "==> verifying"
if PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig:$PREFIX/lib/x86_64-linux-gnu/pkgconfig:${PKG_CONFIG_PATH:-}" \
     pkg-config --exists thorvg-1; then
    ver="$(PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig:$PREFIX/lib/x86_64-linux-gnu/pkgconfig:${PKG_CONFIG_PATH:-}" pkg-config --modversion thorvg-1)"
    echo "==> done. thorvg-1 $ver installed under $PREFIX"
else
    echo "==> installed under $PREFIX, but pkg-config could not find thorvg-1." >&2
    echo "    Add its pkgconfig dir to PKG_CONFIG_PATH before building flowmux." >&2
fi
