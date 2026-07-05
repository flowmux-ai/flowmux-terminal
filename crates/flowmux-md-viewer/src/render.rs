// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt;
use std::path::{Path, PathBuf};

use comrak::{markdown_to_html, Options};

const MIN_WIDTH: u32 = 320;
const MAX_WIDTH: u32 = 4096;
const DEFAULT_WIDTH: u32 = 900;
const DEFAULT_ZOOM: f32 = 1.0;
const DEFAULT_FONT: &str = "system-ui, -apple-system, BlinkMacSystemFont, \"Segoe UI\", sans-serif";

#[derive(Clone, Debug)]
pub struct RenderOptions {
    pub width: u32,
    pub zoom: f32,
    pub font_family: Option<String>,
    pub base_dir: Option<PathBuf>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            zoom: DEFAULT_ZOOM,
            font_family: None,
            base_dir: None,
        }
    }
}

impl RenderOptions {
    pub fn normalized_width(&self) -> u32 {
        self.width.clamp(MIN_WIDTH, MAX_WIDTH)
    }

    pub fn normalized_zoom(&self) -> f32 {
        if self.zoom.is_finite() {
            self.zoom.clamp(0.25, 4.0)
        } else {
            DEFAULT_ZOOM
        }
    }

    pub fn font_family(&self) -> &str {
        self.font_family.as_deref().unwrap_or(DEFAULT_FONT)
    }
}

#[derive(Clone, Debug)]
pub struct HtmlDocument {
    pub html: String,
    pub base_dir: Option<PathBuf>,
}

impl HtmlDocument {
    pub fn body_contains(&self, needle: &str) -> bool {
        self.html.contains(needle)
    }
}

#[derive(Debug)]
pub enum RenderError {
    Io(String),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RenderError {}

pub fn render_markdown_file(
    path: &Path,
    options: &RenderOptions,
) -> Result<HtmlDocument, RenderError> {
    let markdown = std::fs::read_to_string(path)
        .map_err(|err| RenderError::Io(format!("read {}: {err}", path.display())))?;
    let mut options = options.clone();
    if options.base_dir.is_none() {
        options.base_dir = path
            .canonicalize()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .or_else(|| path.parent().map(Path::to_path_buf));
    }
    render_markdown(&markdown, &options)
}

pub fn render_markdown(
    markdown: &str,
    options: &RenderOptions,
) -> Result<HtmlDocument, RenderError> {
    let body = render_markdown_body(markdown);
    Ok(HtmlDocument {
        html: wrap_html_document(&body, options),
        base_dir: options.base_dir.clone(),
    })
}

pub fn render_markdown_body(markdown: &str) -> String {
    markdown_to_html(markdown, &comrak_options())
}

fn comrak_options<'a>() -> Options<'a> {
    let mut options = Options::default();

    options.extension.strikethrough = true;
    options.extension.tagfilter = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;

    options.extension.footnotes = true;
    options.extension.inline_footnotes = true;
    options.extension.description_lists = true;
    options.extension.front_matter_delimiter = Some("---".to_string());
    options.extension.multiline_block_quotes = true;
    options.extension.alerts = true;
    options.extension.math_dollars = true;
    options.extension.math_code = true;
    options.extension.shortcodes = true;
    options.extension.superscript = true;
    options.extension.subscript = true;
    options.extension.spoiler = true;
    options.extension.underline = true;
    options.extension.header_id_prefix = Some(String::new());
    options.extension.header_id_prefix_in_href = true;

    options.render.r#unsafe = true;
    options.render.github_pre_lang = false;
    options.render.full_info_string = true;
    options.render.tasklist_classes = true;
    options
}

fn wrap_html_document(body: &str, options: &RenderOptions) -> String {
    let max_width = options.normalized_width().clamp(320, 1012);
    let font_family = css_string(options.font_family());
    format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="color-scheme" content="light">
<style>
:root {{
  --bg: #ffffff;
  --fg: #202428;
  --muted: #66707a;
  --border: #d8dee4;
  --code-bg: #f4f6f8;
  --link: #0969da;
}}
html {{
  background: var(--bg);
  color: var(--fg);
  color-scheme: light;
  font-family: {font_family};
  line-height: 1.5;
  text-size-adjust: 100%;
}}
body {{
  margin: 0;
  background: var(--bg);
}}
.markdown-body {{
  box-sizing: border-box;
  max-width: {max_width}px;
  margin: 0 auto;
  padding: 32px 40px;
  font-size: 16px;
}}
h1, h2, h3, h4, h5, h6 {{
  line-height: 1.25;
  margin: 1.4em 0 0.65em;
  font-weight: 650;
}}
h1 {{ font-size: 2em; }}
h2 {{ font-size: 1.5em; }}
h3 {{ font-size: 1.25em; }}
h4 {{ font-size: 1em; }}
h5 {{ font-size: 0.875em; }}
h6 {{
  color: var(--muted);
  font-size: 0.85em;
}}
h1:first-child {{
  margin-top: 0;
}}
h1, h2 {{
  padding-bottom: 0.25em;
  border-bottom: 1px solid var(--border);
}}
p, blockquote, ul, ol, dl, table, pre {{
  margin-top: 0;
  margin-bottom: 1em;
}}
a {{
  color: var(--link);
  text-decoration: none;
}}
a:hover {{
  text-decoration: underline;
}}
blockquote {{
  color: var(--muted);
  border-left: 0.25em solid var(--border);
  padding: 0 1em;
}}
code, kbd, samp {{
  font-family: "DejaVu Sans Mono", "Noto Sans Mono", ui-monospace, monospace;
  font-size: 0.92em;
}}
code {{
  background: var(--code-bg);
  border-radius: 4px;
  padding: 0.12em 0.32em;
}}
pre {{
  overflow: auto;
  padding: 16px;
  background: var(--code-bg);
  border-radius: 6px;
}}
pre code {{
  display: block;
  padding: 0;
  background: transparent;
  white-space: pre;
}}
img {{
  border-style: none;
}}
img, video {{
  max-width: 100%;
  height: auto;
  box-sizing: content-box;
  vertical-align: middle;
}}
video {{
  display: block;
  margin: 1em 0;
}}
table {{
  border-collapse: collapse;
  width: max-content;
  max-width: 100%;
  overflow: auto;
  display: block;
}}
th, td {{
  border: 1px solid var(--border);
  padding: 6px 13px;
}}
tr:nth-child(2n) {{
  background: color-mix(in srgb, var(--code-bg) 60%, transparent);
}}
hr {{
  border: 0;
  border-top: 1px solid var(--border);
  margin: 24px 0;
}}
input[type="checkbox"] {{
  margin-right: 0.35em;
}}
.footnotes {{
  color: var(--muted);
  font-size: 0.9em;
  border-top: 1px solid var(--border);
  margin-top: 2em;
}}
</style>
</head>
<body>
<main class="markdown-body">
{body}
</main>
</body>
</html>
"#
    )
}

fn css_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' | '\r' => escaped.push(' '),
            _ => escaped.push(ch),
        }
    }
    escaped.push('"');
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_gfm_extensions_to_html() {
        let html = render_markdown(
            "| A | B |\n|---|---|\n| 1 | 2 |\n\n- [x] done\n\n~~gone~~ www.example.com\n",
            &RenderOptions::default(),
        )
        .expect("render markdown")
        .html;

        assert!(html.contains("<table>"));
        assert!(html.contains(r#"<input type="checkbox""#));
        assert!(html.contains("<del>gone</del>"));
        assert!(html.contains(r#"<a href="http://www.example.com">"#));
    }

    #[test]
    fn wraps_html_as_full_document() {
        let html = render_markdown("# Title\n\nBody", &RenderOptions::default())
            .expect("render markdown")
            .html;

        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains(r#"<main class="markdown-body">"#));
        assert!(html.contains("<h1>"));
    }

    #[test]
    fn file_render_sets_base_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("doc.md");
        std::fs::write(&path, "![alt](image.png)").expect("write markdown");

        let document = render_markdown_file(&path, &RenderOptions::default()).expect("render file");

        let expected = path
            .canonicalize()
            .expect("canonical path")
            .parent()
            .map(Path::to_path_buf);
        assert_eq!(document.base_dir, expected);
        assert!(document
            .html
            .contains(r#"<img src="image.png" alt="alt" />"#));
    }
}
