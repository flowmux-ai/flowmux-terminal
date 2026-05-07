// SPDX-License-Identifier: GPL-3.0-or-later
//! In-process notification log shown in the sidebar's bell popover.
//!
//! flowmux already forwards every `Request::Notify` to the desktop via
//! `org.freedesktop.Notifications` (flowmux-notify). The GUI also keeps
//! a small in-memory transcript so the user can scroll past
//! notifications even after the OS toast fades — pressing the bell
//! button at the top of the sidebar opens a popover listing them.
//!
//! Entries are flagged `seen` once the popover opens, so subsequent
//! views show old items dim while fresh ones stand out.

use flowmux_core::NotificationLevel;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub struct NotificationEntry {
    pub title: String,
    pub body: String,
    pub level: NotificationLevel,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub seen: bool,
}

pub type NotificationLog = Rc<RefCell<Vec<NotificationEntry>>>;

pub fn new_log() -> NotificationLog {
    Rc::new(RefCell::new(Vec::new()))
}
