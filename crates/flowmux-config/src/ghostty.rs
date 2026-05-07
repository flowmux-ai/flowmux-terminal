// SPDX-License-Identifier: GPL-3.0-or-later
//! Read-only loader for `~/.config/ghostty/config`.
//!
//! The Ghostty config file is documented as `key = value` lines with `#`
//! comments and `key = value, value` for lists. We only extract the
//! subset flowmux needs (font, theme, colors); unknown keys are kept as
//! raw strings in `extras` so the data round-trips for diagnostics.

use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct GhosttyConfig {
    pub font_family: Option<String>,
    pub font_size: Option<f32>,
    pub theme: Option<String>,
    pub background: Option<String>,
    pub foreground: Option<String>,
    pub cursor_color: Option<String>,
    pub selection_background: Option<String>,
    pub selection_foreground: Option<String>,
    /// 16-color ANSI palette. Index N is the color for `palette = N=#...`.
    pub palette: [Option<String>; 16],
    /// Anything we don't model explicitly. Useful for diagnostics.
    pub extras: BTreeMap<String, String>,
}

impl GhosttyConfig {
    /// Layer `other` on top of self — non-empty fields in `other` win.
    /// Used when applying the user's config over a resolved theme file.
    pub fn merge(&mut self, other: GhosttyConfig) {
        if other.font_family.is_some() {
            self.font_family = other.font_family;
        }
        if other.font_size.is_some() {
            self.font_size = other.font_size;
        }
        if other.theme.is_some() {
            self.theme = other.theme;
        }
        if other.background.is_some() {
            self.background = other.background;
        }
        if other.foreground.is_some() {
            self.foreground = other.foreground;
        }
        if other.cursor_color.is_some() {
            self.cursor_color = other.cursor_color;
        }
        if other.selection_background.is_some() {
            self.selection_background = other.selection_background;
        }
        if other.selection_foreground.is_some() {
            self.selection_foreground = other.selection_foreground;
        }
        for (i, v) in other.palette.into_iter().enumerate() {
            if v.is_some() {
                self.palette[i] = v;
            }
        }
        for (k, v) in other.extras {
            self.extras.insert(k, v);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub fn load(path: &Path) -> Result<GhosttyConfig, LoadError> {
    let text = std::fs::read_to_string(path)?;
    Ok(parse(&text))
}

/// Strip an inline trailing comment introduced by whitespace + `#`.
/// Hex colors (`#1e1e2e`) and similar values that start with `#` are
/// preserved because they are not preceded by whitespace within the
/// value substring.
fn strip_inline_comment(value: &str) -> &str {
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' && i > 0 && bytes[i - 1].is_ascii_whitespace() {
            return value[..i].trim_end();
        }
        i += 1;
    }
    value
}

pub fn parse(text: &str) -> GhosttyConfig {
    let mut cfg = GhosttyConfig::default();
    for raw in text.lines() {
        let line = raw.trim();
        // Ghostty's config format treats `#` at the start of a (trimmed)
        // line as a comment. We deliberately do NOT split on inline `#`
        // since values like hex colors (`#1e1e2e`) start with `#`.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        let raw_value = v.trim();
        let value = if key == "palette" {
            raw_value
        } else {
            strip_inline_comment(raw_value)
        };
        match key {
            "font-family" => cfg.font_family = Some(value.into()),
            "font-size" => cfg.font_size = value.parse().ok(),
            "theme" => cfg.theme = Some(value.into()),
            "background" => cfg.background = Some(value.into()),
            "foreground" => cfg.foreground = Some(value.into()),
            "cursor-color" => cfg.cursor_color = Some(value.into()),
            "selection-background" => cfg.selection_background = Some(value.into()),
            "selection-foreground" => cfg.selection_foreground = Some(value.into()),
            "palette" => {
                // Format: `palette = N=#RRGGBB` or `palette = N #RRGGBB`.
                let rest = value
                    .split_once('=')
                    .map(|(n, c)| (n.trim(), c.trim()))
                    .or_else(|| {
                        value
                            .split_once(char::is_whitespace)
                            .map(|(n, c)| (n.trim(), c.trim()))
                    });
                if let Some((idx, color)) = rest {
                    if let Ok(i) = idx.parse::<usize>() {
                        if i < 16 {
                            cfg.palette[i] = Some(strip_inline_comment(color).to_string());
                        }
                    }
                }
            }
            other => {
                cfg.extras.insert(other.into(), value.into());
            }
        }
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_palette_entries() {
        let raw = "\
            background = #0e1015\n\
            palette = 0=#1d2021\n\
            palette = 2 #b8bb26\n\
            palette = 7=#fbf1c7\n\
            palette = 15=#ffffff\n\
            palette = 16=#ignored\n\
            palette = nope=#ignored\n\
        ";
        let cfg = parse(raw);
        assert_eq!(cfg.palette[0].as_deref(), Some("#1d2021"));
        assert_eq!(cfg.palette[2].as_deref(), Some("#b8bb26"));
        assert_eq!(cfg.palette[7].as_deref(), Some("#fbf1c7"));
        assert_eq!(cfg.palette[15].as_deref(), Some("#ffffff"));
        assert!(cfg.palette[3].is_none());
    }

    #[test]
    fn merge_lets_user_overrides_win() {
        let mut base = parse("background = #000000\nfont-size = 11\n");
        base.merge(parse("background = #0e1015\n"));
        assert_eq!(base.background.as_deref(), Some("#0e1015"));
        assert_eq!(base.font_size, Some(11.0));
    }

    #[test]
    fn parses_selection_colors_and_preserves_unknowns() {
        let cfg = parse(
            "selection-background = #334455\n\
             selection-foreground = #ddeeff\n\
             window-padding-x = 8\n",
        );

        assert_eq!(cfg.selection_background.as_deref(), Some("#334455"));
        assert_eq!(cfg.selection_foreground.as_deref(), Some("#ddeeff"));
        assert_eq!(
            cfg.extras.get("window-padding-x").map(String::as_str),
            Some("8")
        );
    }

    #[test]
    fn parses_typical_ghostty_config() {
        let raw = "\
            # Comment line\n\
            font-family = JetBrains Mono\n\
            font-size = 13\n\
            theme = catppuccin-mocha\n\
            background = #1e1e2e   # inline comment\n\
            keybind = ctrl+s>r=reload_config\n\
        ";
        let cfg = parse(raw);
        assert_eq!(cfg.font_family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(cfg.font_size, Some(13.0));
        assert_eq!(cfg.theme.as_deref(), Some("catppuccin-mocha"));
        assert_eq!(cfg.background.as_deref(), Some("#1e1e2e"));
        assert!(cfg.extras.contains_key("keybind"));
    }
}
