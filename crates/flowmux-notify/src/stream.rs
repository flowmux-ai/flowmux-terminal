// SPDX-License-Identifier: GPL-3.0-or-later
//! Byte-stream OSC extractor.
//!
//! Some terminal backends (libghostty, raw PTY readers) hand flowmux raw
//! bytes from the child process. This module is a tiny state machine
//! that watches that byte stream, finds OSC sequences (`ESC ] ... ST`
//! or `ESC ] ... BEL`), and emits the payload to a callback. Other
//! bytes (CSI, SGR, regular text) are passed through untouched.
//!
//! VTE-based panes don't need this module — VTE exposes a
//! `bell-event` / OSC signal directly. We use this for the libghostty
//! backend and for piping `flowmux notify` output through stdin.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Outside any escape sequence.
    Ground,
    /// Last byte was ESC (0x1B).
    Esc,
    /// Inside ESC `]` payload.
    Osc,
    /// Inside ESC `]` payload after seeing ESC (looking for `\` to terminate).
    OscEsc,
}

pub struct OscExtractor<F: FnMut(&str)> {
    state: State,
    buf: Vec<u8>,
    on_osc: F,
}

impl<F: FnMut(&str)> OscExtractor<F> {
    pub fn new(on_osc: F) -> Self {
        Self {
            state: State::Ground,
            buf: Vec::with_capacity(64),
            on_osc,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.step(b);
        }
    }

    fn emit(&mut self) {
        if let Ok(s) = std::str::from_utf8(&self.buf) {
            (self.on_osc)(s);
        }
        self.buf.clear();
    }

    fn step(&mut self, b: u8) {
        match self.state {
            State::Ground => {
                if b == 0x1B {
                    self.state = State::Esc;
                }
            }
            State::Esc => {
                if b == b']' {
                    self.state = State::Osc;
                    self.buf.clear();
                } else {
                    // Any other CSI / sequence — ignore for our purposes.
                    self.state = State::Ground;
                }
            }
            State::Osc => match b {
                0x07 /* BEL */ => {
                    self.emit();
                    self.state = State::Ground;
                }
                0x1B /* ESC */ => {
                    self.state = State::OscEsc;
                }
                // Drop control chars except in payload to keep utf8 valid.
                _ => self.buf.push(b),
            },
            State::OscEsc => {
                if b == b'\\' {
                    // ST terminator (ESC \\)
                    self.emit();
                    self.state = State::Ground;
                } else {
                    // Spurious ESC inside payload; treat as data.
                    self.buf.push(0x1B);
                    self.buf.push(b);
                    self.state = State::Osc;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(bytes: &[u8]) -> Vec<String> {
        let mut out = vec![];
        let mut x = OscExtractor::new(|s| out.push(s.to_string()));
        x.feed(bytes);
        out
    }

    #[test]
    fn extracts_bel_terminated() {
        let s = b"hello\x1b]9;done\x07world";
        assert_eq!(collect(s), vec!["9;done"]);
    }

    #[test]
    fn extracts_st_terminated() {
        let s = b"\x1b]777;notify;Codex;ready\x1b\\";
        assert_eq!(collect(s), vec!["777;notify;Codex;ready"]);
    }

    #[test]
    fn handles_split_chunks() {
        let mut out = vec![];
        let mut x = OscExtractor::new(|s| out.push(s.to_string()));
        x.feed(b"\x1b]9");
        x.feed(b";split message");
        x.feed(b"\x07tail");
        assert_eq!(out, vec!["9;split message"]);
    }

    #[test]
    fn passes_through_non_osc_escapes() {
        // A CSI sequence (\x1b[31m) should not produce OSC events.
        assert!(collect(b"\x1b[31mred\x1b[0m").is_empty());
    }
}
