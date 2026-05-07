// SPDX-License-Identifier: GPL-3.0-or-later
//! Config loaders for flowmux.
//!
//! Two distinct sources, intentionally orthogonal:
//!
//! * `cmux.json` — per-project file (cmux's documented schema for custom
//!   commands). Lives at the project root and is committed.
//! * `~/.config/ghostty/config` — the user's existing Ghostty config, used
//!   for fonts/colors/themes so flowmux renders consistently with their
//!   terminal. Read-only; never written.
//!
//! See `docs/upstream-mapping/config.md` for the field-by-field mapping.

pub mod cmux_json;
pub mod ghostty;
pub mod options;
pub mod paths;
pub mod theme;
