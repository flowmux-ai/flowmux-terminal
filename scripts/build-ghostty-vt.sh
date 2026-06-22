#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Build libghostty-vt (the headless VT state machine extracted from Ghostty)
# from a pinned source revision and install it to a prefix as a static
# library + C headers.
#
# Why: flowmux's terminal surfaces are migrating off the VTE GTK widget onto
# a libghostty-vt core that flowmux renders itself (see crate
# `flowmux-terminal`, cargo feature `libghostty`). libghostty-vt owns VT
# parsing, terminal state, scrollback and reflow; the GTK layer owns the
# PTY and the renderer. Unlike the patched VTE path (scripts/build-vte.sh),
# this links a *static* `libghostty-vt.a` with no extra runtime deps, so no
# rpath is required.
#
# Ghostty is MIT licensed (see the upstream LICENSE); flowmux is
# GPL-3.0-or-later, which MIT is compatible with. Attribution is recorded in
# crates/flowmux-terminal (NOTICE + module docs).
#
# Usage:
#   scripts/build-ghostty-vt.sh [PREFIX]
# PREFIX defaults to target/ghostty-vt/prefix under the repo root.
#
# Requires Zig 0.15.x on PATH (matches Ghostty's minimum_zig_version) and
# network access on the first run to fetch the pinned source + zig deps.
set -euo pipefail

# Pinned Ghostty revision. Kept in lockstep with the C API the flowmux-terminal
# FFI bindings are written against; bump deliberately and re-test the bindings.
GHOSTTY_REV="ae52f97dcac558735cfa916ea3965f247e5c6e9e"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${1:-$REPO_ROOT/target/ghostty-vt/prefix}"
WORK="${GHOSTTY_VT_BUILD_DIR:-$REPO_ROOT/target/ghostty-vt/src}"
SRC="$WORK/ghostty"

# Idempotent: a prior build that already produced the static lib is reused so
# build.rs and repeated `cargo test` runs do not re-clone/re-compile Ghostty.
if [ -f "$PREFIX/lib/libghostty-vt.a" ]; then
    echo "==> libghostty-vt already built at $PREFIX (skipping)"
    echo "$PREFIX"
    exit 0
fi

if ! command -v zig >/dev/null 2>&1; then
    echo "error: zig not found on PATH (Ghostty needs Zig 0.15.x)" >&2
    exit 1
fi

echo "==> libghostty-vt: rev=$GHOSTTY_REV prefix=$PREFIX"
mkdir -p "$SRC"
if [ ! -d "$SRC/.git" ]; then
    echo "==> fetching pinned Ghostty source (shallow)"
    git init -q "$SRC"
    git -C "$SRC" remote add origin https://github.com/ghostty-org/ghostty.git 2>/dev/null || true
    git -C "$SRC" fetch --depth 1 origin "$GHOSTTY_REV"
    git -C "$SRC" checkout -q FETCH_HEAD
fi

# Keep zig's package cache inside target/ so a `cargo clean` reclaims it and it
# never pollutes the user's global cache.
export ZIG_GLOBAL_CACHE_DIR="${ZIG_GLOBAL_CACHE_DIR:-$WORK/zig-cache}"

echo "==> zig build -Demit-lib-vt=true (ReleaseFast)"
( cd "$SRC" && zig build -Demit-lib-vt=true -Doptimize=ReleaseFast --prefix "$PREFIX" )

if [ ! -f "$PREFIX/lib/libghostty-vt.a" ]; then
    echo "error: build did not produce $PREFIX/lib/libghostty-vt.a" >&2
    exit 1
fi

echo "==> installed libghostty-vt:"
echo "    $PREFIX/lib/libghostty-vt.a"
echo "    $PREFIX/include/ghostty/vt.h"
# Print the prefix on the last line so callers (build.rs) can capture it.
echo "$PREFIX"
