#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Release-build flowmux and install the binaries to the host.
#
# flowmux uses the system GTK4/libadwaita/WebKitGTK/VTE libraries. The only
# vendored C/C++ source built here is the ThorVG image-viewer backend under
# third_party/, compiled via cc with the system C++ compiler (build-essential);
# no extra compiler toolchain is required.
#
# Usage: ./install.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"


echo "==> building flowmux (release)"
cargo build --release -p flowmux -p flowmux-cli -p flowmux-md-viewer

for dir in "$HOME/.local/bin" "$HOME/.cargo/bin"; do
    if [ -d "$dir" ]; then
        install -m755 \
            target/release/flowmux \
            target/release/flowmuxctl \
            target/release/flowmux-md-viewer \
            "$dir/"
        echo "==> installed to $dir"
    fi
done

echo "==> done. Fully restart the running flowmux GUI to pick up the new binary."
