// SPDX-License-Identifier: GPL-3.0-or-later
//! Backend-agnostic terminal pane wrapper.
//!
//! The pane registry stores `PaneTerminal` rather than a concrete terminal so
//! the GUI can host either the VTE-backed [`TerminalPane`] (the default) or the
//! libghostty-backed [`GhosttyPane`] (opt-in, task C) without the rest of the
//! app caring which one a surface uses. The VTE variant forwards verbatim to
//! the unchanged `TerminalPane`, so enabling the libghostty backend cannot
//! regress the VTE path.
//!
//! Only the methods callers invoke on a *stored* terminal live here. Backend-
//! specific setup that runs at spawn time (theme application, OSC title/cwd
//! notify wiring) is done on the concrete type before wrapping — see
//! `workspace_view.rs`.

use std::path::PathBuf;

use gtk::prelude::IsA;

use flowmux_core::PaneId;

use crate::ui::terminal_pane::TerminalPane;

#[cfg(feature = "libghostty")]
use crate::ui::ghostty_pane::GhosttyPane;

/// A terminal surface backed by either VTE or libghostty-vt.
#[derive(Clone)]
pub enum PaneTerminal {
    Vte(TerminalPane),
    #[cfg(feature = "libghostty")]
    Ghostty(GhosttyPane),
}

impl PaneTerminal {
    /// The owning leaf pane id.
    pub fn id(&self) -> PaneId {
        match self {
            PaneTerminal::Vte(t) => t.id,
            #[cfg(feature = "libghostty")]
            PaneTerminal::Ghostty(g) => g.id,
        }
    }

    pub fn root_widget(&self) -> gtk::Widget {
        match self {
            PaneTerminal::Vte(t) => t.root_widget(),
            #[cfg(feature = "libghostty")]
            PaneTerminal::Ghostty(g) => g.root_widget(),
        }
    }

    pub fn grab_focus(&self) {
        match self {
            PaneTerminal::Vte(t) => t.grab_focus(),
            #[cfg(feature = "libghostty")]
            PaneTerminal::Ghostty(g) => g.grab_focus(),
        }
    }

    pub fn current_dir(&self) -> Option<PathBuf> {
        match self {
            PaneTerminal::Vte(t) => t.current_dir(),
            #[cfg(feature = "libghostty")]
            PaneTerminal::Ghostty(g) => g.current_dir(),
        }
    }

    pub fn set_font_scale(&self, scale: f64) {
        match self {
            PaneTerminal::Vte(t) => t.set_font_scale(scale),
            #[cfg(feature = "libghostty")]
            PaneTerminal::Ghostty(g) => g.set_font_scale(scale),
        }
    }

    pub fn set_font(&self, desc: &gtk::pango::FontDescription) {
        match self {
            PaneTerminal::Vte(t) => t.set_font(desc),
            #[cfg(feature = "libghostty")]
            PaneTerminal::Ghostty(g) => g.set_font(desc),
        }
    }

    pub fn has_selection(&self) -> bool {
        match self {
            PaneTerminal::Vte(t) => t.has_selection(),
            #[cfg(feature = "libghostty")]
            PaneTerminal::Ghostty(g) => g.has_selection(),
        }
    }

    pub fn copy_selection_to_clipboard(&self) {
        match self {
            PaneTerminal::Vte(t) => t.copy_selection_to_clipboard(),
            #[cfg(feature = "libghostty")]
            PaneTerminal::Ghostty(g) => g.copy_selection_to_clipboard(),
        }
    }

    pub fn paste_clipboard(&self) {
        match self {
            PaneTerminal::Vte(t) => t.paste_clipboard(),
            #[cfg(feature = "libghostty")]
            PaneTerminal::Ghostty(g) => g.paste_clipboard(),
        }
    }

    pub fn feed(&self, bytes: &[u8]) {
        match self {
            PaneTerminal::Vte(t) => t.feed(bytes),
            #[cfg(feature = "libghostty")]
            PaneTerminal::Ghostty(g) => g.feed(bytes),
        }
    }

    pub fn feed_after_preedit_commit(&self, bytes: &'static [u8]) {
        match self {
            PaneTerminal::Vte(t) => t.feed_after_preedit_commit(bytes),
            #[cfg(feature = "libghostty")]
            PaneTerminal::Ghostty(g) => g.feed_after_preedit_commit(bytes),
        }
    }

    pub fn screen_text(&self) -> Option<String> {
        match self {
            PaneTerminal::Vte(t) => t.screen_text(),
            #[cfg(feature = "libghostty")]
            PaneTerminal::Ghostty(g) => g.screen_text(),
        }
    }

    pub fn add_controller(&self, controller: impl IsA<gtk::EventController>) {
        match self {
            PaneTerminal::Vte(t) => t.add_controller(controller),
            #[cfg(feature = "libghostty")]
            PaneTerminal::Ghostty(g) => g.add_controller(controller),
        }
    }

    /// Test-only access to the underlying VTE widget for the VTE-identity
    /// assertions in `window.rs`'s split tests (which only build VTE panes).
    #[cfg(test)]
    pub fn vte_widget(&self) -> &vte::Terminal {
        match self {
            PaneTerminal::Vte(t) => &t.widget,
            #[cfg(feature = "libghostty")]
            PaneTerminal::Ghostty(_) => panic!("vte_widget() called on a libghostty pane"),
        }
    }
}
