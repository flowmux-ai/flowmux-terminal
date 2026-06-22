// SPDX-License-Identifier: GPL-3.0-or-later
//
// Build script for flowmux-terminal.
//
// When the `libghostty` cargo feature is enabled, this compiles the C shim
// (csrc/ghostty_shim.c) and links it against a static libghostty-vt built by
// scripts/build-ghostty-vt.sh. The feature is OFF by default, so a plain
// `cargo check`/`cargo build` does no native work and needs neither Zig nor
// the Ghostty source — the VTE-backed GUI remains the shipping default while
// this backend is developed (see crate docs).

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Only do native work for the opt-in backend. Cargo exposes enabled
    // features to build scripts as CARGO_FEATURE_<NAME>.
    if env::var_os("CARGO_FEATURE_LIBGHOSTTY").is_none() {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    // crates/flowmux-terminal -> workspace root.
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("flowmux-terminal lives at <root>/crates/flowmux-terminal")
        .to_path_buf();

    let prefix = resolve_prefix(&workspace_root);
    let include_dir = prefix.join("include");
    let lib_dir = prefix.join("lib");
    let static_lib = lib_dir.join("libghostty-vt.a");
    assert!(
        static_lib.is_file(),
        "expected {} after build-ghostty-vt.sh; got nothing",
        static_lib.display()
    );

    // Compile the stable shim against the pinned libghostty-vt headers.
    cc::Build::new()
        .file(manifest_dir.join("csrc/ghostty_shim.c"))
        .include(manifest_dir.join("csrc"))
        .include(&include_dir)
        .warnings(true)
        .compile("flowmux_ghostty_shim");

    // Link the static libghostty-vt. It is self-contained (the static .pc lists
    // no private deps); pthread/m cover the libc bits the Zig archive expects.
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=ghostty-vt");
    println!("cargo:rustc-link-lib=dylib=pthread");
    println!("cargo:rustc-link-lib=dylib=m");

    // Rebuild triggers.
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("csrc/ghostty_shim.c").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("csrc/ghostty_shim.h").display()
    );
    println!("cargo:rerun-if-changed={}", static_lib.display());
    println!("cargo:rerun-if-env-changed=FLOWMUX_GHOSTTY_VT_PREFIX");
}

/// Locate (or build) the libghostty-vt install prefix.
///
/// Priority:
/// 1. `FLOWMUX_GHOSTTY_VT_PREFIX` — a pre-built prefix (CI / packaging).
/// 2. `<workspace>/target/ghostty-vt/prefix` — built on demand by
///    scripts/build-ghostty-vt.sh (idempotent; needs Zig + network on the
///    first run only).
fn resolve_prefix(workspace_root: &std::path::Path) -> PathBuf {
    if let Some(p) = env::var_os("FLOWMUX_GHOSTTY_VT_PREFIX") {
        return PathBuf::from(p);
    }

    let default_prefix = workspace_root.join("target/ghostty-vt/prefix");
    if default_prefix.join("lib/libghostty-vt.a").is_file() {
        return default_prefix;
    }

    // Build it. The script is idempotent and prints the prefix on its last line.
    let script = workspace_root.join("scripts/build-ghostty-vt.sh");
    let status = Command::new("bash")
        .arg(&script)
        .arg(&default_prefix)
        .status()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", script.display()));
    assert!(
        status.success(),
        "{} failed; build libghostty-vt manually or set FLOWMUX_GHOSTTY_VT_PREFIX",
        script.display()
    );
    default_prefix
}
