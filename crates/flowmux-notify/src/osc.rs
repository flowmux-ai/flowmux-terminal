// SPDX-License-Identifier: GPL-3.0-or-later
//! OSC notification sequence parser.
//!
//! Inputs are the *payload* between `ESC ]` and the terminator (`BEL` or
//! `ESC \`); the caller is responsible for stripping the framing.
//!
//! Format references (all public):
//!
//! * iTerm2 OSC 9     `9 ; <body>`
//! * Konsole OSC 99   `99 ; <key=val>;<key=val>... ; <body>`
//! * urxvt   OSC 777  `777 ; notify ; <summary> [ ; <body> ]`

use flowmux_core::NotificationLevel;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OscNotification {
    pub title: String,
    pub body: String,
    pub level: NotificationLevel,
}

pub fn parse_osc(payload: &str) -> Option<OscNotification> {
    let (code, rest) = payload.split_once(';')?;
    match code.trim() {
        "9" => Some(parse_osc_9(rest)),
        "99" => Some(parse_osc_99(rest)),
        "777" => parse_osc_777(rest),
        _ => None,
    }
}

fn parse_osc_9(body: &str) -> OscNotification {
    let body = sanitize(body);
    let level = infer_level(&body);
    OscNotification {
        title: "Terminal".into(),
        body,
        level,
    }
}

fn parse_osc_99(rest: &str) -> OscNotification {
    // `99 ; opts ; body` — opts may itself be empty.
    let (_opts, body) = match rest.split_once(';') {
        Some((opts, body)) => (opts, body),
        None => ("", rest),
    };
    let body = sanitize(body);
    let level = infer_level(&body);
    OscNotification {
        title: "Terminal".into(),
        body,
        level,
    }
}

fn parse_osc_777(rest: &str) -> Option<OscNotification> {
    // urxvt: `777 ; notify ; <summary> [ ; <body> ]`
    let mut parts = rest.splitn(3, ';');
    let kind = parts.next()?.trim();
    if kind != "notify" {
        return None;
    }
    let summary = sanitize(parts.next()?);
    let body = sanitize(parts.next().unwrap_or(""));
    let level = infer_level(if body.is_empty() { &summary } else { &body });
    Some(OscNotification {
        title: summary,
        body,
        level,
    })
}

/// Strip control characters from an OSC field before it reaches the
/// user-visible bell popover. Real notification bodies are plain text;
/// anything else (ESC, BEL, OSC remnants from a misbehaving emitter)
/// would render as garbled boxes in the GTK label and tripled as the
/// "Terminal / 4;0;rgb…" entries the user reported. Tab + newline are
/// kept so multi-line messages survive verbatim.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

/// Heuristic level inference. Agent messages containing "waiting" /
/// "input" promote to attention.
fn infer_level(text: &str) -> NotificationLevel {
    let t = text.to_ascii_lowercase();
    if t.contains("error") || t.contains("failed") {
        NotificationLevel::Error
    } else if t.contains("waiting")
        || t.contains("needs your input")
        || t.contains("attention")
        || t.contains("approval")
    {
        NotificationLevel::AttentionNeeded
    } else {
        NotificationLevel::Info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc_9_iterm() {
        let n = parse_osc("9;Build complete").unwrap();
        assert_eq!(n.body, "Build complete");
        assert_eq!(n.level, NotificationLevel::Info);
    }

    #[test]
    fn osc_99_konsole_with_opts() {
        let n = parse_osc("99;urgency=critical;Claude is waiting for your input").unwrap();
        assert_eq!(n.body, "Claude is waiting for your input");
        assert_eq!(n.level, NotificationLevel::AttentionNeeded);
    }

    #[test]
    fn osc_777_urxvt_two_parts() {
        let n = parse_osc("777;notify;Codex;needs approval").unwrap();
        assert_eq!(n.title, "Codex");
        assert_eq!(n.body, "needs approval");
        assert_eq!(n.level, NotificationLevel::AttentionNeeded);
    }

    #[test]
    fn unknown_osc_returns_none() {
        assert!(parse_osc("4;0;rgb:11/22/33").is_none());
    }

    // -- 5 variant scenarios per agent + 1 error provoke ------------

    #[test]
    fn osc_9_recognizes_claude_style_completion_messages() {
        let n = parse_osc("9;Claude finished — review the diff").unwrap();
        assert_eq!(n.title, "Terminal");
        assert_eq!(n.body, "Claude finished — review the diff");
        assert_eq!(n.level, NotificationLevel::Info);
    }

    #[test]
    fn osc_9_promotes_attention_when_body_says_waiting() {
        let n = parse_osc("9;Codex is waiting for your review").unwrap();
        assert_eq!(n.level, NotificationLevel::AttentionNeeded);
    }

    #[test]
    fn osc_99_with_empty_options_field_still_parses_body() {
        let n = parse_osc("99;;OpenCode ready").unwrap();
        assert_eq!(n.body, "OpenCode ready");
    }

    #[test]
    fn osc_777_without_body_still_parses_summary() {
        let n = parse_osc("777;notify;Claude").unwrap();
        assert_eq!(n.title, "Claude");
        assert_eq!(n.body, "");
        assert_eq!(n.level, NotificationLevel::Info);
    }

    #[test]
    fn osc_777_non_notify_kind_returns_none() {
        // urxvt OSC 777 also carries other kinds (e.g. iconBeep). They
        // are not desktop notifications and must not be promoted.
        assert!(parse_osc("777;iconBeep;something").is_none());
    }

    #[test]
    fn missing_semicolon_returns_none_instead_of_panicking() {
        // Provoke malformed OSC: a bare code with no separator.
        assert!(parse_osc("9").is_none());
        assert!(parse_osc("").is_none());
    }
}
