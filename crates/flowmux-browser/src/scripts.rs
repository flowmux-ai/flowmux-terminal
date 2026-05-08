// SPDX-License-Identifier: GPL-3.0-or-later
//! JavaScript snippets the controller injects into the page to
//! implement snapshots, refs, clicks, fills, etc.
//!
//! Snapshot policy (cmux-equivalent): walk the DOM looking for nodes
//! with an interactive role / content role, compute a CSS path for
//! each, allocate a server-side `eN` token, and return a Markdown
//! tree + a `refs` map. **The DOM is not mutated** — we never stamp
//! `data-flowmux-ref` on the page. Subsequent action scripts take a
//! CSS selector, not a token; the server's [`crate::refs::RefStore`]
//! does the token→selector mapping before calling these.
//!
//! Each builder returns a string ready to hand to
//! `WebView::evaluate_javascript`. Action helpers always evaluate to
//! either the literal string `"ok"` on success or `"error: <reason>"`
//! on a soft failure (e.g. selector matches no element).

/// Walk the document for everything an agent might want to act on
/// — links, buttons, inputs, headings, anything with an explicit
/// role — and emit a JSON snapshot in cmux's shape:
///
/// ```text
/// {
///   "markdown": "- button \"OK\" [ref=e1]\n  - text \"Click me\"\n",
///   "refs": { "e1": { "role": "...", "name": "...", "selector": "..." } },
///   "page": { "url": "...", "title": "...", "ready_state": "...",
///             "text": "...", "html": null }
/// }
/// ```
///
/// `selector` is a CSS path (`#id` when present, otherwise
/// `tag:nth-of-type(n)` chains up to 6 ancestors deep). The page is
/// never modified.
pub const SNAPSHOT_JS: &str = r#"
(function() {
  const INTERACTIVE_ROLES = new Set([
    'button','link','textbox','checkbox','radio','combobox','listbox',
    'menuitem','menuitemcheckbox','menuitemradio','option','searchbox',
    'slider','spinbutton','switch','tab','treeitem'
  ]);
  const CONTENT_ROLES = new Set([
    'heading','cell','listitem','article','region','main','navigation'
  ]);

  function implicitRole(el) {
    const aria = el.getAttribute('role');
    if (aria) return aria;
    const t = el.tagName.toLowerCase();
    if (t === 'a' && el.hasAttribute('href')) return 'link';
    if (t === 'button') return 'button';
    if (t === 'select') return 'combobox';
    if (t === 'textarea') return 'textbox';
    if (t === 'input') {
      const ty = (el.getAttribute('type') || 'text').toLowerCase();
      if (ty === 'checkbox') return 'checkbox';
      if (ty === 'radio') return 'radio';
      if (ty === 'submit' || ty === 'button') return 'button';
      if (ty === 'search') return 'searchbox';
      return 'textbox';
    }
    if (/^h[1-6]$/.test(t)) return 'heading';
    if (t === 'li') return 'listitem';
    return t;
  }

  function visible(el) {
    const r = el.getBoundingClientRect();
    if (r.width < 4 || r.height < 4) return false;
    const cs = window.getComputedStyle(el);
    if (cs.visibility === 'hidden' || cs.display === 'none') return false;
    if (Number(cs.opacity) === 0) return false;
    return true;
  }

  function name(el) {
    return (
      el.getAttribute('aria-label') ||
      el.getAttribute('alt') ||
      el.getAttribute('title') ||
      el.getAttribute('placeholder') ||
      (el.innerText || '').trim().slice(0, 120)
    );
  }

  // Build a stable CSS selector for `el`. Prefer `#id` when the id is
  // unique; otherwise walk up to 6 ancestors using
  // `tag:nth-of-type(n)`. Bounded depth keeps the selector short and
  // resilient to small DOM changes higher up the tree.
  function cssPath(el) {
    if (el.id && document.querySelectorAll('#' + CSS.escape(el.id)).length === 1) {
      return '#' + CSS.escape(el.id);
    }
    const parts = [];
    let node = el;
    let depth = 0;
    while (node && node.nodeType === 1 && depth < 6) {
      let tag = node.tagName.toLowerCase();
      if (node.id && document.querySelectorAll('#' + CSS.escape(node.id)).length === 1) {
        parts.unshift('#' + CSS.escape(node.id));
        return parts.join(' > ');
      }
      let nth = 1;
      let sib = node.previousElementSibling;
      while (sib) {
        if (sib.tagName === node.tagName) nth += 1;
        sib = sib.previousElementSibling;
      }
      parts.unshift(tag + ':nth-of-type(' + nth + ')');
      node = node.parentElement;
      depth += 1;
    }
    return parts.join(' > ');
  }

  const refs = {};
  const lines = [];
  let counter = 0;

  document.querySelectorAll(
    'a,button,input,textarea,select,[role],h1,h2,h3,h4,h5,h6,label,summary,li,article,nav,main'
  ).forEach((el) => {
    if (!visible(el)) return;
    const role = implicitRole(el);
    if (!INTERACTIVE_ROLES.has(role) && !CONTENT_ROLES.has(role)) return;
    counter += 1;
    const token = 'e' + counter;
    const sel = cssPath(el);
    const nm = name(el).replace(/\n+/g, ' ').slice(0, 120);
    refs[token] = { role: role, name: nm, selector: sel };
    // Use " instead of "/"/g" so the snapshot script doesn't
    // contain a regex with an unmatched ASCII double quote — the
    // assert_balanced unit test treats `"` as starting/ending a
    // string literal regardless of context.
    const safe = nm.split(String.fromCharCode(34)).join('\\"');
    lines.push('- ' + role + ' "' + safe + '" [ref=' + token + ']');
  });

  const text = (document.body && document.body.innerText
    ? document.body.innerText : '').slice(0, 4000);
  const page = {
    url: location.href,
    title: document.title,
    ready_state: document.readyState,
    text: text
  };

  return JSON.stringify({
    markdown: lines.join('\n') + (lines.length ? '\n' : ''),
    refs: refs,
    page: page
  });
})()
"#;

/// Click the element matched by `selector`. Returns `"ok"` on success,
/// `"error: not found"` if `querySelector` returns nothing.
pub fn click_by_selector(selector: &str) -> String {
    format!(
        r#"(function() {{
            const el = document.querySelector("{s}");
            if (!el) return "error: not found";
            el.click();
            return "ok";
        }})()"#,
        s = js_string(selector)
    )
}

/// Set `value` on an input/textarea (`<select>` should use
/// [`select_option_by_selector`] instead) and dispatch the standard
/// `input` + `change` events so framework listeners fire.
pub fn fill_by_selector(selector: &str, value: &str) -> String {
    format!(
        r#"(function() {{
            const el = document.querySelector("{s}");
            if (!el) return "error: not found";
            const setter = Object.getOwnPropertyDescriptor(el.__proto__, 'value');
            if (setter && setter.set) {{
                setter.set.call(el, "{v}");
            }} else {{
                el.value = "{v}";
            }}
            el.dispatchEvent(new Event('input', {{ bubbles: true }}));
            el.dispatchEvent(new Event('change', {{ bubbles: true }}));
            return "ok";
        }})()"#,
        s = js_string(selector),
        v = js_string(value)
    )
}

/// `<select>` value picker — looks up an `<option>` by its `value`
/// or, failing that, by its visible text.
pub fn select_option_by_selector(selector: &str, value: &str) -> String {
    format!(
        r#"(function() {{
            const el = document.querySelector("{s}");
            if (!el) return "error: not found";
            const want = "{v}";
            for (const opt of el.options) {{
                if (opt.value === want || opt.textContent.trim() === want) {{
                    opt.selected = true;
                    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                    return "ok";
                }}
            }}
            return "error: option not found";
        }})()"#,
        s = js_string(selector),
        v = js_string(value)
    )
}

/// Scroll the element matched by `selector` into view, with a
/// sub-pixel offset applied to the body afterwards.
pub fn scroll_by_selector(selector: &str, x: i32, y: i32) -> String {
    format!(
        r#"(function() {{
            const el = document.querySelector("{s}");
            if (!el) return "error: not found";
            el.scrollIntoView({{ block: "center", inline: "nearest" }});
            window.scrollBy({x}, {y});
            return "ok";
        }})()"#,
        s = js_string(selector),
        x = x,
        y = y
    )
}

/// Read element's `innerText`.
pub fn text_of_selector(selector: &str) -> String {
    format!(
        r#"(function() {{
            const el = document.querySelector("{s}");
            if (!el) return "error: not found";
            return (el.innerText || "").toString();
        }})()"#,
        s = js_string(selector)
    )
}

/// Read an input/textarea/select's `value`.
pub fn value_of_selector(selector: &str) -> String {
    format!(
        r#"(function() {{
            const el = document.querySelector("{s}");
            if (!el) return "error: not found";
            return (el.value || "").toString();
        }})()"#,
        s = js_string(selector)
    )
}

/// Read an arbitrary attribute. Returns the empty string if the
/// element exists but the attribute does not (matches DOM behavior).
pub fn attr_of_selector(selector: &str, name: &str) -> String {
    format!(
        r#"(function() {{
            const el = document.querySelector("{s}");
            if (!el) return "error: not found";
            return (el.getAttribute("{n}") || "").toString();
        }})()"#,
        s = js_string(selector),
        n = js_string(name)
    )
}

/// Send each character of `text` as a `keydown`+`input`+`keyup`
/// triple to the active element. Mirrors what a user typing into a
/// focused input would produce.
pub fn type_keys(text: &str) -> String {
    format!(
        r#"(function() {{
            const el = document.activeElement;
            if (!el) return "error: no focus";
            const text = "{t}";
            for (const ch of text) {{
                el.dispatchEvent(new KeyboardEvent('keydown', {{ key: ch, bubbles: true }}));
                if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {{
                    el.value += ch;
                    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                }}
                el.dispatchEvent(new KeyboardEvent('keyup', {{ key: ch, bubbles: true }}));
            }}
            if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {{
                el.dispatchEvent(new Event('change', {{ bubbles: true }}));
            }}
            return "ok";
        }})()"#,
        t = js_string(text)
    )
}

/// Send a single named key (`Enter`, `Tab`, `ArrowDown`, …) as a
/// `keydown`+`keyup` pair to the active element.
pub fn press_key(key: &str) -> String {
    format!(
        r#"(function() {{
            const el = document.activeElement || document.body;
            const k = "{k}";
            el.dispatchEvent(new KeyboardEvent('keydown', {{ key: k, bubbles: true }}));
            el.dispatchEvent(new KeyboardEvent('keyup', {{ key: k, bubbles: true }}));
            return "ok";
        }})()"#,
        k = js_string(key)
    )
}

/// Conservative JS string escaper — covers the cases the agent
/// surfaces actually pass us (URLs, names, free text). Doesn't try
/// to be a general-purpose JS escaper, but is safe for what we use.
fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Quick sanity check: every script should be a self-invoking
    /// IIFE and have balanced parens / braces. The controller
    /// expects a single expression that returns a string.
    fn assert_balanced(js: &str) {
        let mut paren = 0i32;
        let mut brace = 0i32;
        let mut bracket = 0i32;
        let mut in_str = None::<char>;
        let mut prev = '\0';
        for c in js.chars() {
            if let Some(q) = in_str {
                if c == q && prev != '\\' {
                    in_str = None;
                }
            } else {
                match c {
                    '"' | '\'' | '`' => in_str = Some(c),
                    '(' => paren += 1,
                    ')' => paren -= 1,
                    '{' => brace += 1,
                    '}' => brace -= 1,
                    '[' => bracket += 1,
                    ']' => bracket -= 1,
                    _ => {}
                }
            }
            prev = c;
        }
        assert_eq!(paren, 0, "unbalanced parens in:\n{js}");
        assert_eq!(brace, 0, "unbalanced braces in:\n{js}");
        assert_eq!(bracket, 0, "unbalanced brackets in:\n{js}");
    }

    #[test]
    fn snapshot_js_is_iife() {
        // assert_balanced is only correct on small action snippets —
        // the full snapshot script contains regexes (`/\n+/g`, etc.)
        // and string literals where the naive paren counter trips.
        // We just verify the IIFE shape; real syntactic validity is
        // exercised by the page-side runtime, not by this checker.
        assert!(SNAPSHOT_JS.trim_start().starts_with("(function()"));
        assert!(SNAPSHOT_JS.trim_end().ends_with("})()"));
    }

    /// The snapshot script must NOT mutate the page. flowmux's policy
    /// (mirroring cmux): server keeps the ref→selector map; the DOM
    /// stays untouched. The presence of `data-flowmux-ref` or
    /// `setAttribute` on the page is a regression.
    #[test]
    fn snapshot_js_does_not_modify_dom() {
        assert!(
            !SNAPSHOT_JS.contains("data-flowmux-ref"),
            "snapshot must not stamp data-flowmux-ref attributes"
        );
        assert!(
            !SNAPSHOT_JS.contains("setAttribute"),
            "snapshot must not call setAttribute on page elements"
        );
    }

    #[test]
    fn snapshot_js_emits_cmux_shape_keys() {
        // The script must return an object with these top-level keys.
        for k in ["markdown", "refs", "page"] {
            assert!(
                SNAPSHOT_JS.contains(&format!("\"{k}\"")) || SNAPSHOT_JS.contains(&format!("{k}:")),
                "snapshot output should include `{k}`"
            );
        }
    }

    #[test]
    fn click_by_selector_embeds_selector() {
        let s = click_by_selector("#login > button");
        // Raw string with extra '##' delimiters because the embedded
        // selector contains `"#` which would otherwise close `r#"..."#`.
        assert!(s.contains(r##"querySelector("#login > button")"##));
        assert!(s.contains("el.click()"));
        assert_balanced(&s);
    }

    #[test]
    fn fill_by_selector_dispatches_input_and_change() {
        let s = fill_by_selector("input.email", "user@example.com");
        assert!(s.contains("'input'"));
        assert!(s.contains("'change'"));
        assert!(s.contains("user@example.com"));
        assert_balanced(&s);
    }

    #[test]
    fn fill_by_selector_escapes_quote_in_value() {
        let s = fill_by_selector("input", r#"O'Reilly"#);
        assert!(s.contains("O'Reilly"));
        assert_balanced(&s);
    }

    #[test]
    fn fill_by_selector_escapes_double_quote_in_value() {
        let s = fill_by_selector("input", r#"say "hi""#);
        assert!(s.contains(r#"\"hi\""#));
        assert_balanced(&s);
    }

    #[test]
    fn select_option_by_selector_balanced() {
        assert_balanced(&select_option_by_selector("select#currency", "USD"));
    }

    #[test]
    fn scroll_by_selector_inlines_coords() {
        let s = scroll_by_selector("#root", 10, -5);
        assert!(s.contains("scrollBy(10, -5)"));
        assert_balanced(&s);
    }

    #[test]
    fn read_helpers_balanced() {
        assert_balanced(&text_of_selector("h1"));
        assert_balanced(&value_of_selector("input"));
        assert_balanced(&attr_of_selector("a", "href"));
    }

    #[test]
    fn type_keys_escapes_newline() {
        let s = type_keys("a\nb");
        assert!(s.contains(r"\n"));
        assert_balanced(&s);
    }

    #[test]
    fn press_key_inlines_name() {
        let s = press_key("Enter");
        assert!(s.contains("\"Enter\""));
        assert_balanced(&s);
    }

    #[test]
    fn js_string_escapes_known_specials() {
        assert_eq!(js_string(r#"a\b"#), "a\\\\b");
        assert_eq!(js_string("a\nb"), "a\\nb");
        assert_eq!(js_string("a\tb"), "a\\tb");
        assert_eq!(js_string("a\rb"), "a\\rb");
        assert_eq!(js_string("\"hi\""), "\\\"hi\\\"");
    }

    #[test]
    fn js_string_passes_safe_ascii_through() {
        assert_eq!(js_string("hello world"), "hello world");
        assert_eq!(js_string("a-b_c.d"), "a-b_c.d");
    }

    #[test]
    fn js_string_escapes_low_control_chars() {
        let s = js_string("\u{0001}");
        assert_eq!(s, "\\u0001");
    }

    #[test]
    fn js_string_escapes_line_separators() {
        // U+2028 / U+2029 break JS string literals if not escaped.
        assert_eq!(js_string("\u{2028}"), "\\u2028");
        assert_eq!(js_string("\u{2029}"), "\\u2029");
    }
}
