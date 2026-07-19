#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Build and install a local macOS FlowMux.app bundle.
#
# Usage:
#   scripts/install-macos.sh [--check] [--fast|--debug|--release] [--launch]
#                            [--app-dir DIR] [--bin-dir DIR]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_DIR="${FLOWMUX_MACOS_APP_DIR:-$HOME/Applications}"
BIN_DIR="${FLOWMUX_MACOS_BIN_DIR:-$HOME/.local/bin}"
PROFILE="${FLOWMUX_MACOS_PROFILE:-fast}"
CHECK_ONLY=false
LAUNCH=false

usage() {
    sed -n '3,9p' "$0" >&2
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --check)
            CHECK_ONLY=true
            shift
            ;;
        --fast)
            PROFILE=fast
            shift
            ;;
        --debug)
            PROFILE=debug
            shift
            ;;
        --release)
            PROFILE=release
            shift
            ;;
        --launch)
            LAUNCH=true
            shift
            ;;
        --app-dir)
            APP_DIR="${2:?--app-dir requires a directory}"
            shift 2
            ;;
        --bin-dir)
            BIN_DIR="${2:?--bin-dir requires a directory}"
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

preflight_macos() {
    if [ "$(uname -s)" != "Darwin" ]; then
        cat >&2 <<'EOF'
error: scripts/install-macos.sh is for macOS.
On Linux or WSLg, use ./install.sh instead.
EOF
        exit 1
    fi

    local missing_commands=()
    for command in cargo pkg-config codesign iconutil install launchctl open plutil xattr xcrun; do
        if ! command -v "$command" >/dev/null 2>&1; then
            missing_commands+=("$command")
        fi
    done

    local missing_frameworks=()
    if command -v xcrun >/dev/null 2>&1; then
        local sdk_path
        sdk_path="$(xcrun --sdk macosx --show-sdk-path 2>/dev/null || true)"
        if [ -z "$sdk_path" ] || [ ! -d "$sdk_path/System/Library/Frameworks/WebKit.framework" ]; then
            missing_frameworks+=("WebKit.framework (macOS SDK)")
        fi
    fi

    local missing_modules=()
    if command -v pkg-config >/dev/null 2>&1; then
        for module in gtk4 libadwaita-1; do
            if ! pkg-config --exists "$module"; then
                missing_modules+=("$module")
            fi
        done
    fi

    if [ "${#missing_commands[@]}" -gt 0 ] || [ "${#missing_frameworks[@]}" -gt 0 ] || [ "${#missing_modules[@]}" -gt 0 ]; then
        if [ "${#missing_commands[@]}" -gt 0 ]; then
            echo "error: missing commands: ${missing_commands[*]}" >&2
        fi
        if [ "${#missing_frameworks[@]}" -gt 0 ]; then
            echo "error: missing Apple frameworks: ${missing_frameworks[*]}" >&2
        fi
        if [ "${#missing_modules[@]}" -gt 0 ]; then
            echo "error: missing pkg-config modules: ${missing_modules[*]}" >&2
        fi
        cat >&2 <<'EOF'
Install the macOS native prerequisites:
  brew install pkg-config gtk4 libadwaita

FlowMux uses Homebrew GTK/libadwaita and Apple WebKit.framework for the
browser pane; do not install WebKitGTK.

Install Rust with rustup if `cargo` is missing:
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
EOF
        exit 1
    fi
}

configure_macos_path() {
    local dir
    for dir in \
        /usr/local/sbin \
        /usr/local/bin \
        /opt/homebrew/sbin \
        /opt/homebrew/bin \
        "$HOME/.cargo/bin"; do
        if [ -d "$dir" ] && [[ ":${PATH:-}:" != *":$dir:"* ]]; then
            PATH="$dir${PATH:+:$PATH}"
        fi
    done
    export PATH
}

workspace_version() {
    awk -F '"' '/^version = / { print $2; exit }' "$REPO_ROOT/Cargo.toml"
}

running_flowmux_pid() {
    local executable="$1"
    local candidate="${FLOWMUX_UPDATE_HOST_PID:-$PPID}"
    local pid command

    while read -r pid command; do
        if [ "$command" = "$executable" ]; then
            printf '%s\n' "$pid"
            return 0
        fi
    done < <(ps -ww -p "$candidate" -o pid=,comm= 2>/dev/null)

    while read -r pid command; do
        if [ "$command" = "$executable" ]; then
            printf '%s\n' "$pid"
            return 0
        fi
    done < <(ps -ww -axo pid=,comm=)
    return 1
}

submit_deferred_app_swap() {
    local running_pid="$1"
    local staged_bundle="$2"
    local destination_bundle="$3"
    local backup_bundle="$4"
    local label="com.flowmux.update.$running_pid"
    local update_dir="$HOME/.cache/flowmux"
    local update_log="$update_dir/update.log"
    local swap_helper="$update_dir/deferred-macos-app-swap.sh"

    mkdir -p "$update_dir"
    install -m755 "$REPO_ROOT/scripts/deferred-macos-app-swap.sh" "$swap_helper"
    launchctl remove "$label" >/dev/null 2>&1 || true
    launchctl submit \
        -l "$label" \
        -o "$update_log" \
        -e "$update_log" \
        -- /bin/sh "$swap_helper" \
        "$running_pid" "$staged_bundle" "$destination_bundle" "$backup_bundle"
}

create_icon() {
    local resources="$1"
    local iconset="$REPO_ROOT/target/macos-bundle/FlowMux.iconset"

    rm -rf "$iconset"
    mkdir -p "$iconset"
    cp "$REPO_ROOT/resources/icons/flowmux-16.png" "$iconset/icon_16x16.png"
    cp "$REPO_ROOT/resources/icons/flowmux-32.png" "$iconset/icon_16x16@2x.png"
    cp "$REPO_ROOT/resources/icons/flowmux-32.png" "$iconset/icon_32x32.png"
    cp "$REPO_ROOT/resources/icons/flowmux-64.png" "$iconset/icon_32x32@2x.png"
    cp "$REPO_ROOT/resources/icons/flowmux-128.png" "$iconset/icon_128x128.png"
    cp "$REPO_ROOT/resources/icons/flowmux-256.png" "$iconset/icon_128x128@2x.png"
    cp "$REPO_ROOT/resources/icons/flowmux-256.png" "$iconset/icon_256x256.png"
    cp "$REPO_ROOT/resources/icons/flowmux-512.png" "$iconset/icon_256x256@2x.png"
    cp "$REPO_ROOT/resources/icons/flowmux-512.png" "$iconset/icon_512x512.png"
    cp "$REPO_ROOT/resources/icons/flowmux-1024.png" "$iconset/icon_512x512@2x.png"
    iconutil -c icns "$iconset" -o "$resources/flowmux.icns"
}

write_info_plist() {
    local path="$1"
    local version="$2"
    cat > "$path" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>FlowMux</string>
  <key>CFBundleExecutable</key>
  <string>flowmux</string>
  <key>CFBundleIconFile</key>
  <string>flowmux</string>
  <key>CFBundleIdentifier</key>
  <string>com.flowmux.App</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>FlowMux</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$version</string>
  <key>CFBundleVersion</key>
  <string>$version</string>
  <key>LSApplicationCategoryType</key>
  <string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key>
  <string>13.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSDocumentsFolderUsageDescription</key>
  <string>FlowMux needs access to Documents so terminal panes and file tools can use workspace files.</string>
  <key>NSDesktopFolderUsageDescription</key>
  <string>FlowMux needs access to Desktop so terminal panes and file tools can use workspace files.</string>
  <key>NSDownloadsFolderUsageDescription</key>
  <string>FlowMux needs access to Downloads so terminal panes and file tools can use workspace files.</string>
</dict>
</plist>
EOF
}

build_args=()
target_subdir="$PROFILE"
case "$PROFILE" in
    fast)
        build_args+=(--profile fast)
        ;;
    release)
        build_args+=(--release)
        ;;
    debug)
        ;;
    *)
        echo "error: unsupported profile: $PROFILE" >&2
        exit 2
        ;;
esac

configure_macos_path
preflight_macos
if [ "$CHECK_ONLY" = true ]; then
    echo "==> macOS native preflight checks passed"
    exit 0
fi

cd "$REPO_ROOT"
echo "==> building flowmux ($PROFILE)"
if [ "${#build_args[@]}" -gt 0 ]; then
    cargo build "${build_args[@]}" -p flowmux -p flowmux-cli
else
    cargo build -p flowmux -p flowmux-cli
fi

version="$(workspace_version)"
bundle_work="$REPO_ROOT/target/macos-bundle/FlowMux.app"
contents="$bundle_work/Contents"
macos="$contents/MacOS"
resources="$contents/Resources"
bundle_dest="$APP_DIR/FlowMux.app"
bundle_pending="$APP_DIR/.FlowMux.app.pending"
bundle_backup="$APP_DIR/.FlowMux.app.previous"

echo "==> creating $bundle_work"
rm -rf "$bundle_work"
mkdir -p "$macos" "$resources"
install -m755 "$REPO_ROOT/target/$target_subdir/flowmux" "$macos/flowmux"
install -m755 "$REPO_ROOT/target/$target_subdir/flowmuxctl" "$macos/flowmuxctl"
install -m644 \
    "$REPO_ROOT/LICENSE" \
    "$REPO_ROOT/NOTICE" \
    "$REPO_ROOT/THIRD_PARTY_LICENSES.md" \
    "$resources/"
create_icon "$resources"
write_info_plist "$contents/Info.plist" "$version"
plutil -lint "$contents/Info.plist" >/dev/null
xattr -cr "$bundle_work" "$macos/flowmux" "$macos/flowmuxctl"
codesign --force --sign - "$macos/flowmux" "$macos/flowmuxctl" >/dev/null
codesign --force --deep --sign - "$bundle_work" >/dev/null

mkdir -p "$APP_DIR" "$BIN_DIR"
running_pid="$(running_flowmux_pid "$bundle_dest/Contents/MacOS/flowmux" || true)"
if [ -n "$running_pid" ]; then
    echo "==> staging app update until FlowMux exits"
    rm -rf "$bundle_pending"
    cp -R "$bundle_work" "$bundle_pending"
    submit_deferred_app_swap "$running_pid" "$bundle_pending" "$bundle_dest" "$bundle_backup"
else
    echo "==> installing app to $bundle_dest"
    rm -rf "$bundle_dest" "$bundle_pending" "$bundle_backup"
    cp -R "$bundle_work" "$bundle_dest"
fi
install -m755 "$REPO_ROOT/target/$target_subdir/flowmux" "$BIN_DIR/flowmux"
install -m755 "$REPO_ROOT/target/$target_subdir/flowmuxctl" "$BIN_DIR/flowmuxctl"

if [ -n "$running_pid" ]; then
    echo "==> staged app update: $bundle_pending"
    echo "==> installed CLI:"
    echo "    $BIN_DIR/flowmux"
    echo "    $BIN_DIR/flowmuxctl"
    echo "==> restart FlowMux to finish installing $bundle_dest"
else
    echo "==> installed:"
    echo "    $bundle_dest"
    echo "    $BIN_DIR/flowmux"
    echo "    $BIN_DIR/flowmuxctl"
    echo "==> launch with: open \"$bundle_dest\""

    if [ "$LAUNCH" = true ]; then
        open "$bundle_dest"
    fi
fi
