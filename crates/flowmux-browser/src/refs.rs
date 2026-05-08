// SPDX-License-Identifier: GPL-3.0-or-later
//! Server-side ref token → CSS selector store.
//!
//! cmux's policy: the snapshot script computes a CSS path for every
//! interactable / labeled DOM node, allocates a token (`@e1`, `@e2`,
//! …), and the *server* (flowmux daemon, in our case) keeps the
//! `(surface_id, ref_token) → selector` map. Action calls (`click`,
//! `fill`, `text_of`, …) take a ref token, the store resolves it to
//! a CSS selector, and the action JS runs `document.querySelector(...)`.
//!
//! The DOM is never modified — no `data-flowmux-ref` attribute is
//! injected — so SPAs that mount MutationObservers / React strict
//! mode see no surprise mutations. (cmux's reasoning, mirrored here.)
//!
//! `RefStore` is *not* thread-safe by itself; callers wrap it in
//! `Rc<RefCell<…>>` (single-threaded GTK side) or `Arc<Mutex<…>>`
//! (tokio side) as needed.

use std::collections::HashMap;

/// Identifier the store uses to namespace refs by surface. Decoupled
/// from the public `flowmux_core::SurfaceId` so this crate stays
/// dependency-free of the domain layer (the GTK side wraps a
/// `SurfaceId` into this type when calling).
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct RefScope(pub u128);

impl RefScope {
    pub fn from_u128(v: u128) -> Self {
        Self(v)
    }
}

#[derive(Debug, Default)]
pub struct RefStore {
    /// `RefScope` (e.g. surface id encoded as u128) → its own
    /// `ref_token → selector` map. Each scope's counter is reset
    /// whenever a new snapshot is taken for that scope.
    scopes: HashMap<RefScope, ScopeState>,
}

#[derive(Debug, Default)]
struct ScopeState {
    /// `e1`, `e2`, … → CSS selector (cssPath).
    refs: HashMap<String, String>,
    next: u32,
}

impl RefStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every ref the given scope has accumulated. Callers invoke
    /// this at the start of each new snapshot so stale refs from a
    /// prior page don't shadow fresh ones.
    pub fn clear(&mut self, scope: RefScope) {
        self.scopes.remove(&scope);
    }

    /// Allocate a fresh `eN` token bound to `selector` for `scope`.
    /// Tokens are stable within a snapshot and unique within the
    /// scope. Returns the token (e.g. `"e3"`).
    pub fn allocate(&mut self, scope: RefScope, selector: impl Into<String>) -> String {
        let st = self.scopes.entry(scope).or_default();
        st.next += 1;
        let token = format!("e{}", st.next);
        st.refs.insert(token.clone(), selector.into());
        token
    }

    /// Insert an explicit `(token, selector)` pair (used when the
    /// snapshot JS already chose tokens client-side and we just need
    /// to mirror the map server-side).
    pub fn insert(
        &mut self,
        scope: RefScope,
        token: impl Into<String>,
        selector: impl Into<String>,
    ) {
        let st = self.scopes.entry(scope).or_default();
        st.refs.insert(token.into(), selector.into());
    }

    /// Look up a ref. Accepts both `e3` and the cmux-style `@e3`.
    /// Returns `None` if the token is not bound in this scope.
    pub fn resolve<'a>(&'a self, scope: RefScope, token: &str) -> Option<&'a str> {
        let st = self.scopes.get(&scope)?;
        let key = token.strip_prefix('@').unwrap_or(token);
        st.refs.get(key).map(String::as_str)
    }

    pub fn len(&self, scope: RefScope) -> usize {
        self.scopes.get(&scope).map(|s| s.refs.len()).unwrap_or(0)
    }

    pub fn is_empty(&self, scope: RefScope) -> bool {
        self.len(scope) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(n: u128) -> RefScope {
        RefScope::from_u128(n)
    }

    #[test]
    fn allocate_returns_sequential_tokens() {
        let mut store = RefStore::new();
        let s = scope(1);
        let t1 = store.allocate(s, "body > button:nth-of-type(1)");
        let t2 = store.allocate(s, "body > input:nth-of-type(1)");
        assert_eq!(t1, "e1");
        assert_eq!(t2, "e2");
        assert_eq!(store.len(s), 2);
    }

    #[test]
    fn resolve_returns_selector_for_known_ref() {
        let mut store = RefStore::new();
        let s = scope(1);
        store.allocate(s, "body > button:nth-of-type(1)");
        assert_eq!(
            store.resolve(s, "e1"),
            Some("body > button:nth-of-type(1)")
        );
    }

    #[test]
    fn resolve_accepts_at_prefix() {
        let mut store = RefStore::new();
        let s = scope(1);
        store.allocate(s, "body > button");
        // cmux CLI surfaces `@e1` to its agents — we accept both.
        assert_eq!(store.resolve(s, "@e1"), Some("body > button"));
        assert_eq!(store.resolve(s, "e1"), Some("body > button"));
    }

    #[test]
    fn resolve_returns_none_for_unknown_ref() {
        let mut store = RefStore::new();
        let s = scope(1);
        store.allocate(s, "body > button");
        assert_eq!(store.resolve(s, "e99"), None);
    }

    #[test]
    fn resolve_is_scoped_per_surface() {
        let mut store = RefStore::new();
        let s1 = scope(1);
        let s2 = scope(2);
        store.allocate(s1, "selector_one");
        store.allocate(s2, "selector_two");
        // Same token "e1" exists in both scopes but maps differently.
        assert_eq!(store.resolve(s1, "e1"), Some("selector_one"));
        assert_eq!(store.resolve(s2, "e1"), Some("selector_two"));
        // A token from one scope is not visible in the other:
        // here, both scopes happen to have e1 — we drop one to verify.
        store.clear(s1);
        assert_eq!(store.resolve(s1, "e1"), None);
        assert_eq!(store.resolve(s2, "e1"), Some("selector_two"));
    }

    #[test]
    fn clear_resets_scope_counter() {
        let mut store = RefStore::new();
        let s = scope(1);
        let t1 = store.allocate(s, "a");
        store.allocate(s, "b");
        store.clear(s);
        // After a clear, a fresh allocate restarts at e1 — important
        // so a new snapshot's ref tokens stay sequential and small.
        let new_t1 = store.allocate(s, "c");
        assert_eq!(t1, "e1");
        assert_eq!(new_t1, "e1");
        assert_eq!(store.len(s), 1);
    }

    #[test]
    fn insert_records_explicit_token_selector_pair() {
        let mut store = RefStore::new();
        let s = scope(1);
        store.insert(s, "e7", "main > section > a:nth-of-type(2)");
        assert_eq!(
            store.resolve(s, "e7"),
            Some("main > section > a:nth-of-type(2)")
        );
    }

    /// Scenario: the snapshot JS already picked tokens for every
    /// element (matching cmux's flow where the page-side script
    /// returns `refs: {e1: ..., e2: ...}` already labeled). Server
    /// mirrors them so subsequent action calls resolve the same
    /// tokens to selectors.
    #[test]
    fn scenario_snapshot_then_resolve_token_for_action() {
        let mut store = RefStore::new();
        let s = scope(0xabc);

        // Pretend the JS just returned this map.
        let snapshot_refs = [
            ("e1", "body > form > input:nth-of-type(1)"),
            ("e2", "body > form > input:nth-of-type(2)"),
            ("e3", "body > form > button:nth-of-type(1)"),
        ];

        store.clear(s);
        for (token, sel) in snapshot_refs {
            store.insert(s, token, sel);
        }

        // Agent's next call is `flowmux browser click e3`.
        let resolved = store.resolve(s, "e3");
        assert_eq!(resolved, Some("body > form > button:nth-of-type(1)"));
    }
}
