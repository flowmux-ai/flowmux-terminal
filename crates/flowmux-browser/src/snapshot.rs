// SPDX-License-Identifier: GPL-3.0-or-later
//! Serializable shape of the page snapshot the controller produces.
//!
//! The snapshot is built by [`crate::scripts::SNAPSHOT_JS`] running
//! inside the WebView and decoded by `serde_json` on the Rust side.
//!
//! Shape mirrors cmux's `v2BrowserSnapshot` response (Markdown-style
//! tree + ref→meta map + page metadata). Importantly, **the snapshot
//! does not stamp any `data-flowmux-ref` attribute on the page**: the
//! server keeps a `(surface_id, ref_token) → cssSelector` map (see
//! [`crate::refs::RefStore`]) so subsequent action calls can resolve
//! a ref to a CSS selector without needing the page to remember it.
//! This matches cmux's policy of leaving the live DOM untouched.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Top-level snapshot returned by the WebView script.
///
/// Wire shape (JSON):
/// ```text
/// {
///   "markdown": "- button \"OK\" [ref=e1]\n  - text \"Click me\"\n",
///   "refs": { "e1": { "role": "button", "name": "OK", "selector": "..." } },
///   "page": { "url": "...", "title": "...", "ready_state": "complete",
///             "text": "...", "html": null }
/// }
/// ```
///
/// `refs` is a `BTreeMap` so JSON output is key-sorted and stable
/// across runs — useful for snapshot test goldens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomSnapshot {
    /// Tree-shaped Markdown the agent reads to plan its next click.
    pub markdown: String,
    /// `ref_token → meta`. Each ref's `selector` is what the server
    /// remembers in [`crate::refs::RefStore`].
    pub refs: BTreeMap<String, RefMeta>,
    /// Page-level metadata.
    pub page: PageMeta,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefMeta {
    /// ARIA role (or implicit role inferred from the tag).
    pub role: String,
    /// Best-effort accessible name (aria-label, alt, title,
    /// placeholder, or trimmed innerText).
    pub name: String,
    /// CSS path the server stores in the RefStore.
    pub selector: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageMeta {
    pub url: String,
    pub title: String,
    /// `document.readyState`: `"loading" | "interactive" | "complete"`.
    pub ready_state: String,
    /// Truncated `body.innerText` so agents have a quick summary
    /// without needing a separate read call.
    pub text: String,
    /// Optional full HTML — usually omitted for size; populated only
    /// when callers explicitly ask (`snapshot --include-html`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
}

impl DomSnapshot {
    pub fn empty(url: impl Into<String>) -> Self {
        Self {
            markdown: String::new(),
            refs: BTreeMap::new(),
            page: PageMeta {
                url: url.into(),
                title: String::new(),
                ready_state: "loading".into(),
                text: String::new(),
                html: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DomSnapshot {
        let mut refs = BTreeMap::new();
        refs.insert(
            "e1".into(),
            RefMeta {
                role: "textbox".into(),
                name: "Email".into(),
                selector: "form#login > input:nth-of-type(1)".into(),
            },
        );
        refs.insert(
            "e2".into(),
            RefMeta {
                role: "button".into(),
                name: "Sign in".into(),
                selector: "form#login > button:nth-of-type(1)".into(),
            },
        );
        DomSnapshot {
            markdown: "- textbox \"Email\" [ref=e1]\n- button \"Sign in\" [ref=e2]\n".into(),
            refs,
            page: PageMeta {
                url: "https://example.com/login".into(),
                title: "Sign in".into(),
                ready_state: "complete".into(),
                text: "Email Password Sign in".into(),
                html: None,
            },
        }
    }

    #[test]
    fn snapshot_roundtrips_through_json() {
        let s = sample();
        let json = serde_json::to_string(&s).unwrap();
        let back: DomSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn refs_map_is_key_sorted_in_json() {
        let s = sample();
        let json = serde_json::to_string(&s).unwrap();
        // BTreeMap serializes keys in sorted order. e1 must appear
        // before e2 in the JSON text.
        let i1 = json.find("\"e1\"").expect("e1 in json");
        let i2 = json.find("\"e2\"").expect("e2 in json");
        assert!(i1 < i2, "ref keys should be sorted: {json}");
    }

    #[test]
    fn empty_constructor_uses_loading_ready_state() {
        let s = DomSnapshot::empty("about:blank");
        assert_eq!(s.page.url, "about:blank");
        assert_eq!(s.page.title, "");
        assert!(s.refs.is_empty());
        assert_eq!(s.markdown, "");
        assert_eq!(s.page.ready_state, "loading");
    }

    #[test]
    fn page_html_is_omitted_when_none() {
        let s = DomSnapshot::empty("about:blank");
        let json = serde_json::to_string(&s).unwrap();
        // `html: None` should not produce a `"html":null` key.
        assert!(
            !json.contains("\"html\""),
            "html must be omitted when None: {json}"
        );
    }

    #[test]
    fn snapshot_parses_real_world_payload() {
        let raw = r#"{
            "markdown": "- button \"OK\" [ref=e1]\n",
            "refs": {
                "e1": {
                    "role": "button",
                    "name": "OK",
                    "selector": "body > button:nth-of-type(1)"
                }
            },
            "page": {
                "url": "https://x.test/",
                "title": "X",
                "ready_state": "complete",
                "text": "OK"
            }
        }"#;
        let s: DomSnapshot = serde_json::from_str(raw).unwrap();
        assert_eq!(s.refs.len(), 1);
        let meta = s.refs.get("e1").unwrap();
        assert_eq!(meta.role, "button");
        assert_eq!(meta.selector, "body > button:nth-of-type(1)");
        assert_eq!(s.page.html, None);
    }
}
