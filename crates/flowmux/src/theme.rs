// SPDX-License-Identifier: GPL-3.0-or-later
//! Visual theme.
//!
//! Resolution order:
//!
//! 1. flowmux's own theme config at `$XDG_CONFIG_HOME/flowmux/theme`,
//!    if the file exists. This is flowmux's own location — no other
//!    application's config is consulted.
//! 2. flowmux's built-in neutral dark default, authored in this file.
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
    /// Only `Some` when the source provided all 16 ANSI entries. We
    /// don't synthesize a partial palette from defaults to avoid
    /// inventing colors the user didn't ask for.
    pub palette: Option<Vec<gdk::RGBA>>,
}

impl ResolvedTheme {
    pub fn load() -> Self {
        let cfg = flowmux_config::theme::load().unwrap_or_default();
        let theme = Self::from_ghostty(&cfg);
        let path = flowmux_config::theme::user_theme_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        tracing::info!(
            source = if cfg.background.is_some() { "user theme file" } else { "builtin defaults" },
            path = %path,
            bg = ?cfg.background.as_deref(),
            fg = ?cfg.foreground.as_deref(),
            font_family = ?cfg.font_family.as_deref(),
            font_size = ?cfg.font_size,
            palette_set = theme.palette.is_some(),
            "resolved theme"
        );
        theme
    }

    fn from_ghostty(cfg: &flowmux_config::ghostty::GhosttyConfig) -> Self {
        // Built-in fallbacks (flowmux-authored) only kick in if the user's
        // ghostty config + theme file haven't supplied a value.
        let bg = cfg
            .background
            .as_deref()
            .and_then(parse)
            .unwrap_or_else(|| parse_or_black("#0e1116"));
        let fg = cfg
            .foreground
            .as_deref()
            .and_then(parse)
            .unwrap_or_else(|| parse_or_black("#d8dee4"));
        let cursor = cfg
            .cursor_color
            .as_deref()
            .and_then(parse)
            .unwrap_or_else(|| parse_or_black("#c5cdd9"));
        let selection_bg = cfg.selection_background.as_deref().and_then(parse);
        let selection_fg = cfg.selection_foreground.as_deref().and_then(parse);

        let parsed: Vec<Option<gdk::RGBA>> = cfg
            .palette
            .iter()
            .map(|s| s.as_deref().and_then(parse))
            .collect();
        let palette = if parsed.iter().all(Option::is_some) {
            Some(parsed.into_iter().map(Option::unwrap).collect())
        } else {
            None
        };

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
        match &self.palette {
            Some(pal) => {
                let refs: Vec<&gdk::RGBA> = pal.iter().collect();
                term.set_colors(Some(&self.fg), Some(&self.bg), &refs);
            }
            None => {
                term.set_color_background(&self.bg);
                term.set_color_foreground(&self.fg);
            }
        }
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
    border-radius: 6px;
    margin: 2px 6px;
    padding: 8px 10px;
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
