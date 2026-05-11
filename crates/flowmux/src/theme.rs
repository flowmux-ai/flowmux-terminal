// SPDX-License-Identifier: GPL-3.0-or-later
//! Visual theme.
//!
//! Resolution order:
//!
//! 1. flowmux's own theme config at `$XDG_CONFIG_HOME/flowmux/theme`,
//!    if the file exists.
//! 2. flowmux's built-in defaults, authored in this file. Background and
//!    foreground mirror Ghostty's shipped defaults verbatim (`#282c34` /
//!    `#ffffff`) so flowmux looks like Ghostty out of the box without
//!    reading any external config file at runtime.
//!
//! Applied to:
//!
//! * VTE terminal widgets — font, bg/fg/cursor, ANSI palette, selection.
//! * libadwaita color scheme — forced dark when the background is dark.
//! * Global CSS — pane frame and sidebar tint.
//!
//! Users who want a specific look should drop a theme file into
//! `~/.config/flowmux/theme` (see `resources/themes/example.theme` for
//! the format) or run `flowmux theme import <path>` to copy one from
//! anywhere on their machine.

use gtk::gdk;
use gtk::pango;
use vte::prelude::*;

pub struct ResolvedTheme {
    pub font: pango::FontDescription,
    pub bg: gdk::RGBA,
    pub fg: gdk::RGBA,
    pub cursor: gdk::RGBA,
    pub selection_bg: Option<gdk::RGBA>,
    pub selection_fg: Option<gdk::RGBA>,
    /// Always 16 entries. Indices missing from the user's theme file
    /// fall back to Ghostty's default ANSI palette (Tomorrow), so a
    /// fresh install renders prompts / ls output with Ghostty's
    /// shipped colors instead of VTE's built-in grays.
    pub palette: Vec<gdk::RGBA>,
}

/// Ghostty's default ANSI 16-color palette, copied verbatim from
/// ghostty's `src/terminal/color.zig` `Name.default()` (the Tomorrow
/// scheme). flowmux inlines this so a fresh install matches Ghostty
/// without reading any external config.
const DEFAULT_PALETTE: [&str; 16] = [
    "#1d1f21", "#cc6666", "#b5bd68", "#f0c674", "#81a2be", "#b294bb", "#8abeb7", "#c5c8c6",
    "#666666", "#d54e53", "#b9ca4a", "#e7c547", "#7aa6da", "#c397d8", "#70c0b1", "#eaeaea",
];

impl ResolvedTheme {
    pub fn load() -> Self {
        let cfg = flowmux_config::theme::load().unwrap_or_default();
        let theme = Self::from_ghostty(&cfg);
        let path = flowmux_config::theme::user_theme_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let user_palette_count = cfg.palette.iter().filter(|p| p.is_some()).count();
        tracing::info!(
            source = if cfg.background.is_some() { "user theme file" } else { "builtin defaults" },
            path = %path,
            bg = ?cfg.background.as_deref(),
            fg = ?cfg.foreground.as_deref(),
            font_family = ?cfg.font_family.as_deref(),
            font_size = ?cfg.font_size,
            user_palette_count,
            "resolved theme"
        );
        theme
    }

    fn from_ghostty(cfg: &flowmux_config::ghostty::GhosttyConfig) -> Self {
        // Built-in fallbacks kick in only when the user's flowmux theme
        // file does not supply a value. `bg`/`fg` mirror Ghostty's shipped
        // defaults verbatim; `cursor` follows `fg` because Ghostty leaves
        // cursor-color unset, which on Ghostty's side resolves to fg too.
        let bg = cfg
            .background
            .as_deref()
            .and_then(parse)
            .unwrap_or_else(|| parse_or_black("#282c34"));
        let fg = cfg
            .foreground
            .as_deref()
            .and_then(parse)
            .unwrap_or_else(|| parse_or_black("#ffffff"));
        let cursor = cfg.cursor_color.as_deref().and_then(parse).unwrap_or(fg);
        let selection_bg = cfg.selection_background.as_deref().and_then(parse);
        let selection_fg = cfg.selection_foreground.as_deref().and_then(parse);

        let palette: Vec<gdk::RGBA> = (0..16)
            .map(|i| {
                cfg.palette[i]
                    .as_deref()
                    .and_then(parse)
                    .unwrap_or_else(|| parse_or_black(DEFAULT_PALETTE[i]))
            })
            .collect();

        let family = cfg.font_family.as_deref().unwrap_or("monospace");
        let size = cfg.font_size.unwrap_or(12.0);
        let font = pango::FontDescription::from_string(&format!("{family} {size}"));

        Self {
            font,
            bg,
            fg,
            cursor,
            selection_bg,
            selection_fg,
            palette,
        }
    }

    pub fn apply_to_vte(&self, term: &vte::Terminal) {
        term.set_font(Some(&self.font));
        let refs: Vec<&gdk::RGBA> = self.palette.iter().collect();
        term.set_colors(Some(&self.fg), Some(&self.bg), &refs);
        term.set_color_cursor(Some(&self.cursor));
        if let Some(sbg) = &self.selection_bg {
            term.set_color_highlight(Some(sbg));
        }
        if let Some(sfg) = &self.selection_fg {
            term.set_color_highlight_foreground(Some(sfg));
        }
        // Soften the look — block-blink cursor, no audible bell, generous
        // scrollback.
        term.set_cursor_blink_mode(vte::CursorBlinkMode::On);
        term.set_cursor_shape(vte::CursorShape::Block);
        term.set_audible_bell(false);
        term.set_scrollback_lines(20_000);
    }

    pub fn is_dark(&self) -> bool {
        relative_luminance(&self.bg) < 0.5
    }

    /// CSS rules that paint the pane frame and tint the sidebar to match
    /// the terminal background. `focus_border_color` is the hex color chosen
    /// in options, and `focus_border_alpha` is the 0.0..=1.0 opacity from
    /// the same options. The focused pane's 1px border is rendered as
    /// `rgba(r,g,b,alpha)` so slider changes apply immediately.
    pub fn css(&self, focus_border_color: &str, focus_border_alpha: f32) -> String {
        let bg_css = rgba_css(&self.bg);
        let focus_css = focus_border_rgba_css(focus_border_color, focus_border_alpha);
        let pane_border_css = rgba_css(&blend_with_alpha(&self.fg, 0.10));
        let tabbar_bg_css = rgba_css(&shift_lightness(
            &self.bg,
            if self.is_dark() { 0.025 } else { -0.025 },
        ));
        let tab_active_bg_css = rgba_css(&shift_lightness(
            &self.bg,
            if self.is_dark() { 0.055 } else { -0.055 },
        ));
        let control_hover_css = rgba_css(&blend_with_alpha(&self.fg, 0.09));
        let subdued_fg_css = rgba_css(&blend_with_alpha(&self.fg, 0.72));
        let sidebar_bg = rgba_css(&shift_lightness(&self.bg, -0.04));
        let toast_bg_css = rgba_css(&blend_with_alpha(
            &shift_lightness(&self.bg, if self.is_dark() { 0.12 } else { -0.12 }),
            0.94,
        ));
        let toast_border_css = rgba_css(&blend_with_alpha(&self.fg, 0.18));
        format!(
            r#"
.flowmux-pane {{
    background-color: {bg};
    border: 1px solid {border};
    border-radius: 4px;
    margin: 1px;
    padding: 0;
}}
.flowmux-pane.focused {{
    border-color: {focus};
    box-shadow: inset 0 0 0 1px {focus};
}}
.flowmux-pane vte-terminal {{
    padding: 7px;
    border-radius: 0 0 3px 3px;
}}
.flowmux-pane-tabbar {{
    min-height: 26px;
    background-color: {tabbar};
    border-bottom: 1px solid {border};
    padding: 0 2px;
}}
.flowmux-pane-tabs {{
    margin: 0;
}}
.flowmux-pane-tab {{
    margin: 2px 1px 0 0;
    border: 1px solid transparent;
    border-bottom: 0;
    border-radius: 4px 4px 0 0;
}}
.flowmux-pane-tab.active {{
    background-color: {tab_active};
    border-color: {border};
}}
.flowmux-pane-tab-main {{
    min-height: 22px;
    padding: 0 7px;
    border-radius: 3px 0 0 0;
    color: {subdued_fg};
}}
.flowmux-pane-tab.active .flowmux-pane-tab-main {{
    color: {fg};
}}
.flowmux-pane-tab-close {{
    min-height: 22px;
    min-width: 20px;
    padding: 0 4px;
    border-radius: 0 3px 0 0;
    opacity: 0.66;
}}
.flowmux-pane-tab-close:hover,
.flowmux-pane-tool:hover {{
    background-color: {control_hover};
    opacity: 1.0;
}}
.flowmux-pane-tools {{
    margin: 0 2px 0 4px;
}}
.flowmux-pane-tool {{
    min-height: 22px;
    min-width: 22px;
    margin-top: 2px;
    padding: 0;
    border-radius: 4px;
    opacity: 0.72;
}}
paned > separator {{
    background-color: {border};
    min-width: 1px;
    min-height: 1px;
}}
.navigation-sidebar {{
    background-color: {sidebar};
}}
.navigation-sidebar row {{
    color: {fg};
    border-radius: 6px;
    margin: 2px 6px;
    padding: 8px 10px;
}}
/* libadwaita wraps the workspace title in .heading and the path
   subtitles in .caption/.dim-label, both of which assign their own
   color in the dark theme. Re-pin the color on the labels so the
   sidebar folder names follow Ghostty's foreground. The .dim-label
   variants keep their natural dimming because that class adjusts
   opacity, not color. */
.navigation-sidebar row label,
.navigation-sidebar row label.heading,
.navigation-sidebar row label.caption {{
    color: {fg};
}}
.navigation-sidebar row.flowmux-attention {{
    background-color: rgba(245, 158, 11, 0.18);
    box-shadow: inset 3px 0 0 rgba(245, 158, 11, 0.85);
}}
.navigation-sidebar row.flowmux-dragging {{
    opacity: 0.4;
}}
.navigation-sidebar row.flowmux-drop-above {{
    box-shadow: inset 0 2px 0 rgba(96, 165, 250, 0.95);
}}
.navigation-sidebar row.flowmux-drop-below {{
    box-shadow: inset 0 -2px 0 rgba(96, 165, 250, 0.95);
}}
.flowmux-pane-tab.flowmux-pane-tab-dragging {{
    opacity: 0.4;
}}
.flowmux-pane-tab.flowmux-pane-tab-drop-before {{
    box-shadow: inset 2px 0 0 rgba(96, 165, 250, 0.95);
}}
.flowmux-pane-tab.flowmux-pane-tab-drop-after {{
    box-shadow: inset -2px 0 0 rgba(96, 165, 250, 0.95);
}}
.flowmux-clipboard-toast {{
    background-color: {toast_bg};
    color: {fg};
    border: 1px solid {toast_border};
    border-radius: 8px;
    box-shadow: 0 6px 18px rgba(0, 0, 0, 0.28);
    padding: 8px 14px;
}}
"#,
            bg = bg_css,
            fg = rgba_css(&self.fg),
            border = pane_border_css,
            focus = focus_css,
            tabbar = tabbar_bg_css,
            tab_active = tab_active_bg_css,
            control_hover = control_hover_css,
            subdued_fg = subdued_fg_css,
            sidebar = sidebar_bg,
            toast_bg = toast_bg_css,
            toast_border = toast_border_css,
        )
    }
}

fn parse(s: &str) -> Option<gdk::RGBA> {
    gdk::RGBA::parse(s).ok()
}

fn parse_or_black(default: &str) -> gdk::RGBA {
    parse(default).unwrap_or_else(|| gdk::RGBA::new(0.0, 0.0, 0.0, 1.0))
}

fn rgba_css(c: &gdk::RGBA) -> String {
    format!(
        "rgba({},{},{},{})",
        (c.red() * 255.0) as u8,
        (c.green() * 255.0) as u8,
        (c.blue() * 255.0) as u8,
        c.alpha(),
    )
}

/// Convert `#rrggbb` or another GTK-accepted hex/rgba color into an
/// `rgba(...)` CSS token with the provided alpha. `alpha` is clamped to
/// 0.0..=1.0. If parsing fails, return the input color unchanged so the
/// fallback remains visually usable.
pub(crate) fn focus_border_rgba_css(color_hex: &str, alpha: f32) -> String {
    let alpha = alpha.clamp(0.0, 1.0);
    if let Some(c) = parse(color_hex) {
        format!(
            "rgba({},{},{},{:.3})",
            (c.red() * 255.0) as u8,
            (c.green() * 255.0) as u8,
            (c.blue() * 255.0) as u8,
            alpha,
        )
    } else {
        color_hex.to_string()
    }
}

fn relative_luminance(c: &gdk::RGBA) -> f32 {
    0.2126 * c.red() + 0.7152 * c.green() + 0.0722 * c.blue()
}

fn blend_with_alpha(c: &gdk::RGBA, alpha: f32) -> gdk::RGBA {
    gdk::RGBA::new(c.red(), c.green(), c.blue(), alpha)
}

fn shift_lightness(c: &gdk::RGBA, delta: f32) -> gdk::RGBA {
    let f = |v: f32| (v + delta).clamp(0.0, 1.0);
    gdk::RGBA::new(f(c.red()), f(c.green()), f(c.blue()), c.alpha())
}
