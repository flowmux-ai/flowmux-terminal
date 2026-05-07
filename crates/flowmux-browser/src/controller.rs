// SPDX-License-Identifier: GPL-3.0-or-later
//! Async trait every concrete browser controller (WebKit pane,
//! headless mock, future libcef binding, …) implements.
//!
//! The methods are deliberately fine-grained — `click`, `fill`,
//! `type_keys`, `press`, `select`, `scroll`, `text_of`, `value_of`,
//! `attr_of` — so the IPC and CLI layers can map one flowmux verb to
//! exactly one trait call.
//!
//! `async_trait` is used so the trait object stays usable
//! (`Box<dyn BrowserController>`) for the IPC dispatcher.

use crate::DomSnapshot;
use async_trait::async_trait;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum BrowserError {
    /// `data-flowmux-ref` attribute set by the most recent snapshot is
    /// no longer in the live DOM (page navigated, element removed).
    #[error("element ref not found: {0}")]
    RefNotFound(String),
    /// JS evaluation threw or returned an unexpected shape.
    #[error("eval failed: {0}")]
    Eval(String),
    /// Navigation rejected (bad URL, network failure surfaced
    /// synchronously by WebKit).
    #[error("navigation failed: {0}")]
    Nav(String),
    /// Snapshot JSON couldn't be decoded into [`DomSnapshot`].
    #[error("snapshot decode: {0}")]
    Decode(String),
    /// Backend transport (IPC channel closed, etc.).
    #[error("transport: {0}")]
    Transport(String),
}

#[async_trait(?Send)]
pub trait BrowserController {
    // ---- navigation ----------------------------------------------------
    async fn navigate(&self, url: &str) -> Result<(), BrowserError>;
    async fn back(&self) -> Result<bool, BrowserError>;
    async fn forward(&self) -> Result<bool, BrowserError>;
    async fn reload(&self) -> Result<(), BrowserError>;

    // ---- introspection ------------------------------------------------
    async fn url(&self) -> Result<String, BrowserError>;
    async fn title(&self) -> Result<String, BrowserError>;
    async fn snapshot(&self) -> Result<DomSnapshot, BrowserError>;

    // ---- low-level eval ----------------------------------------------
    /// Run arbitrary JavaScript in the page context and return the
    /// stringified result (whatever the JS expression evaluates to).
    async fn eval(&self, source: &str) -> Result<String, BrowserError>;

    // ---- element interactions ----------------------------------------
    async fn click(&self, ref_id: &str) -> Result<(), BrowserError>;
    async fn fill(&self, ref_id: &str, value: &str) -> Result<(), BrowserError>;
    async fn select_option(&self, ref_id: &str, value: &str) -> Result<(), BrowserError>;
    async fn scroll(&self, ref_id: &str, x: i32, y: i32) -> Result<(), BrowserError>;

    // ---- keyboard input ----------------------------------------------
    async fn type_keys(&self, text: &str) -> Result<(), BrowserError>;
    async fn press(&self, key: &str) -> Result<(), BrowserError>;

    // ---- read element state ------------------------------------------
    async fn text_of(&self, ref_id: &str) -> Result<String, BrowserError>;
    async fn value_of(&self, ref_id: &str) -> Result<String, BrowserError>;
    async fn attr_of(&self, ref_id: &str, name: &str) -> Result<String, BrowserError>;
}

#[cfg(test)]
mod tests {
    //! These tests exercise the trait shape via a synchronous mock.
    //! The mock isn't a meaningful browser — it just verifies that
    //! the trait surface is implementable from outside the crate
    //! and behaves as documented for the simple cases (record &
    //! replay).

    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Debug, Default)]
    struct MockState {
        url: String,
        title: String,
        nodes: HashMap<String, MockNode>,
        log: Vec<String>,
        history: Vec<String>,
        history_idx: i32,
    }

    #[derive(Debug, Clone, Default)]
    struct MockNode {
        text: String,
        value: String,
        attrs: HashMap<String, String>,
    }

    struct MockBrowser {
        state: RefCell<MockState>,
    }

    impl MockBrowser {
        fn new() -> Self {
            Self {
                state: RefCell::new(MockState {
                    url: "about:blank".into(),
                    title: "".into(),
                    history: vec!["about:blank".into()],
                    history_idx: 0,
                    ..Default::default()
                }),
            }
        }

        fn add_node(&self, r: &str, n: MockNode) {
            self.state.borrow_mut().nodes.insert(r.into(), n);
        }

        fn log(&self, s: impl Into<String>) {
            self.state.borrow_mut().log.push(s.into());
        }

        fn calls(&self) -> Vec<String> {
            self.state.borrow().log.clone()
        }
    }

    #[async_trait(?Send)]
    impl BrowserController for MockBrowser {
        async fn navigate(&self, url: &str) -> Result<(), BrowserError> {
            self.log(format!("navigate {url}"));
            let mut s = self.state.borrow_mut();
            s.url = url.into();
            let cutoff = (s.history_idx + 1) as usize;
            s.history.truncate(cutoff);
            s.history.push(url.into());
            s.history_idx = (s.history.len() - 1) as i32;
            Ok(())
        }
        async fn back(&self) -> Result<bool, BrowserError> {
            self.log("back");
            let mut s = self.state.borrow_mut();
            if s.history_idx > 0 {
                s.history_idx -= 1;
                s.url = s.history[s.history_idx as usize].clone();
                Ok(true)
            } else {
                Ok(false)
            }
        }
        async fn forward(&self) -> Result<bool, BrowserError> {
            self.log("forward");
            let mut s = self.state.borrow_mut();
            if (s.history_idx as usize) + 1 < s.history.len() {
                s.history_idx += 1;
                s.url = s.history[s.history_idx as usize].clone();
                Ok(true)
            } else {
                Ok(false)
            }
        }
        async fn reload(&self) -> Result<(), BrowserError> {
            self.log("reload");
            Ok(())
        }
        async fn url(&self) -> Result<String, BrowserError> {
            Ok(self.state.borrow().url.clone())
        }
        async fn title(&self) -> Result<String, BrowserError> {
            Ok(self.state.borrow().title.clone())
        }
        async fn snapshot(&self) -> Result<DomSnapshot, BrowserError> {
            let s = self.state.borrow();
            Ok(DomSnapshot::empty(&s.url))
        }
        async fn eval(&self, src: &str) -> Result<String, BrowserError> {
            self.log(format!("eval {src}"));
            Ok("mock".into())
        }
        async fn click(&self, r: &str) -> Result<(), BrowserError> {
            self.log(format!("click {r}"));
            if !self.state.borrow().nodes.contains_key(r) {
                return Err(BrowserError::RefNotFound(r.into()));
            }
            Ok(())
        }
        async fn fill(&self, r: &str, v: &str) -> Result<(), BrowserError> {
            self.log(format!("fill {r} {v}"));
            let mut s = self.state.borrow_mut();
            let n = s
                .nodes
                .get_mut(r)
                .ok_or(BrowserError::RefNotFound(r.into()))?;
            n.value = v.into();
            Ok(())
        }
        async fn select_option(&self, r: &str, v: &str) -> Result<(), BrowserError> {
            self.log(format!("select {r} {v}"));
            self.fill(r, v).await
        }
        async fn scroll(&self, r: &str, x: i32, y: i32) -> Result<(), BrowserError> {
            self.log(format!("scroll {r} {x} {y}"));
            Ok(())
        }
        async fn type_keys(&self, text: &str) -> Result<(), BrowserError> {
            self.log(format!("type {text}"));
            Ok(())
        }
        async fn press(&self, k: &str) -> Result<(), BrowserError> {
            self.log(format!("press {k}"));
            Ok(())
        }
        async fn text_of(&self, r: &str) -> Result<String, BrowserError> {
            self.state
                .borrow()
                .nodes
                .get(r)
                .map(|n| n.text.clone())
                .ok_or(BrowserError::RefNotFound(r.into()))
        }
        async fn value_of(&self, r: &str) -> Result<String, BrowserError> {
            self.state
                .borrow()
                .nodes
                .get(r)
                .map(|n| n.value.clone())
                .ok_or(BrowserError::RefNotFound(r.into()))
        }
        async fn attr_of(&self, r: &str, k: &str) -> Result<String, BrowserError> {
            let s = self.state.borrow();
            let n = s.nodes.get(r).ok_or(BrowserError::RefNotFound(r.into()))?;
            n.attrs
                .get(k)
                .cloned()
                .ok_or(BrowserError::RefNotFound(format!("{r}.{k}")))
        }
    }

    #[tokio::test]
    async fn navigate_pushes_history_and_back_pops() {
        let b = MockBrowser::new();
        b.navigate("https://a/").await.unwrap();
        b.navigate("https://b/").await.unwrap();
        assert_eq!(b.url().await.unwrap(), "https://b/");
        assert!(b.back().await.unwrap());
        assert_eq!(b.url().await.unwrap(), "https://a/");
        assert!(b.back().await.unwrap());
        assert_eq!(b.url().await.unwrap(), "about:blank");
        assert!(!b.back().await.unwrap()); // already at start
    }

    #[tokio::test]
    async fn forward_after_back_replays_history() {
        let b = MockBrowser::new();
        b.navigate("https://a/").await.unwrap();
        b.navigate("https://b/").await.unwrap();
        b.back().await.unwrap();
        assert_eq!(b.url().await.unwrap(), "https://a/");
        assert!(b.forward().await.unwrap());
        assert_eq!(b.url().await.unwrap(), "https://b/");
        assert!(!b.forward().await.unwrap());
    }

    #[tokio::test]
    async fn navigate_after_back_drops_forward_history() {
        let b = MockBrowser::new();
        b.navigate("https://a/").await.unwrap();
        b.navigate("https://b/").await.unwrap();
        b.back().await.unwrap();
        b.navigate("https://c/").await.unwrap();
        assert!(!b.forward().await.unwrap());
    }

    #[tokio::test]
    async fn fill_then_value_of_returns_input() {
        let b = MockBrowser::new();
        b.add_node("e1", MockNode::default());
        b.fill("e1", "hello").await.unwrap();
        assert_eq!(b.value_of("e1").await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn click_unknown_ref_returns_ref_not_found() {
        let b = MockBrowser::new();
        let err = b.click("e404").await.unwrap_err();
        assert_eq!(err, BrowserError::RefNotFound("e404".into()));
    }

    #[tokio::test]
    async fn each_call_is_logged_in_order() {
        let b = MockBrowser::new();
        b.add_node("e1", MockNode::default());
        b.navigate("https://x/").await.unwrap();
        b.fill("e1", "u").await.unwrap();
        b.click("e1").await.unwrap();
        b.press("Enter").await.unwrap();
        assert_eq!(
            b.calls(),
            vec![
                "navigate https://x/".to_string(),
                "fill e1 u".into(),
                "click e1".into(),
                "press Enter".into(),
            ]
        );
    }

    #[test]
    fn error_display_is_actionable() {
        assert_eq!(
            BrowserError::RefNotFound("e1".into()).to_string(),
            "element ref not found: e1"
        );
        assert_eq!(
            BrowserError::Eval("syntax".into()).to_string(),
            "eval failed: syntax"
        );
    }
}
