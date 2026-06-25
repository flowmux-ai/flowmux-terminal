#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Runtime smoke test for flowmux inside a real WSLg session.
#
# Usage:
#   scripts/check-wslg-runtime.sh [--no-install] [--keep-open]
#                                 [--bin PATH] [--timeout SECONDS]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL=true
KEEP_OPEN=false
TIMEOUT=120
FLOWMUX_BIN="${FLOWMUX_BIN:-$HOME/.local/bin/flowmux}"

usage() {
    sed -n '3,8p' "$0" >&2
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --no-install)
            INSTALL=false
            shift
            ;;
        --keep-open)
            KEEP_OPEN=true
            shift
            ;;
        --bin)
            FLOWMUX_BIN="${2:?--bin requires a path}"
            shift 2
            ;;
        --timeout)
            TIMEOUT="${2:?--timeout requires seconds}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage
            exit 2
            ;;
    esac
done

is_wsl() {
    if [ -n "${WSL_INTEROP:-}" ] || [ -n "${WSL_DISTRO_NAME:-}" ]; then
        return 0
    fi
    if [ -r /proc/sys/kernel/osrelease ] \
        && tr '[:upper:]' '[:lower:]' </proc/sys/kernel/osrelease | grep -q microsoft; then
        return 0
    fi
    return 1
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

preflight_wslg() {
    if [ "$(uname -s)" != "Linux" ]; then
        echo "error: WSLg smoke must run inside Linux/WSL, not macOS" >&2
        exit 1
    fi
    if ! is_wsl; then
        echo "error: WSLg smoke must run inside WSL/WSLg" >&2
        exit 1
    fi
    if [ -z "${WAYLAND_DISPLAY:-}" ] && [ -z "${DISPLAY:-}" ]; then
        cat >&2 <<'EOF'
error: no WSLg display was detected. Start this script from a WSL terminal with
WSLg enabled; WAYLAND_DISPLAY or DISPLAY must be set.
EOF
        exit 1
    fi
    if [ "$(ubuntu_version_id || true)" = "22.04" ]; then
        cat >&2 <<'EOF'
error: Ubuntu 22.04 on WSL should use the Flatpak path from the README.
Native WSLg smoke is supported for Ubuntu 24.04 and 26.04.
EOF
        exit 1
    fi
    if ! command -v python3 >/dev/null 2>&1; then
        echo "error: python3 is required for parsing flowmux tree output" >&2
        exit 1
    fi
}

preflight_wslg
cd "$REPO_ROOT"

if [ "$INSTALL" = true ]; then
    scripts/install-host.sh --check
    scripts/install-host.sh
elif [ ! -x "$FLOWMUX_BIN" ]; then
    echo "error: $FLOWMUX_BIN is not executable; rerun without --no-install" >&2
    exit 1
fi

smoke_state="$(mktemp -d "${TMPDIR:-/tmp}/flowmux-wslg-state.XXXXXX")"
smoke_runtime="$(mktemp -d "${TMPDIR:-/tmp}/flowmux-wslg-runtime.XXXXXX")"
smoke_log="$(mktemp "${TMPDIR:-/tmp}/flowmux-wslg-gui.XXXXXX.log")"
gui_pid=""
chmod 700 "$smoke_runtime"
export XDG_STATE_HOME="$smoke_state"
export FLOWMUX_RUNTIME_DIR="$smoke_runtime"

cleanup() {
    status=$?
    if [ "$KEEP_OPEN" != true ] && [ -n "$gui_pid" ]; then
        kill "$gui_pid" >/dev/null 2>&1 || true
        wait "$gui_pid" >/dev/null 2>&1 || true
    fi
    if [ "$KEEP_OPEN" != true ]; then
        rm -rf "$smoke_state"
        rm -rf "$smoke_runtime"
        rm -f /tmp/flowmux-wslg-ping.out \
            /tmp/flowmux-wslg-ping.err \
            /tmp/flowmux-wslg-workspace.json \
            /tmp/flowmux-wslg-tree.json \
            /tmp/flowmux-wslg-send.out \
            /tmp/flowmux-wslg-screen.txt
    fi
    if [ "$status" -ne 0 ]; then
        cat "$smoke_log" >&2 || true
    elif [ "$KEEP_OPEN" != true ]; then
        rm -f "$smoke_log"
    fi
    exit "$status"
}
trap cleanup EXIT

echo "==> launching flowmux in WSLg"
"$FLOWMUX_BIN" >"$smoke_log" 2>&1 &
gui_pid=$!

for _ in $(seq 1 "$((TIMEOUT * 4))"); do
    if "$FLOWMUX_BIN" ping >/tmp/flowmux-wslg-ping.out 2>/tmp/flowmux-wslg-ping.err; then
        break
    fi
    if ! kill -0 "$gui_pid" 2>/dev/null; then
        echo "error: flowmux GUI exited before ping" >&2
        exit 1
    fi
    sleep 0.25
done

"$FLOWMUX_BIN" workspace new \
    --name "WSLg Smoke" --root "$REPO_ROOT" --json >/tmp/flowmux-wslg-workspace.json

pane=""
for _ in $(seq 1 "$((TIMEOUT * 4))"); do
    "$FLOWMUX_BIN" tree --json >/tmp/flowmux-wslg-tree.json
    pane="$(python3 - <<'PY'
import json
try:
    data = json.load(open("/tmp/flowmux-wslg-tree.json"))
    print(data["tree"]["workspaces"][0]["panes"][0]["id"])
except Exception:
    pass
PY
)"
    if [ -n "$pane" ]; then
        break
    fi
    sleep 0.25
done

if [ -z "$pane" ]; then
    echo "error: no pane created" >&2
    cat /tmp/flowmux-wslg-tree.json >&2 || true
    exit 1
fi

marker="FLOWMUX_WSLG_SMOKE_OK"
keys=$(printf 'printf "%s\\n"\n' "$marker")
"$FLOWMUX_BIN" send-keys "$pane" "$keys" >/tmp/flowmux-wslg-send.out
sleep 0.5
"$FLOWMUX_BIN" read-screen "$pane" >/tmp/flowmux-wslg-screen.txt
grep -q "$marker" /tmp/flowmux-wslg-screen.txt

echo "==> WSLg runtime smoke passed pane=$pane"
if [ "$KEEP_OPEN" = true ]; then
    echo "==> flowmux left running pid=$gui_pid state=$smoke_state runtime=$smoke_runtime log=$smoke_log"
    echo "==> use: XDG_STATE_HOME=$smoke_state FLOWMUX_RUNTIME_DIR=$smoke_runtime $FLOWMUX_BIN ping"
fi
