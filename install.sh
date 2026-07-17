#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Release-build flowmux and install the binaries, the .desktop entry and the
# app icons to the host, so flowmux shows up in the app launcher / dock.
#
# flowmux uses the system GTK4/libadwaita/WebKitGTK/VTE libraries. The image
# viewer discovers ThorVG at runtime with libloading, so the core app builds
# without it and enables image rendering when the shared library is present.
#
# Usage: ./install.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

# Launcher-started GUIs (and the in-app updater they spawn) inherit a session
# PATH without ~/.cargo/bin, so a bare `cargo` below can resolve to an outdated
# distro toolchain. Prefer the rustup toolchain unless the caller's PATH
# already provides it.
if [ -d "$HOME/.cargo/bin" ]; then
    case ":$PATH:" in
        *":$HOME/.cargo/bin:"*) ;;
        *) PATH="$HOME/.cargo/bin:$PATH" ;;
    esac
fi

# Fail with a clear message when cargo is missing or predates the workspace
# MSRV — an old toolchain otherwise dies on the lock file or mid-build with
# errors that do not name the real problem.
MSRV="$(sed -n 's/^rust-version = "\(.*\)"$/\1/p' Cargo.toml)"
if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found on PATH. Install Rust via https://rustup.rs" >&2
    exit 1
fi
CARGO_VERSION="$(cargo --version | awk '{print $2}')"
if [ "$(printf '%s\n' "$MSRV" "$CARGO_VERSION" | sort -V | head -n1)" != "$MSRV" ]; then
    echo "error: cargo $CARGO_VERSION ($(command -v cargo)) is older than the" >&2
    echo "       required $MSRV (rust-version in Cargo.toml). This usually means" >&2
    echo "       a distro toolchain (e.g. apt cargo) shadowed rustup's." >&2
    echo "       Install or update Rust via rustup: https://rustup.rs" >&2
    exit 1
fi

# ThorVG is an optional runtime dependency: flowmux builds and installs without
# it. If it is missing, everything works except the inline image viewer, which
# shows a "ThorVG is not installed" message until the library is present.
if ! ldconfig -p 2>/dev/null | grep -q 'libthorvg-1\.so' \
    && ! pkg-config --exists thorvg-1 2>/dev/null; then
    echo "note: ThorVG not detected — the image viewer will be disabled until" >&2
    echo "      you install it:" >&2
    echo "        sudo apt install libthorvg-dev   # where packaged (e.g. Debian)" >&2
    echo "        scripts/install-thorvg.sh        # build from source (Ubuntu)" >&2
fi

echo "==> building flowmux (release)"
cargo build --release -p flowmux -p flowmux-cli -p flowmux-md-viewer

# The first directory installed to is the one the .desktop entry points at, so
# launching from the dock runs the same binary a shell on PATH would.
PRIMARY_BIN_DIR=""
for dir in "$HOME/.local/bin" "$HOME/.cargo/bin"; do
    if [ -d "$dir" ]; then
        install -m755 \
            target/release/flowmux \
            target/release/flowmuxctl \
            target/release/flowmux-md-viewer \
            "$dir/"
        echo "==> installed to $dir"
        [ -n "$PRIMARY_BIN_DIR" ] || PRIMARY_BIN_DIR="$dir"
    fi
done

if [ -z "$PRIMARY_BIN_DIR" ]; then
    echo "error: neither ~/.local/bin nor ~/.cargo/bin exists; nothing installed." >&2
    echo "       create one of them and re-run: mkdir -p ~/.local/bin" >&2
    exit 1
fi

# Desktop entry + icons. A .desktop file alone is not enough — the launcher
# resolves `Icon=com.flowmux.App` through the hicolor theme, so the PNG/SVG have
# to land under the matching per-size apps/ directories with that basename.
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
APP_ID="com.flowmux.App"

# `Exec=flowmux` relies on the session PATH, which does not always include
# ~/.local/bin (and never includes ~/.cargo/bin) for launcher-started apps.
# Rewrite it to the absolute path so the dock entry works regardless.
install -d "$DATA_DIR/applications"
sed "s|^Exec=flowmux$|Exec=$PRIMARY_BIN_DIR/flowmux|" \
    "resources/desktop/$APP_ID.desktop" \
    > "$DATA_DIR/applications/$APP_ID.desktop"
chmod 644 "$DATA_DIR/applications/$APP_ID.desktop"
echo "==> installed desktop entry to $DATA_DIR/applications/$APP_ID.desktop"

install -Dm644 resources/icons/flowmux.svg \
    "$DATA_DIR/icons/hicolor/scalable/apps/$APP_ID.svg"
for size in 16 24 32 48 64 96 128 256 512; do
    install -Dm644 "resources/icons/flowmux-${size}.png" \
        "$DATA_DIR/icons/hicolor/${size}x${size}/apps/$APP_ID.png"
done
echo "==> installed icons to $DATA_DIR/icons/hicolor"

# Refresh the launcher caches. Both tools are optional: GNOME rescans on login
# anyway, so a missing binary only delays the icon showing up.
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DATA_DIR/applications" 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -qtf "$DATA_DIR/icons/hicolor" 2>/dev/null || true
fi

echo "==> done. Fully restart the running flowmux GUI to pick up the new binary."
