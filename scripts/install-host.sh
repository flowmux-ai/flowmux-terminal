#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Release-build flowmux against the flowmux-private patched VTE and install
# the binaries to the host. The patched VTE (see scripts/build-vte.sh) keeps
# text selections alive while a TUI repaints; linking it is what makes
# drag-selection work in Codex / Claude Code panes.
#
# Usage:
#   scripts/install-host.sh [VTE_PREFIX]
#   scripts/install-host.sh --check
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHECK_ONLY=false
if [ "${1:-}" = "--check" ]; then
    CHECK_ONLY=true
    shift
fi
PREFIX="${1:-$HOME/.local/flowmux-vte}"
cd "$REPO_ROOT"

truthy() {
    case "${1:-}" in
        1|true|TRUE|yes|YES|on|ON) return 0 ;;
        *) return 1 ;;
    esac
}

ubuntu_version_id() {
    if [ ! -r /etc/os-release ]; then
        return 1
    fi
    # shellcheck disable=SC1091
    . /etc/os-release
    if [ "${ID:-}" = "ubuntu" ]; then
        printf '%s\n' "${VERSION_ID:-}"
        return 0
    fi
    return 1
}

preflight_native_host() {
    if [ "$(uname -s)" != "Linux" ]; then
        cat >&2 <<'EOF'
error: scripts/install-host.sh is for Linux/WSLg native installs.
On macOS, build and run the FlowMux.app bundle instead of this Linux installer.
EOF
        exit 1
    fi

    local ubuntu_version
    ubuntu_version="$(ubuntu_version_id || true)"
    if [ "$ubuntu_version" = "22.04" ] \
        && ! truthy "${FLOWMUX_ALLOW_NATIVE_UBUNTU_22_04:-}"; then
        cat >&2 <<'EOF'
error: Ubuntu 22.04's native GTK/libadwaita/WebKit/VTE floor is too old for flowmux.
Use the README's Ubuntu 22.04 Flatpak path instead. To bypass this guard for
local experiments, set FLOWMUX_ALLOW_NATIVE_UBUNTU_22_04=1.
EOF
        exit 1
    fi

    local missing_commands=()
    for command in cargo git meson ninja pkg-config; do
        if ! command -v "$command" >/dev/null 2>&1; then
            missing_commands+=("$command")
        fi
    done

    local missing_modules=()
    if command -v pkg-config >/dev/null 2>&1; then
        for module in \
            gtk4 libadwaita-1 webkitgtk-6.0 \
            openssl libssh2 dbus-1 libsecret-1 \
            fribidi gnutls icu-i18n liblz4 libpcre2-8
        do
            if ! pkg-config --exists "$module"; then
                missing_modules+=("$module")
            fi
        done
    fi

    if [ "${#missing_commands[@]}" -gt 0 ] || [ "${#missing_modules[@]}" -gt 0 ]; then
        if [ "${#missing_commands[@]}" -gt 0 ]; then
            echo "error: missing commands: ${missing_commands[*]}" >&2
        fi
        if [ "${#missing_modules[@]}" -gt 0 ]; then
            echo "error: missing pkg-config modules: ${missing_modules[*]}" >&2
        fi
        cat >&2 <<'EOF'
Install the Ubuntu native prerequisites:
  sudo apt install build-essential pkg-config git meson ninja-build \
      libgtk-4-dev libadwaita-1-dev libwebkitgtk-6.0-dev \
      libssl-dev libssh2-1-dev libdbus-1-dev libsecret-1-dev \
      liblz4-dev libpcre2-dev libfribidi-dev libicu-dev libgnutls28-dev

Install Rust with rustup if `cargo` is missing:
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
EOF
        exit 1
    fi
}

preflight_native_host
if [ "$CHECK_ONLY" = true ]; then
    echo "==> host native preflight checks passed"
    exit 0
fi

if [ ! -f "$PREFIX/lib/pkgconfig/vte-2.91-gtk4.pc" ]; then
    echo "==> patched VTE not found at $PREFIX; building it"
    scripts/build-vte.sh "$PREFIX"
fi

echo "==> building flowmux against patched VTE ($PREFIX)"
# RUNPATH so the installed binary loads the patched libvte at runtime instead
# of the system one. PKG_CONFIG_PATH so the build links the patched headers/.so.
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
export RUSTFLAGS="-C link-arg=-Wl,-rpath,$PREFIX/lib ${RUSTFLAGS:-}"
# The patched VTE built above is 0.78.4, so it supports the text
# extraction API behind `vte-text` — enable it here to ship
# `flowmux read-screen`. The default (system-VTE) build leaves it off to
# keep the v0_70 compatibility floor.
cargo build --release -p flowmux -p flowmux-cli --features flowmux/vte-text

for dir in "$HOME/.local/bin" "$HOME/.cargo/bin"; do
    if [ -d "$dir" ]; then
        install -m755 target/release/flowmux target/release/flowmuxctl "$dir/"
        echo "==> installed to $dir"
    fi
done

echo "==> verifying the installed GUI links the patched VTE:"
ldd "$HOME/.local/bin/flowmux" 2>/dev/null | grep -i 'libvte' || true
echo "==> done. Fully restart the running flowmux GUI to pick up the new binary."
