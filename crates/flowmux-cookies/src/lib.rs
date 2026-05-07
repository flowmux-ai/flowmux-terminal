// SPDX-License-Identifier: GPL-3.0-or-later
//! Browser cookie + session import.
//!
//! Each browser stores cookies in its own SQLite database under its
//! profile directory. Most Chromium-family browsers also encrypt the
//! `value` column with a key kept in libsecret / Secret Service /
//! GNOME Keyring; Firefox stores cookies in plaintext.
//!
//! `flowmux-cookies` exposes a [`Source`] trait that abstracts the
//! per-browser layout, plus concrete implementations:
//!
//! * `firefox` — fully working (plaintext SQLite).
//! * `chromium`, `chrome`, `brave`, `edge`, `arc` — share one
//!   Chromium-family implementation; encrypted-value extraction is a
//!   stub today and returns `Error::EncryptedValuesUnsupported` until
//!   libsecret integration lands.

pub mod chromium;
pub mod cookie;
pub mod firefox;
pub mod source;

pub use cookie::Cookie;
pub use source::{discover_sources, BrowserId, Source};
