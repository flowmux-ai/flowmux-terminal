// SPDX-License-Identifier: GPL-3.0-or-later
//! Embed an absolute rpath pointing at the libghostty-vt build output.
//!
//! libghostty-vt-sys builds `libghostty-vt.so` inside its `OUT_DIR`
//! and only emits `cargo:rustc-link-search` (a build-time path). The
//! linked binary therefore has no `RUNPATH`/`RPATH`, so running
//! `./target/release/flowmux` straight out of a fresh build fails with
//! "libghostty-vt.so.0: cannot open shared object file".
//!
//! The sys crate uses `links = "ghostty-vt"` and exposes its include
//! directory via `cargo:include=`. Cargo forwards that to dependents
//! as `DEP_GHOSTTY_VT_INCLUDE`. The matching shared library lives in
//! the sibling `lib/` directory of the same install prefix, so derive
//! that path and emit it as an absolute rpath.
//!
//! Flatpak installs the shared object into `/app/lib`, which the
//! runtime's loader already searches, so leaving this rpath in place
//! is harmless: the build-tree path no longer exists at runtime and
//! the loader falls through to the system search.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=DEP_GHOSTTY_VT_INCLUDE");

    let Ok(include) = std::env::var("DEP_GHOSTTY_VT_INCLUDE") else {
        return;
    };
    let include_dir = PathBuf::from(include);
    let Some(prefix) = include_dir.parent() else {
        return;
    };
    let lib_dir = prefix.join("lib");
    if !lib_dir.is_dir() {
        return;
    }
    println!(
        "cargo:rustc-link-arg=-Wl,-rpath,{}",
        lib_dir.display()
    );
}
