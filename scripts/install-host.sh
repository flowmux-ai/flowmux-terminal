#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Release-build flowmux and install the binaries to the host.
#
# flowmux no longer links VTE: the terminal is rendered by a pure-Rust
# backend (alacritty_terminal engine + a custom GTK4 renderer), so a
# plain `cargo build --release` is all that is needed — no patched VTE,
# no meson/ninja, no special PKG_CONFIG_PATH or RUNPATH.
#
# Usage: scripts/install-host.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

echo "==> building flowmux (release)"
cargo build --release -p flowmux -p flowmux-cli

for dir in "$HOME/.local/bin" "$HOME/.cargo/bin"; do
    if [ -d "$dir" ]; then
        install -m755 target/release/flowmux target/release/flowmuxctl "$dir/"
        echo "==> installed to $dir"
    fi
done

echo "==> done. Fully restart the running flowmux GUI to pick up the new binary."
