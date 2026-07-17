// SPDX-License-Identifier: GPL-3.0-or-later
//! Built-in theme presets shown in the options dialog's Theme tab.
//!
//! Each preset is a Ghostty-format theme file embedded at compile time
//! from `themes/`. The `default` preset is intentionally empty: resolving
//! it yields [`crate::ghostty::GhosttyConfig::default`], so the built-in
//! fallback colors (flowmux's current stock look) apply.

use crate::ghostty::GhosttyConfig;

pub struct ThemePreset {
    /// Stable identifier stored in `options.json`.
    pub id: &'static str,
    /// Human-readable name shown in the Theme tab.
    pub name: &'static str,
    /// Ghostty-format theme source. Empty for the default preset.
    pub source: &'static str,
}

pub const PRESETS: &[ThemePreset] = &[
    ThemePreset {
        id: "default",
        name: "Default",
        source: "",
    },
    ThemePreset {
        id: "one-dark",
        name: "One Dark",
        source: include_str!("../themes/one-dark.theme"),
    },
    ThemePreset {
        id: "dracula",
        name: "Dracula",
        source: include_str!("../themes/dracula.theme"),
    },
    ThemePreset {
        id: "nord",
        name: "Nord",
        source: include_str!("../themes/nord.theme"),
    },
    ThemePreset {
        id: "gruvbox-dark",
        name: "Gruvbox Dark",
        source: include_str!("../themes/gruvbox-dark.theme"),
    },
    ThemePreset {
        id: "catppuccin-mocha",
        name: "Catppuccin Mocha",
        source: include_str!("../themes/catppuccin-mocha.theme"),
    },
    ThemePreset {
        id: "tokyo-night",
        name: "Tokyo Night",
        source: include_str!("../themes/tokyo-night.theme"),
    },
    ThemePreset {
        id: "solarized-dark",
        name: "Solarized Dark",
        source: include_str!("../themes/solarized-dark.theme"),
    },
    ThemePreset {
        id: "solarized-light",
        name: "Solarized Light",
        source: include_str!("../themes/solarized-light.theme"),
    },
    ThemePreset {
        id: "github-light",
        name: "GitHub Light",
        source: include_str!("../themes/github-light.theme"),
    },
    ThemePreset {
        id: "catppuccin-latte",
        name: "Catppuccin Latte",
        source: include_str!("../themes/catppuccin-latte.theme"),
    },
];

pub fn find(id: &str) -> Option<&'static ThemePreset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

/// Parse the preset's theme source. `None` for unknown ids so callers can
/// fall back to the default look when `options.json` names a preset that
/// no longer exists.
pub fn config(id: &str) -> Option<GhosttyConfig> {
    find(id).map(|preset| crate::ghostty::parse(preset.source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_parses_with_complete_colors() {
        for preset in PRESETS {
            let cfg = crate::ghostty::parse(preset.source);
            if preset.id == "default" {
                assert!(cfg.background.is_none(), "default preset must stay empty");
                continue;
            }
            assert!(
                cfg.background.is_some(),
                "{}: missing background",
                preset.id
            );
            assert!(
                cfg.foreground.is_some(),
                "{}: missing foreground",
                preset.id
            );
            assert!(
                cfg.cursor_color.is_some(),
                "{}: missing cursor-color",
                preset.id
            );
            assert!(
                cfg.selection_background.is_some(),
                "{}: missing selection-background",
                preset.id
            );
            for (i, slot) in cfg.palette.iter().enumerate() {
                let color = slot.as_deref().unwrap_or_else(|| {
                    panic!("{}: missing palette slot {i}", preset.id);
                });
                assert!(
                    crate::options::is_valid_hex_color(color),
                    "{}: palette {i} is not a hex color: {color}",
                    preset.id
                );
            }
        }
    }

    #[test]
    fn preset_ids_are_unique_and_default_is_first() {
        let mut seen = std::collections::HashSet::new();
        for preset in PRESETS {
            assert!(seen.insert(preset.id), "duplicate preset id {}", preset.id);
        }
        assert_eq!(PRESETS[0].id, "default");
    }

    #[test]
    fn unknown_preset_id_resolves_to_none() {
        assert!(config("no-such-theme").is_none());
        assert!(config("dracula").is_some());
    }
}
