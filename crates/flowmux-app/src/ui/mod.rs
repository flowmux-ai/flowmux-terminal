// SPDX-License-Identifier: GPL-3.0-or-later
pub mod browser_pane;
pub mod sidebar;
pub mod terminal_pane;
pub mod window;
pub mod workspace_view;

pub use window::{spawn_dispatch_loop, WindowController};
