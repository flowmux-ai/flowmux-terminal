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
    let body = body.trim();
    OscNotification {
        title: "Terminal".into(),
        body: body.to_string(),
        level: infer_level(body),
    }
}

fn parse_osc_99(rest: &str) -> OscNotification {
    // `99 ; opts ; body` — opts may itself be empty.
    let (_opts, body) = match rest.split_once(';') {
        Some((opts, body)) => (opts, body),
        None => ("", rest),
    };
    OscNotification {
        title: "Terminal".into(),
        body: body.trim().to_string(),
        level: infer_level(body),
    }
}

fn parse_osc_777(rest: &str) -> Option<OscNotification> {
    // urxvt: `777 ; notify ; <summary> [ ; <body> ]`
    let mut parts = rest.splitn(3, ';');
    let kind = parts.next()?.trim();
    if kind != "notify" {
        return None;
    }
    let summary = parts.next()?.trim().to_string();
    let body = parts.next().unwrap_or("").trim().to_string();
    let level = infer_level(if body.is_empty() { &summary } else { &body });
    Some(OscNotification {
        title: summary,
        body,
        level,
    })
}

/// Heuristic level inference. cmux's documented behavior is that any
/// agent message containing "waiting" / "input" promotes to attention.
/// See docs/upstream-mapping/notifications.md.
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
}
