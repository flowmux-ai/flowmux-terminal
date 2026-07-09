// SPDX-License-Identifier: GPL-3.0-or-later
//! Named-key -> escape-byte mapping for the send-key command.
//!
//! Split out of `main.rs` (pure move; behavior unchanged).

/// Translate a named terminal key (`Enter`, `Tab`, `ArrowUp`, …) into
/// the byte sequence a PTY expects. A single character passes through as
/// itself. Used by `flowmux send-key` for tmux-style key-name input;
/// raw byte/escape input still goes through `send-keys`.
pub(crate) fn named_key_to_bytes(key: &str) -> anyhow::Result<String> {
    let seq = match key {
        "Enter" | "Return" | "CR" => "\r",
        "Tab" => "\t",
        "Escape" | "Esc" => "\x1b",
        "Backspace" | "BSpace" => "\x7f",
        "Delete" | "DC" => "\x1b[3~",
        "Space" => " ",
        "Up" | "ArrowUp" => "\x1b[A",
        "Down" | "ArrowDown" => "\x1b[B",
        "Right" | "ArrowRight" => "\x1b[C",
        "Left" | "ArrowLeft" => "\x1b[D",
        "Home" => "\x1b[H",
        "End" => "\x1b[F",
        "PageUp" | "PPage" => "\x1b[5~",
        "PageDown" | "NPage" => "\x1b[6~",
        // A bare single character (e.g. `a`, `:`) is sent verbatim.
        other if other.chars().count() == 1 => return Ok(other.to_string()),
        other => {
            anyhow::bail!("unknown key name {other:?}; use a named key (Enter, Tab, ArrowUp, …), a single character, or `send-keys` for raw input")
        }
    };
    Ok(seq.to_string())
}
