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
//! * flowmux terminal widgets — font, bg/fg/cursor, ANSI palette, selection.
//! * libadwaita color scheme — forced dark when the background is dark.
//! * Global CSS — pane frame and sidebar tint.
//!
//! Users who want a specific look should drop a theme file into
//! `~/.config/flowmux/theme` (see `resources/themes/example.theme` for
//! the format) or run `flowmux theme import <path>` to copy one from
//! anywhere on their machine.

use flowmux_core::AGENT_BAR_ITEM_MIN_WIDTH_PX;
use gtk::gdk;
use gtk::pango;

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
    /// shipped colors instead of toolkit defaults.
    pub palette: Vec<gdk::RGBA>,
}

/// Ghostty's default ANSI 16-color palette, based on the Tomorrow
/// scheme. The two black slots are raised slightly for flowmux's
/// `#282c34` default background so ANSI black text remains legible on a
/// fresh install.
pub(crate) const DEFAULT_BG: &str = "#282c34";
pub(crate) const DEFAULT_FG: &str = "#ffffff";

pub(crate) const DEFAULT_PALETTE: [&str; 16] = [
    "#5c6370", "#cc6666", "#b5bd68", "#f0c674", "#81a2be", "#b294bb", "#8abeb7", "#c5c8c6",
    "#7f848e", "#d54e53", "#b9ca4a", "#e7c547", "#7aa6da", "#c397d8", "#70c0b1", "#eaeaea",
];

impl ResolvedTheme {
    pub fn load() -> Self {
        Self::resolve(&flowmux_config::options::load())
    }

    /// Resolve the effective theme for the given options:
    ///
    /// 1. The preset named by `options.theme`, if any — the user's theme
    ///    file still contributes its font when the preset has none.
    /// 2. Otherwise the user's `~/.config/flowmux/theme` file, if present.
    /// 3. `options.theme_overrides` layered on top.
    /// 4. Built-in defaults for anything still unset (in `from_ghostty`).
    pub fn resolve(options: &flowmux_config::options::Options) -> Self {
        Self::resolve_with_file(options, flowmux_config::theme::load())
    }

    fn resolve_with_file(
        options: &flowmux_config::options::Options,
        file_cfg: Option<flowmux_config::ghostty::GhosttyConfig>,
    ) -> Self {
        let mut cfg = match options.theme.as_deref() {
            Some(id) => {
                let mut preset = flowmux_config::presets::config(id).unwrap_or_else(|| {
                    tracing::warn!(theme = id, "unknown theme preset — using the default look");
                    Default::default()
                });
                if let Some(file) = file_cfg {
                    preset.font_family = preset.font_family.or(file.font_family);
                    preset.font_size = preset.font_size.or(file.font_size);
                }
                preset
            }
            None => file_cfg.unwrap_or_default(),
        };
        cfg.merge(options.theme_overrides.to_ghostty());
        let theme = Self::from_ghostty(&cfg);
        tracing::info!(
            preset = ?options.theme.as_deref(),
            overrides = !options.theme_overrides.is_empty(),
            bg = ?cfg.background.as_deref(),
            fg = ?cfg.foreground.as_deref(),
            font_family = ?cfg.font_family.as_deref(),
            font_size = ?cfg.font_size,
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
            .unwrap_or_else(|| parse_or_black(DEFAULT_BG));
        let fg = cfg
            .foreground
            .as_deref()
            .and_then(parse)
            .unwrap_or_else(|| parse_or_black(DEFAULT_FG));
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

    /// Resolved theme font family (the `font-family` from the theme file, or
    /// the built-in `monospace` fallback). Used to seed the options dialog
    /// when the user has no font override.
    pub fn font_family(&self) -> String {
        self.font
            .family()
            .map(|f| f.to_string())
            .unwrap_or_else(|| "monospace".to_string())
    }

    /// Resolved theme font size in points (the theme file's `font-size`, or
    /// the built-in 12pt fallback).
    pub fn font_size(&self) -> f32 {
        let size = self.font.size();
        if size <= 0 {
            12.0
        } else {
            size as f32 / pango::SCALE as f32
        }
    }

    /// Build the effective terminal font: start from the resolved theme font
    /// and layer the options dialog's family / size overrides on top. `None`
    /// for either field keeps the theme value, so a fresh install with no
    /// overrides reproduces the theme font exactly.
    pub fn font_with_overrides(
        &self,
        family: Option<&str>,
        size: Option<f32>,
    ) -> pango::FontDescription {
        let mut desc = self.font.clone();
        if let Some(family) = family.map(str::trim).filter(|f| !f.is_empty()) {
            desc.set_family(family);
        }
        if let Some(size) = size.filter(|s| *s > 0.0) {
            desc.set_size((size * pango::SCALE as f32).round() as i32);
        }
        desc
    }

    /// Push the theme font + default fg/bg, the 16 ANSI palette colors, the
    /// cursor color, and selection colors into the VTE terminal pane.
    /// Indices 16..256 keep VTE's standard xterm fill (so a 16-color
    /// theme expands the same way a traditional terminal would).
    pub fn apply_to_ghostty(&self, pane: &crate::ui::ghostty_pane::GhosttyPane) {
        use flowmux_terminal::Rgb;
        fn to_rgb(c: &gdk::RGBA) -> Rgb {
            Rgb {
                r: (c.red() * 255.0).round().clamp(0.0, 255.0) as u8,
                g: (c.green() * 255.0).round().clamp(0.0, 255.0) as u8,
                b: (c.blue() * 255.0).round().clamp(0.0, 255.0) as u8,
            }
        }
        pane.set_font(&self.font);
        let palette: Vec<Rgb> = self.palette.iter().map(to_rgb).collect();
        pane.apply_colors(
            to_rgb(&self.fg),
            to_rgb(&self.bg),
            to_rgb(&self.cursor),
            &palette,
            self.selection_bg.as_ref().map(to_rgb),
            self.selection_fg.as_ref().map(to_rgb),
        );
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
        // Active-workspace edge stripes always paint at full opacity,
        // independent of the focus-border opacity slider.
        let focus_full_css = focus_border_rgba_css(focus_border_color, 1.0);
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
        // Fainter than control_hover so the active workspace row only
        // whispers under the pointer instead of reading as a full tint.
        let sidebar_hover_css = rgba_css(&blend_with_alpha(&self.fg, 0.045));
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
    border: 0;
    border-radius: 0;
    margin: 0;
    padding: 0;
}}
.flowmux-pane.focused .flowmux-pane-tabbar,
.flowmux-pane.flowmux-notification .flowmux-pane-tabbar {{
    box-shadow: inset 0 2px {focus};
}}
.flowmux-pane.focused.flowmux-solo .flowmux-pane-tabbar {{
    box-shadow: none;
}}
.flowmux-pane .flowmux-terminal {{
    padding: 7px;
    border-radius: 0;
}}
.flowmux-terminal-search-entry {{
    /* Keep the search field readable over terminal output while retaining a
       small amount of the underlying theme for visual continuity. */
    background-color: alpha(@theme_base_color, 0.95);
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
.flowmux-pane-zoom-badge {{
    color: {focus_full};
    border: 1px solid {focus};
    border-radius: 4px;
    margin: 3px 4px 3px 5px;
    padding: 0 5px;
}}
.flowmux-pane-tab {{
    margin: 2px 1px 0 0;
    border: 0;
    border-radius: 4px 4px 0 0;
}}
.flowmux-pane-tab.active {{
    background-color: {tab_active};
}}
.flowmux-pane-tabs.has-multi-tabs > .flowmux-pane-tab.active {{
    border-top: 2px solid {focus};
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
.flowmux-sidebar-shell {{
    background-color: @sidebar_bg_color;
    color: @sidebar_fg_color;
}}
.flowmux-sidebar-shell headerbar {{
    background-color: @sidebar_bg_color;
    color: @sidebar_fg_color;
}}
.flowmux-window-split > separator {{
    background-color: transparent;
    background-image: none;
    border: 0;
    box-shadow: none;
}}
.navigation-sidebar {{
    background-color: @sidebar_bg_color;
}}
.navigation-sidebar row {{
    color: @sidebar_fg_color;
    border-radius: 6px;
    margin: 2px 6px;
    padding: 8px 10px;
}}
/* Keep the window chrome on libadwaita's semantic sidebar palette;
   terminal themes remain scoped to panes. The .dim-label variants keep
   their natural dimming because that class adjusts opacity, not color. */
.navigation-sidebar row label,
.navigation-sidebar row label.heading,
.navigation-sidebar row label.caption {{
    color: @sidebar_fg_color;
}}
/* Metadata rows use a decorative Cairo-drawn tree gutter. Keep the lines
   neutral and quieter than text/status colors; active and hovered rows lift
   contrast slightly without competing with the workspace accent stripe. */
.navigation-sidebar row .flowmux-sidebar-tree-gutter {{
    color: alpha(@sidebar_fg_color, 0.24);
}}
.navigation-sidebar row.activatable:hover .flowmux-sidebar-tree-gutter,
.navigation-sidebar row.activatable:active .flowmux-sidebar-tree-gutter {{
    color: alpha(@sidebar_fg_color, 0.30);
}}
.navigation-sidebar row.activatable:selected .flowmux-sidebar-tree-gutter {{
    color: alpha(@sidebar_fg_color, 0.34);
}}
/* Suppress libadwaita selected-row tint on workspace rows. The ListBox
   keeps SelectionMode::Single so navigation helpers can read
   selected_workspace(), but flowmux does not paint active-workspace
   as a separate visual state — focus and .flowmux-attention are the
   only highlights users see. libadwaita ships rules whose selectors
   include row.activatable plus :selected combined with :hover, :active,
   .has-open-popup, and a child-combinator variant with a 1px inset
   border. Plain row:selected loses on specificity, so each variant is
   matched explicitly with .activatable below and the border is cleared too. */
.navigation-sidebar row.activatable:selected,
.navigation-sidebar row.activatable:selected:focus,
.navigation-sidebar row.activatable:selected.has-open-popup,
.navigation-sidebar > row.activatable:selected,
.navigation-sidebar > row.activatable:selected.has-open-popup {{
    background-color: transparent;
    border-left: 0 solid transparent;
}}
/* Visible "active workspace" indicator. A left-edge accent
   stripe in the focus color lets the user see which workspace is
   currently active. Always full opacity (focus_full), independent of
   the focus-border opacity slider, and is the only row-level left-edge
   accent. The :hover/:focus/:active/
   .has-open-popup variants must mirror the suppression block above: those
   selectors clear borders at the same specificity, so without matching
   variants here the stripe vanishes whenever the row is hovered. */
.navigation-sidebar row.activatable:selected,
.navigation-sidebar row.activatable:selected:hover,
.navigation-sidebar row.activatable:selected:focus,
.navigation-sidebar row.activatable:selected:active,
.navigation-sidebar row.activatable:selected.has-open-popup,
.navigation-sidebar > row.activatable:selected,
.navigation-sidebar > row.activatable:selected:hover,
.navigation-sidebar > row.activatable:selected:active,
.navigation-sidebar > row.activatable:selected.has-open-popup {{
    border-left: 5px solid {focus_full};
    /* Nudge the selected row's content 5px right (base left padding is
       10px) so the active workspace reads as indented. */
    padding-left: 15px;
}}
/* Hover and press share one faint tint across every workspace row, so
   hovering only whispers and clicking introduces no separate color step.
   The :selected variants are spelled out to outrank libadwaita's own
   selected-hover/active rules; the idle :selected state stays untinted
   (handled by the suppression block above). */
.navigation-sidebar row.activatable:hover,
.navigation-sidebar row.activatable:active,
.navigation-sidebar row.activatable:selected:hover,
.navigation-sidebar row.activatable:selected:active,
.navigation-sidebar > row.activatable:hover,
.navigation-sidebar > row.activatable:active,
.navigation-sidebar > row.activatable:selected:hover,
.navigation-sidebar > row.activatable:selected:active {{
    background-color: alpha(@sidebar_fg_color, 0.055);
}}
.navigation-sidebar row.activatable:selected label,
.navigation-sidebar row.activatable:selected label.heading,
.navigation-sidebar row.activatable:selected label.caption {{
    color: @sidebar_fg_color;
}}
.navigation-sidebar row.flowmux-attention {{
    background-color: rgba(245, 158, 11, 0.18);
}}
.flowmux-workspace-notification-dot {{
    min-width: 7px;
    min-height: 7px;
    border-radius: 99px;
    background-color: {focus_full};
}}
.navigation-sidebar row.flowmux-agent-blocked {{
    background-color: rgba(239, 68, 68, 0.16);
}}
.navigation-sidebar row.flowmux-agent-done {{
    background-color: rgba(59, 130, 246, 0.14);
}}
.navigation-sidebar row label.flowmux-sidebar-agent-blocked,
.navigation-sidebar row image.flowmux-sidebar-agent-blocked {{
    color: rgba(239, 68, 68, 0.95);
}}
.navigation-sidebar row label.flowmux-sidebar-agent-working,
.navigation-sidebar row image.flowmux-sidebar-agent-working {{
    color: rgba(245, 158, 11, 0.95);
}}
.navigation-sidebar row label.flowmux-sidebar-agent-done,
.navigation-sidebar row image.flowmux-sidebar-agent-done {{
    color: rgba(59, 130, 246, 0.95);
}}
.navigation-sidebar row label.flowmux-sidebar-agent-idle,
.navigation-sidebar row image.flowmux-sidebar-agent-idle,
.navigation-sidebar row label.flowmux-sidebar-agent-unknown,
.navigation-sidebar row image.flowmux-sidebar-agent-unknown {{
    color: @sidebar_fg_color;
    opacity: 0.72;
}}
.flowmux-agent-bar {{
    background-color: {sidebar};
    border-top: 1px solid {border};
    padding: 3px 6px;
}}
.flowmux-agent-bar > label {{
    color: {fg};
    min-width: 52px;
}}
.flowmux-agent-bar-item {{
    min-width: {agent_item_min}px;
    min-height: 39px;
    padding: 3px 6px;
    border: 1px solid transparent;
    border-radius: 6px;
}}
.flowmux-agent-bar-item:hover {{
    background-color: {control_hover};
}}
.flowmux-agent-bar-item.focused {{
    border-color: {focus};
}}
.flowmux-agent-bar-item.flowmux-attention {{
    background-color: rgba(245, 158, 11, 0.18);
}}
.flowmux-agent-bar-item.flowmux-drop-before {{
    border-left-color: {focus_full};
}}
.flowmux-agent-bar-item.flowmux-drop-after {{
    border-right-color: {focus_full};
}}
.flowmux-agent-bar-color {{
    border-radius: 2px;
}}
.navigation-sidebar row.flowmux-dragging {{
    opacity: 0.4;
}}
.navigation-sidebar row.flowmux-drop-above {{
    border-top: 2px solid rgba(96, 165, 250, 0.95);
}}
.navigation-sidebar row.flowmux-drop-below {{
    border-bottom: 2px solid rgba(96, 165, 250, 0.95);
}}
.flowmux-pane-tab.flowmux-pane-tab-dragging {{
    opacity: 0.4;
}}
.flowmux-pane-tab.flowmux-pane-tab-drop-before {{
    border-left: 2px solid rgba(96, 165, 250, 0.95);
}}
.flowmux-pane-tab.flowmux-pane-tab-drop-after {{
    border-right: 2px solid rgba(96, 165, 250, 0.95);
}}
.flowmux-file-browser {{
    background-color: {sidebar};
    color: {fg};
    border-left: 1px solid {border};
}}
.flowmux-file-browser.focused {{
    border-left: 2px solid {focus};
}}
.flowmux-file-browser-header {{
    padding: 8px 8px 6px 10px;
    border-bottom: 1px solid {border};
}}
.flowmux-file-browser.focused .flowmux-file-browser-header {{
    background-color: {sidebar_hover};
}}
.flowmux-file-browser-list {{
    background: transparent;
}}
.flowmux-file-browser-row {{
    min-height: 24px;
}}
.flowmux-file-browser-row:hover {{
    background-color: {sidebar_hover};
}}
.flowmux-file-browser-row:selected {{
    background-color: {control_hover};
}}
.flowmux-file-browser-row.selected {{
    background-color: {control_hover};
}}
.flowmux-file-browser-row.focused {{
    background-color: {control_hover};
}}
.flowmux-file-browser-row.cut {{
    opacity: 0.45;
}}
.flowmux-worktree-panel {{
    background-color: {sidebar};
    color: {fg};
    border-left: 1px solid {border};
}}
.flowmux-worktree-panel.focused {{
    border-left: 2px solid {focus};
}}
.flowmux-worktree-panel-header {{
    padding: 8px 8px 6px 10px;
    border-bottom: 1px solid {border};
}}
.flowmux-worktree-list {{
    background: transparent;
}}
.flowmux-worktree-row:hover,
.flowmux-worktree-row:selected {{
    background-color: {sidebar_hover};
}}
.flowmux-clipboard-toast {{
    background-color: {toast_bg};
    color: {fg};
    border: 1px solid {toast_border};
    border-radius: 8px;
    padding: 8px 14px;
}}
.flowmux-overlay-menu {{
    background-color: {toast_bg};
    color: {fg};
    border: 1px solid {toast_border};
    border-radius: 8px;
    padding: 4px 0;
}}
"#,
            bg = bg_css,
            fg = rgba_css(&self.fg),
            border = pane_border_css,
            focus = focus_css,
            focus_full = focus_full_css,
            tabbar = tabbar_bg_css,
            tab_active = tab_active_bg_css,
            control_hover = control_hover_css,
            sidebar_hover = sidebar_hover_css,
            subdued_fg = subdued_fg_css,
            sidebar = sidebar_bg,
            toast_bg = toast_bg_css,
            toast_border = toast_border_css,
            agent_item_min = AGENT_BAR_ITEM_MIN_WIDTH_PX,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper that yields a deterministic CSS string. We intentionally
    /// construct the theme through `from_ghostty` with an empty config
    /// so the assertions key on selectors / border values,
    /// not on theme-derived colour bytes that depend on Ghostty's
    /// shipped fallback palette.
    fn sample_css() -> String {
        let cfg = flowmux_config::ghostty::GhosttyConfig::default();
        ResolvedTheme::from_ghostty(&cfg).css("#fff4b3", 0.5)
    }

    #[test]
    fn resolve_prefers_preset_over_theme_file_but_keeps_its_font() {
        let file_cfg = flowmux_config::ghostty::parse(
            "background = #101010\nfont-family = Fira Code\nfont-size = 14\n",
        );
        let options = flowmux_config::options::Options::default();

        // No preset selected → the theme file wins (legacy behavior).
        let theme = ResolvedTheme::resolve_with_file(&options, Some(file_cfg.clone()));
        assert_eq!(rgba_css(&theme.bg), "rgba(16,16,16,1)");

        // Preset selected → preset colors win, theme-file font survives.
        let mut options = options;
        options.theme = Some("dracula".into());
        let theme = ResolvedTheme::resolve_with_file(&options, Some(file_cfg));
        assert_eq!(rgba_css(&theme.bg), "rgba(40,42,54,1)");
        assert_eq!(theme.font_family(), "Fira Code");
        assert!((theme.font_size() - 14.0).abs() < 0.01);
    }

    #[test]
    fn resolve_layers_color_overrides_on_top_of_the_preset() {
        let mut options = flowmux_config::options::Options {
            theme: Some("nord".into()),
            ..Default::default()
        };
        options.theme_overrides.background = Some("#123456".into());
        options.theme_overrides.cursor = Some("not-a-color".into());

        let theme = ResolvedTheme::resolve_with_file(&options, None);

        assert_eq!(rgba_css(&theme.bg), "rgba(18,52,86,1)");
        // Invalid override is discarded → Nord's own cursor color remains.
        assert_eq!(rgba_css(&theme.cursor), "rgba(216,222,233,1)");
    }

    #[test]
    fn resolve_with_unknown_preset_falls_back_to_default_look() {
        let options = flowmux_config::options::Options {
            theme: Some("deleted-preset".into()),
            ..Default::default()
        };

        let theme = ResolvedTheme::resolve_with_file(&options, None);

        assert_eq!(rgba_css(&theme.bg), "rgba(40,44,52,1)");
    }

    #[test]
    fn builtin_ansi_black_stays_legible_on_default_background() {
        let cfg = flowmux_config::ghostty::GhosttyConfig::default();
        let theme = ResolvedTheme::from_ghostty(&cfg);
        let bg_luma = relative_luminance(&theme.bg);

        assert!(
            relative_luminance(&theme.palette[0]) - bg_luma > 0.20,
            "ANSI black must not disappear into the default terminal background"
        );
        assert!(
            relative_luminance(&theme.palette[8]) - bg_luma > 0.30,
            "ANSI bright black must remain visibly brighter than the default background"
        );
    }

    /// A "selected" workspace row in the side panel must be visually
    /// distinguishable. We had a regression where the libadwaita tint
    /// was suppressed without a replacement, leaving the active
    /// workspace with no on-screen indicator. Lock in the left accent
    /// stripe so a future CSS edit cannot silently drop it again.
    #[test]
    fn sidebar_selected_workspace_has_visible_accent() {
        let css = sample_css();
        let selected_block_start = css
            .find(".navigation-sidebar row.activatable:selected")
            .expect("selected-row rule must exist");
        let tail = &css[selected_block_start..];
        assert!(
            tail.contains("border-left: 5px solid"),
            "selected workspace row is missing its left-edge accent stripe"
        );
        // The very last `:selected` rule wins because every block in
        // this stylesheet shares the same specificity; verify the
        // visible block sits *after* the suppression block so the
        // accent is what GTK actually renders.
        let suppression_block = tail
            .find("background-color: transparent")
            .expect("suppression rule must still be present");
        let accent_block = tail
            .find("border-left: 5px solid")
            .expect("accent rule must be present");
        assert!(
            accent_block > suppression_block,
            "accent rule must follow the suppression rule so it overrides"
        );
    }

    #[test]
    fn sidebar_status_tints_do_not_draw_active_workspace_accent() {
        let css = sample_css();

        for selector in [
            ".navigation-sidebar row.flowmux-attention",
            ".navigation-sidebar row.flowmux-agent-blocked",
            ".navigation-sidebar row.flowmux-agent-done",
        ] {
            let rule_start = css.find(selector).expect("sidebar status rule missing");
            let rule_tail = &css[rule_start..];
            let rule_end = rule_tail
                .find('}')
                .expect("sidebar status rule unterminated");
            let rule = &rule_tail[..rule_end];

            assert!(
                !rule.contains("border-left:"),
                "{selector} must not draw the active workspace accent"
            );
        }
    }

    /// Pane focus is shown inside its tab header instead of drawing a rounded
    /// card around the whole pane. Trivial 1-pane / 1-tab workspaces still hide
    /// the focus cue via `.flowmux-solo`.
    #[test]
    fn focused_pane_uses_header_accent_without_card_border() {
        let css = sample_css();
        let pane_rule_idx = css.find(".flowmux-pane {").expect("pane rule missing");
        let pane_rule =
            &css[pane_rule_idx..css[pane_rule_idx..].find('}').unwrap() + pane_rule_idx];
        assert!(
            pane_rule.contains("border: 0;") && pane_rule.contains("border-radius: 0;"),
            "pane shell must remain flat"
        );
        let solo_rule_idx = css
            .find(".flowmux-pane.focused.flowmux-solo .flowmux-pane-tabbar {")
            .expect("solo override missing");
        let tail = &css[solo_rule_idx..];
        assert!(
            css.contains(".flowmux-pane.focused .flowmux-pane-tabbar,")
                && css.contains("box-shadow: inset 0 2px")
                && tail.contains("box-shadow: none"),
            "focus accent must stay in the pane header and disappear for solo panes"
        );
    }

    #[test]
    fn notification_cues_reuse_the_focus_accent() {
        let css = sample_css();
        assert!(css.contains(".flowmux-pane.flowmux-notification .flowmux-pane-tabbar {"));
        assert!(css.contains(".flowmux-workspace-notification-dot {"));
        let dot_rule = css
            .split(".flowmux-workspace-notification-dot {")
            .nth(1)
            .expect("notification dot rule missing");
        assert!(dot_rule
            .split('}')
            .next()
            .is_some_and(|rule| rule.contains("background-color: rgba(")));
    }

    #[test]
    fn sidebar_chrome_uses_libadwaita_semantic_colors() {
        let css = sample_css();
        assert!(
            css.contains(".flowmux-sidebar-shell {")
                && css.contains("background-color: @sidebar_bg_color")
                && css.contains("color: @sidebar_fg_color"),
            "sidebar shell must follow the Ubuntu/libadwaita palette"
        );

        let divider_start = css
            .find(".flowmux-window-split > separator {")
            .expect("window split divider rule missing");
        let divider_tail = &css[divider_start..];
        let divider_rule = &divider_tail[..divider_tail
            .find('}')
            .expect("window split divider rule unterminated")];
        assert!(
            divider_rule.contains("background-color: transparent")
                && divider_rule.contains("background-image: none")
                && divider_rule.contains("border: 0")
                && divider_rule.contains("box-shadow: none"),
            "window split divider must not paint a line between the native headers"
        );
    }

    /// Multi-tab panes paint a 2px top stripe on the active tab. The
    /// stripe is the only visual cue for "this tab is current" when the
    /// pane has ≥2 tabs, so guard the selector against silent drops.
    #[test]
    fn active_tab_top_stripe_visible_in_multi_tab_pane() {
        let css = sample_css();
        let stripe_idx = css
            .find(".flowmux-pane-tabs.has-multi-tabs > .flowmux-pane-tab.active")
            .expect("multi-tab active-stripe selector missing");
        let tail = &css[stripe_idx..];
        assert!(
            tail.contains("border-top: 2px solid"),
            "active-tab top stripe must use a 2px top border"
        );
    }

    #[test]
    fn sidebar_agent_state_colors_match_status_contract() {
        let css = sample_css();
        assert!(
            !css.contains("flowmux-breathe")
                && !css.contains("flowmux-agent-running .flowmux-color-bar"),
            "running agent state must not animate the sidebar color bar"
        );
        assert!(
            css.contains(".navigation-sidebar row.flowmux-agent-blocked")
                && css.contains("rgba(239, 68, 68, 0.16)")
                && css.contains("rgba(239, 68, 68, 0.95)"),
            "blocked agent state must use red row and inline status colors"
        );
        assert!(
            css.contains(".navigation-sidebar row label.flowmux-sidebar-agent-working")
                && css.contains("rgba(245, 158, 11, 0.95)"),
            "working agent inline status must use yellow"
        );
        assert!(
            css.contains(".navigation-sidebar row.flowmux-agent-done")
                && css.contains("rgba(59, 130, 246, 0.14)")
                && css.contains("rgba(59, 130, 246, 0.95)"),
            "unseen done agent state must use blue row and inline status colors"
        );
        assert!(
            css.contains(".flowmux-agent-bar")
                && css.contains(".flowmux-agent-bar-item.flowmux-attention")
                && css.contains(".flowmux-agent-bar-item.focused")
                && css.contains(".flowmux-agent-bar-item.flowmux-drop-before")
                && css.contains(&format!("min-width: {AGENT_BAR_ITEM_MIN_WIDTH_PX}px")),
            "agent bar must keep its bottom-bar and item sizing rules"
        );
    }

    #[test]
    fn sidebar_tree_connectors_stay_neutral_across_row_states() {
        let css = sample_css();
        assert!(
            css.contains(".navigation-sidebar row .flowmux-sidebar-tree-gutter")
                && css.contains("color: alpha(@sidebar_fg_color, 0.24)")
                && css.contains(
                    ".navigation-sidebar row.activatable:hover .flowmux-sidebar-tree-gutter"
                )
                && css.contains("color: alpha(@sidebar_fg_color, 0.30)")
                && css.contains(
                    ".navigation-sidebar row.activatable:selected .flowmux-sidebar-tree-gutter"
                )
                && css.contains("color: alpha(@sidebar_fg_color, 0.34)"),
            "tree connectors must use neutral sidebar foreground contrast, independent of agent status colors"
        );
    }

    #[test]
    fn worktree_panel_styles_cover_focus_header_list_and_rows() {
        let css = sample_css();
        for selector in [
            ".flowmux-worktree-panel {",
            ".flowmux-worktree-panel.focused {",
            ".flowmux-worktree-panel-header {",
            ".flowmux-worktree-list {",
            ".flowmux-worktree-row:hover,",
            ".flowmux-worktree-row:selected {",
        ] {
            assert!(
                css.contains(selector),
                "worktree selector missing: {selector}"
            );
        }
    }
}
