// SPDX-License-Identifier: GPL-3.0-or-later
pub mod agent_bar;
mod browser_bookmarks;
mod browser_downloads;
pub mod browser_pane;
pub mod editor_pane;
pub mod file_browser;
pub mod ghostty_pane;
pub mod image_viewer;
pub mod keybindings_panel;
pub mod options_dialog;
pub mod overlay_menu;
pub mod pane_terminal;
pub mod popover_pos;
pub mod show_in_folder;
pub mod sidebar;
pub mod theme_tab;
pub mod thorvg;
pub mod update_banner;
pub mod usage_popover;
pub mod window;
pub mod workspace_view;
#[allow(dead_code)] // Standalone in this task; controller wiring follows separately.
pub mod worktree_panel;

pub use window::{spawn_dispatch_loop, WindowController};
