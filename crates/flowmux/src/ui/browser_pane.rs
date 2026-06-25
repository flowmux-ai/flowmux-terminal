// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(target_os = "linux")]
#[path = "browser_pane_webkit.rs"]
mod imp;

#[cfg(target_os = "macos")]
#[path = "browser_pane_macos.rs"]
mod imp;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[path = "browser_pane_stub.rs"]
mod imp;

pub use imp::*;
