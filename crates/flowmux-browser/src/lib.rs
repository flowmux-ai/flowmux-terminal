// SPDX-License-Identifier: GPL-3.0-or-later
//! Common browser-controller types shared between flowmux-app's WebKit
//! pane implementation, the IPC/CLI layer, and headless test mocks.
//!
//! flowmux ships an in-pane browser modeled after the cmux feature set
//! (page snapshot, scripted click / fill / eval, profile-aware
//! cookie storage). The actual rendering uses WebKitGTK 6 inside
//! flowmux-app, but this crate keeps everything that doesn't need GTK
//! — the trait surface, the snapshot data shape, the JavaScript
//! helpers that get evaluated on the page, and the profile model —
//! so it can be unit-tested without a display.
//!
//! Public surface:
//!
//! * [`BrowserController`]  — async trait every concrete controller
//!   implements (WebKit, mock, future libcef bindings, …).
//! * [`DomSnapshot`] — serde-stable shape for the snapshot the
//!   page-side JS returns (Markdown tree + ref→meta map + page meta).
//! * [`refs::RefStore`] — server-side `(scope, ref_token) → cssSelector`
//!   map that subsequent `click`/`fill`/etc. calls resolve through.
//! * [`BrowserProfile`]  — pick a cookie / data store: WebKit
//!   default, Firefox import, Chrome import, named custom profile.
//! * [`scripts`]         — string constants holding the JS the
//!   controller injects into the page.

pub mod controller;
pub mod profile;
pub mod refs;
pub mod scripts;
pub mod snapshot;

pub use controller::{BrowserController, BrowserError};
pub use profile::{BrowserProfile, ProfileError};
pub use refs::{RefScope, RefStore};
pub use snapshot::{DomSnapshot, PageMeta, RefMeta};
