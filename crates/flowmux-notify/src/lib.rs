// SPDX-License-Identifier: GPL-3.0-or-later
//! Two responsibilities:
//!
//! 1. Parse OSC notification escape sequences emitted by terminal
//!    programs into a structured [`OscNotification`].
//! 2. Send those notifications to the user's desktop via the
//!    `org.freedesktop.Notifications` D-Bus service (libnotify
//!    compatible).
//!
//! The OSC formats handled are the publicly documented terminal
//! escapes used by iTerm2, KDE Konsole, and rxvt-unicode:
//!
//! * OSC 9    — iTerm2 single-line notification
//! * OSC 99   — Konsole / KDE notification with options
//! * OSC 777  — rxvt-unicode `notify;<summary>;<body>`
//!
//! See `docs/upstream-mapping/notifications.md` for the cmux behavioral
//! spec we mirror (level inference, "Claude is waiting" detection, etc.).

pub mod osc;
pub mod sender;
pub mod stream;

pub use osc::{parse_osc, OscNotification};
pub use sender::DesktopNotifier;
pub use stream::OscExtractor;
