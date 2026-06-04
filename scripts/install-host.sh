#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Release-build flowmux against the flowmux-private patched VTE and install
# the binaries to the host. The patched VTE (see scripts/build-vte.sh) keeps
# text selections alive while a TUI repaints; linking it is what makes
# drag-selection work in Codex / Claude Code panes.
#
# Usage: scripts/install-host.sh [VTE_PREFIX]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${1:-$HOME/.local/flowmux-vte}"
cd "$REPO_ROOT"

if [ ! -f "$PREFIX/lib/pkgconfig/vte-2.91-gtk4.pc" ]; then
    echo "==> patched VTE not found at $PREFIX; building it"
    scripts/build-vte.sh "$PREFIX"
fi

echo "==> building flowmux against patched VTE ($PREFIX)"
# RUNPATH so the installed binary loads the patched libvte at runtime instead
# of the system one. PKG_CONFIG_PATH so the build links the patched headers/.so.
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
export RUSTFLAGS="-C link-arg=-Wl,-rpath,$PREFIX/lib ${RUSTFLAGS:-}"
cargo build --release -p flowmux -p flowmux-cli

for dir in "$HOME/.local/bin" "$HOME/.cargo/bin"; do
    if [ -d "$dir" ]; then
        install -m755 target/release/flowmux target/release/flowmuxctl "$dir/"
        echo "==> installed to $dir"
    fi
done

echo "==> verifying the installed GUI links the patched VTE:"
ldd "$HOME/.local/bin/flowmux" 2>/dev/null | grep -i 'libvte' || true
echo "==> done. Fully restart the running flowmux GUI to pick up the new binary."
