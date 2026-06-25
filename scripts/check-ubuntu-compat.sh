#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Docker-backed compatibility smoke check for the Ubuntu support matrix.
#
# Ubuntu 24.04 and 26.04 have native GTK4/libadwaita/VTE/WebKitGTK 6 packages,
# so this script builds the GUI and runs it under Xvfb, then verifies CLI and
# terminal I/O against the live daemon. Ubuntu 22.04 remains the Flatpak target
# because its native GTK/libadwaita/VTE floor is too low for the GUI crate; the
# script verifies that expected package gap.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DOCKER="${DOCKER:-docker}"
NATIVE_UBUNTU_VERSIONS="${NATIVE_UBUNTU_VERSIONS:-24.04 26.04}"

need_docker() {
    if ! command -v "$DOCKER" >/dev/null 2>&1; then
        echo "error: docker is required; set DOCKER=/path/to/docker to override" >&2
        exit 1
    fi
}

check_jammy_flatpak_path() {
    echo "==> ubuntu:22.04 package floor"
    "$DOCKER" run --rm ubuntu:22.04 bash -lc '
set -euo pipefail
apt-get update >/dev/null
gtk="$(apt-cache policy libgtk-4-dev | awk "/Candidate:/ {print \$2}")"
adw="$(apt-cache policy libadwaita-1-dev | awk "/Candidate:/ {print \$2}")"
if apt-cache show libvte-2.91-gtk4-dev >/dev/null 2>&1; then
    echo "error: unexpected native GTK4 VTE package on ubuntu:22.04" >&2
    exit 1
fi
echo "gtk=$gtk libadwaita=$adw vte4=missing; use Flatpak GNOME runtime for 22.04"
'
    "$DOCKER" run --rm \
        --mount "type=bind,source=$REPO_ROOT,target=/workspace,readonly" \
        ubuntu:22.04 bash -lc '
set -euo pipefail
if /workspace/scripts/install-host.sh --check 2>/tmp/flowmux-install-host.err; then
    echo "error: install-host native preflight unexpectedly passed on ubuntu:22.04" >&2
    exit 1
fi
cat /tmp/flowmux-install-host.err
grep -q "Ubuntu 22.04" /tmp/flowmux-install-host.err
'
}

check_native_ubuntu() {
    local version="$1"
    echo "==> ubuntu:$version native GUI smoke"
    "$DOCKER" run --rm \
        --mount "type=bind,source=$REPO_ROOT,target=/workspace,readonly" \
        -e DEBIAN_FRONTEND=noninteractive \
        "ubuntu:$version" bash -lc '
set -euo pipefail
apt-get update >/dev/null
apt-get install -y --no-install-recommends \
    ca-certificates curl git build-essential pkg-config \
    meson ninja-build python3 xvfb xauth \
    libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev \
    libwebkitgtk-6.0-dev libssl-dev libssh2-1-dev \
    libdbus-1-dev libsecret-1-dev \
    liblz4-dev libpcre2-dev libfribidi-dev libicu-dev libgnutls28-dev >/dev/null
curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal >/dev/null
. "$HOME/.cargo/env"
echo "rustc $(rustc --version)"
/workspace/scripts/install-host.sh --check
CARGO_HOME=/tmp/cargo CARGO_TARGET_DIR=/tmp/flowmux-target \
    cargo build --manifest-path /workspace/Cargo.toml \
    -p flowmux -p flowmux-cli --features flowmux/vte-text --locked
export XDG_RUNTIME_DIR=/tmp/flowmux-runtime
export XDG_STATE_HOME=/tmp/flowmux-state
export XDG_DATA_HOME=/tmp/flowmux-data
export XDG_CONFIG_HOME=/tmp/flowmux-config
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_DATA_HOME" "$XDG_CONFIG_HOME"
chmod 700 "$XDG_RUNTIME_DIR"
xvfb-run -a -s "-screen 0 1280x800x24" /tmp/flowmux-target/debug/flowmux \
    >/tmp/flowmux-gui.log 2>&1 &
gui_pid=$!
cleanup() {
    status=$?
    kill "$gui_pid" >/dev/null 2>&1 || true
    wait "$gui_pid" >/dev/null 2>&1 || true
    if [ "$status" -ne 0 ]; then
        cat /tmp/flowmux-gui.log >&2 || true
    fi
    exit "$status"
}
trap cleanup EXIT
for _ in $(seq 1 120); do
    if /tmp/flowmux-target/debug/flowmux ping >/tmp/flowmux-ping.out 2>/tmp/flowmux-ping.err; then
        break
    fi
    if ! kill -0 "$gui_pid" 2>/dev/null; then
        echo "flowmux GUI exited before ping" >&2
        exit 1
    fi
    sleep 0.25
done
/tmp/flowmux-target/debug/flowmux workspace new \
    --name "Linux GUI Smoke" --root /workspace --json >/tmp/flowmux-workspace.json
pane=""
for _ in $(seq 1 120); do
    /tmp/flowmux-target/debug/flowmux tree --json >/tmp/flowmux-tree.json
    pane=$(python3 - <<PY
import json
try:
    data = json.load(open("/tmp/flowmux-tree.json"))
    print(data["tree"]["workspaces"][0]["panes"][0]["id"])
except Exception:
    pass
PY
)
    if [ -n "$pane" ]; then
        break
    fi
    sleep 0.25
done
if [ -z "$pane" ]; then
    echo "no pane created" >&2
    cat /tmp/flowmux-tree.json >&2
    exit 1
fi
keys=$(printf "printf \"FLOWMUX_LINUX_GUI_SMOKE_OK\\\\n\"\\n")
/tmp/flowmux-target/debug/flowmux send-keys "$pane" "$keys" >/tmp/flowmux-send.out
sleep 0.5
/tmp/flowmux-target/debug/flowmux read-screen "$pane" >/tmp/flowmux-screen.txt
grep -q FLOWMUX_LINUX_GUI_SMOKE_OK /tmp/flowmux-screen.txt
echo "ubuntu GUI smoke passed pane=$pane"
'
}

need_docker
check_jammy_flatpak_path
for version in $NATIVE_UBUNTU_VERSIONS; do
    check_native_ubuntu "$version"
done
echo "==> ubuntu compatibility smoke checks passed"
