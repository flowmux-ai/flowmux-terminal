#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Build a flowmux-private copy of VTE (GTK4) from source with
# packaging/vte-patches applied, and install it to a prefix.
#
# Why: upstream VTE drops a text selection the moment the foreground
# application rewrites the cells under it (vte.cc, in process_incoming()).
# Agent TUIs such as Codex and Claude Code repaint their UI continuously,
# so a drag-selection vanishes on the next frame. VTE exposes no public API
# to disable that behaviour, so flowmux links a patched VTE instead. The
# patch only removes the deselect-on-output; nothing else changes, and the
# system libvte other apps use is left untouched.
#
# Usage:
#   scripts/build-vte.sh [PREFIX]
# PREFIX defaults to ~/.local/flowmux-vte. After this, build flowmux with:
#   PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" \
#   RUSTFLAGS="-C link-arg=-Wl,-rpath,$PREFIX/lib" \
#   cargo build --release -p flowmux -p flowmux-cli
# (scripts/install-host.sh does exactly that.)
set -euo pipefail

VTE_TAG="0.78.4"
PREFIX="${1:-$HOME/.local/flowmux-vte}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PATCH="$REPO_ROOT/packaging/vte-patches/0001-keep-selection-on-output.patch"
WORK="${VTE_BUILD_DIR:-$REPO_ROOT/target/vte-build}"

echo "==> flowmux VTE: tag=$VTE_TAG prefix=$PREFIX"
mkdir -p "$WORK"
cd "$WORK"

if [ ! -d vte/.git ]; then
    git clone --depth 1 --branch "$VTE_TAG" https://gitlab.gnome.org/GNOME/vte.git vte
fi
cd vte
# Start from a clean tree so re-runs re-apply the patch deterministically.
git checkout -- .
git clean -fdq -e _build || true
echo "==> applying $PATCH"
git apply --check "$PATCH"
git apply "$PATCH"

# Same option set as the Flatpak manifest (packaging/flatpak), minus the
# Flatpak-only libdir override default. Force libdir=lib so the .pc and .so
# land in $PREFIX/lib regardless of distro lib64 conventions.
meson setup _build \
    --prefix "$PREFIX" \
    --libdir lib \
    --buildtype release \
    -Dgtk3=false \
    -Dgtk4=true \
    -Dvapi=false \
    -Dgir=false \
    -Ddocs=false \
    -D_systemd=false
meson compile -C _build
meson install -C _build

echo "==> installed patched VTE:"
PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" pkg-config --modversion vte-2.91-gtk4
echo "    $PREFIX/lib/$(ls "$PREFIX"/lib | grep -m1 'libvte-2.91-gtk4.so')"
