// SPDX-License-Identifier: GPL-3.0-or-later
//! Reusable daemon core. The handler logic lives here so both
//! `flowmux-daemon` (headless) and `flowmux` (GTK) embed the same
//! implementation. The GUI binary subscribes to events and updates the
//! widget tree; the headless binary just logs them.

pub mod handler;
pub mod state_store;

pub use handler::DaemonHandler;
pub use state_store::{CloseOutcome, StateStore};
