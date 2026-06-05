// SPDX-License-Identifier: GPL-3.0-or-later
//! User-customizable keyboard shortcuts.
//!
//! Stored under the `keybindings` field of `options.json` as a partial
//! overlay over the built-in defaults exposed by [`defaults`]. Only the
//! actions the user changes need to be listed — anything missing keeps
//! its default accelerators. An empty accel array means "leave the
//! action unbound".
//!
//! Action names use the bare form (`"split-right"`, not `"win.split-right"`).
//! Callers that hand the resolved table to GTK prepend the `win.` group
//! prefix themselves.
//!
//! Accelerator strings follow GTK's `gtk_accelerator_parse` syntax
//! (`"<Ctrl><Shift>Page_Up"`, `"<Alt>Left"`, …). This crate does not
//! depend on GTK, so syntactic validation of each accel is deferred to
//! the install step in the `flowmux` crate.
//!
//! Example user file (only the changed actions need to appear):
//!
//! ```json
//! {
//!   "keybindings": {
//!     "copy":  ["<Ctrl>c"],
//!     "paste": ["<Ctrl>v"],
//!     "next-workspace": []
//!   }
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Every action that flowmux exposes as a user-rebindable shortcut.
///
/// The string form (`as_str`) is the wire key used in `options.json` and
/// also the bare action name registered on the `ApplicationWindow` under
/// the `win` group. The two MUST stay in sync — `defaults()` and the
/// install routine both rely on the same mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionId {
    SplitRight,
    SplitDown,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    CloseSurface,
    QuitApp,
    NextSurface,
    PrevSurface,
    NextWorkspace,
    PrevWorkspace,
    Workspace1,
    Workspace2,
    Workspace3,
    Workspace4,
    Workspace5,
    Workspace6,
    Workspace7,
    Workspace8,
    Copy,
    Paste,
    NewSurface,
    NewBrowserSurface,
    NewWorkspace,
    NewWindow,
    /// Copy the focused pane's current working directory to the system
    /// clipboard and surface a toast confirming what was copied.
    CopyPanePath,
}

impl ActionId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SplitRight => "split-right",
            Self::SplitDown => "split-down",
            Self::FocusLeft => "focus-left",
            Self::FocusRight => "focus-right",
            Self::FocusUp => "focus-up",
            Self::FocusDown => "focus-down",
            Self::CloseSurface => "close-surface",
            Self::QuitApp => "quit-app",
            Self::NextSurface => "next-surface",
            Self::PrevSurface => "prev-surface",
            Self::NextWorkspace => "next-workspace",
            Self::PrevWorkspace => "prev-workspace",
            Self::Workspace1 => "workspace-1",
            Self::Workspace2 => "workspace-2",
            Self::Workspace3 => "workspace-3",
            Self::Workspace4 => "workspace-4",
            Self::Workspace5 => "workspace-5",
            Self::Workspace6 => "workspace-6",
            Self::Workspace7 => "workspace-7",
            Self::Workspace8 => "workspace-8",
            Self::Copy => "copy",
            Self::Paste => "paste",
            Self::NewSurface => "new-surface",
            Self::NewBrowserSurface => "new-browser-surface",
            Self::NewWorkspace => "new-workspace",
            Self::NewWindow => "new-window",
            Self::CopyPanePath => "copy-pane-path",
        }
    }

    /// Human-readable label for the options dialog.
    pub fn label(self) -> &'static str {
        match self {
            Self::SplitRight => "Split pane right",
            Self::SplitDown => "Split pane down",
            Self::FocusLeft => "Focus pane left",
            Self::FocusRight => "Focus pane right",
            Self::FocusUp => "Focus pane up",
            Self::FocusDown => "Focus pane down",
            Self::CloseSurface => "Close tab",
            Self::QuitApp => "Quit flowmux",
            Self::NextSurface => "Next tab in pane",
            Self::PrevSurface => "Previous tab in pane",
            Self::NextWorkspace => "Next workspace",
            Self::PrevWorkspace => "Previous workspace",
            Self::Workspace1 => "Go to workspace 1",
            Self::Workspace2 => "Go to workspace 2",
            Self::Workspace3 => "Go to workspace 3",
            Self::Workspace4 => "Go to workspace 4",
            Self::Workspace5 => "Go to workspace 5",
            Self::Workspace6 => "Go to workspace 6",
            Self::Workspace7 => "Go to workspace 7",
            Self::Workspace8 => "Go to workspace 8",
            Self::Copy => "Copy selection",
            Self::Paste => "Paste clipboard",
            Self::NewSurface => "New terminal tab",
            Self::NewBrowserSurface => "New browser tab",
            Self::NewWorkspace => "New workspace",
            Self::NewWindow => "New window",
            Self::CopyPanePath => "Copy focused pane path",
        }
    }

    /// Ordered list of every editable action. The dialog and the
    /// install step iterate this slice instead of pattern-matching on
    /// each variant so a new action only needs entries here, in
    /// [`Self::as_str`], in [`Self::label`], and in [`DEFAULTS`].
    pub fn all() -> &'static [ActionId] {
        &[
            Self::SplitRight,
            Self::SplitDown,
            Self::FocusLeft,
            Self::FocusRight,
            Self::FocusUp,
            Self::FocusDown,
            Self::CloseSurface,
            Self::QuitApp,
            Self::NextSurface,
            Self::PrevSurface,
            Self::NextWorkspace,
            Self::PrevWorkspace,
            Self::Workspace1,
            Self::Workspace2,
            Self::Workspace3,
            Self::Workspace4,
            Self::Workspace5,
            Self::Workspace6,
            Self::Workspace7,
            Self::Workspace8,
            Self::Copy,
            Self::Paste,
            Self::NewSurface,
            Self::NewBrowserSurface,
            Self::NewWorkspace,
            Self::NewWindow,
            Self::CopyPanePath,
        ]
    }

    pub fn from_wire(s: &str) -> Option<ActionId> {
        Self::all().iter().copied().find(|a| a.as_str() == s)
    }

    /// User-editable shortcuts. Copy / Paste are intentionally excluded
    /// because they are universally `Ctrl+Shift+C` / `Ctrl+Shift+V` in
    /// every modern terminal emulator and rebinding them through the
    /// dialog would let the user accidentally swap them with `Ctrl+C` —
    /// the same key that sends SIGINT to the foreground process. The
    /// install path still applies their hard-coded defaults; only the
    /// dialog and the override resolution skip them.
    pub fn is_user_editable(self) -> bool {
        !matches!(self, Self::Copy | Self::Paste)
    }

    /// Iterator over actions that should appear in the options dialog
    /// and accept overrides from `options.json`.
    pub fn editable() -> impl Iterator<Item = ActionId> {
        Self::all().iter().copied().filter(|a| a.is_user_editable())
    }
}

/// One action can carry multiple accelerators (e.g. Ctrl+Shift+Tab and
/// Ctrl+ISO_Left_Tab both move to the previous workspace). The defaults
/// here mirror what `BINDINGS` used to hold before the table moved into
/// the config crate — keep changes in lock-step with the regression
/// tests in `flowmux::keybindings`.
const DEFAULTS: &[(ActionId, &[&str])] = &[
    (ActionId::SplitRight, &["<Ctrl><Shift>Page_Up"]),
    (ActionId::SplitDown, &["<Ctrl><Shift>Page_Down"]),
    (ActionId::FocusLeft, &["<Alt>Left"]),
    (ActionId::FocusRight, &["<Alt>Right"]),
    (ActionId::FocusUp, &["<Alt>Up"]),
    (ActionId::FocusDown, &["<Alt>Down"]),
    (ActionId::CloseSurface, &["<Alt>w"]),
    (ActionId::QuitApp, &["<Ctrl><Shift>w"]),
    (ActionId::NextSurface, &["<Ctrl><Shift>Right"]),
    (ActionId::PrevSurface, &["<Ctrl><Shift>Left"]),
    (ActionId::NextWorkspace, &["<Ctrl>Tab"]),
    (ActionId::PrevWorkspace, &["<Ctrl><Shift>Tab"]),
    (ActionId::Workspace1, &["<Alt>1"]),
    (ActionId::Workspace2, &["<Alt>2"]),
    (ActionId::Workspace3, &["<Alt>3"]),
    (ActionId::Workspace4, &["<Alt>4"]),
    (ActionId::Workspace5, &["<Alt>5"]),
    (ActionId::Workspace6, &["<Alt>6"]),
    (ActionId::Workspace7, &["<Alt>7"]),
    (ActionId::Workspace8, &["<Alt>8"]),
    (ActionId::Copy, &["<Ctrl><Shift>c"]),
    (ActionId::Paste, &["<Ctrl><Shift>v"]),
    (ActionId::NewSurface, &["<Ctrl><Shift>t"]),
    (ActionId::NewBrowserSurface, &["<Ctrl><Shift>b"]),
    (ActionId::NewWorkspace, &["<Ctrl>n"]),
    (ActionId::NewWindow, &["<Ctrl><Shift>n"]),
    (ActionId::CopyPanePath, &["<Ctrl><Shift>k"]),
];

/// Built-in default accelerators. The first install path reads this and
/// then layers any user overrides on top.
pub fn defaults() -> &'static [(ActionId, &'static [&'static str])] {
    DEFAULTS
}

/// Default accelerators for a single action.
pub fn default_accels(action: ActionId) -> &'static [&'static str] {
    DEFAULTS
        .iter()
        .find_map(|(a, accels)| (*a == action).then_some(*accels))
        .unwrap_or(&[])
}

/// User overrides serialized inside `options.json` as `"keybindings": { … }`.
///
/// Storage is intentionally a `String → Vec<String>` map rather than
/// `ActionId → Vec<String>` so an unknown action key in the user file
/// can be detected and dropped with a warning at resolve time instead
/// of failing the whole load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeybindingOverrides {
    map: HashMap<String, Vec<String>>,
}

impl KeybindingOverrides {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Look up the user's override for `action`, if any.
    pub fn get(&self, action: ActionId) -> Option<&[String]> {
        self.map.get(action.as_str()).map(|v| v.as_slice())
    }

    /// Set an override. Pass an empty vec to mark the action as
    /// explicitly unbound.
    pub fn set(&mut self, action: ActionId, accels: Vec<String>) {
        self.map.insert(action.as_str().to_string(), accels);
    }

    /// Drop the override so the default takes effect again.
    pub fn clear(&mut self, action: ActionId) {
        self.map.remove(action.as_str());
    }

    /// Drop every override.
    pub fn clear_all(&mut self) {
        self.map.clear();
    }

    /// Partial-overlay resolution: for every known [`ActionId`] return
    /// the user override when present, otherwise the built-in default.
    /// Overrides on non-editable actions (see [`ActionId::is_user_editable`])
    /// are ignored so a stale `copy` / `paste` entry left over from an
    /// earlier flowmux release cannot reroute SIGINT-sensitive keys.
    /// Unknown action keys in the user map are silently dropped (the
    /// caller may log them via [`Self::unknown_keys`]).
    pub fn resolve(&self) -> Vec<(ActionId, Vec<String>)> {
        ActionId::all()
            .iter()
            .map(|action| {
                let user_override = if action.is_user_editable() {
                    self.get(*action).map(|user| user.to_vec())
                } else {
                    None
                };
                let accels = user_override.unwrap_or_else(|| {
                    default_accels(*action)
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect()
                });
                (*action, accels)
            })
            .collect()
    }

    /// Override keys present in the user file that target non-editable
    /// actions (currently `copy` / `paste`). Logged at install time so
    /// the user understands why their entry had no effect.
    pub fn non_editable_keys(&self) -> Vec<String> {
        self.map
            .keys()
            .filter(|k| {
                ActionId::from_wire(k)
                    .map(|a| !a.is_user_editable())
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    /// Keys in the user file that do not match any known action. The
    /// install step logs these so a typo in `options.json` does not
    /// silently disappear.
    pub fn unknown_keys(&self) -> Vec<String> {
        self.map
            .keys()
            .filter(|k| ActionId::from_wire(k).is_none())
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_every_action() {
        for action in ActionId::all() {
            assert!(
                DEFAULTS.iter().any(|(a, _)| a == action),
                "missing default for {:?}",
                action
            );
        }
    }

    #[test]
    fn wire_strings_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for action in ActionId::all() {
            assert!(seen.insert(action.as_str()), "duplicate wire string");
        }
    }

    #[test]
    fn from_wire_round_trips_every_variant() {
        for action in ActionId::all() {
            assert_eq!(ActionId::from_wire(action.as_str()), Some(*action));
        }
        assert_eq!(ActionId::from_wire("not-an-action"), None);
    }

    #[test]
    fn resolve_returns_defaults_when_overrides_are_empty() {
        let overrides = KeybindingOverrides::new();
        let resolved = overrides.resolve();
        assert_eq!(resolved.len(), ActionId::all().len());
        for (action, accels) in &resolved {
            let want: Vec<String> = default_accels(*action)
                .iter()
                .map(|s| s.to_string())
                .collect();
            assert_eq!(accels, &want, "{:?}", action);
        }
    }

    #[test]
    fn resolve_overlays_only_specified_actions() {
        let mut overrides = KeybindingOverrides::new();
        overrides.set(ActionId::SplitRight, vec!["<Ctrl><Alt>r".to_string()]);

        let resolved = overrides.resolve();
        let split = resolved
            .iter()
            .find(|(a, _)| *a == ActionId::SplitRight)
            .unwrap();
        assert_eq!(split.1, vec!["<Ctrl><Alt>r".to_string()]);

        // Untouched action keeps its default.
        let down = resolved
            .iter()
            .find(|(a, _)| *a == ActionId::SplitDown)
            .unwrap();
        assert_eq!(down.1, vec!["<Ctrl><Shift>Page_Down".to_string()]);
    }

    #[test]
    fn empty_array_unbinds_action() {
        let mut overrides = KeybindingOverrides::new();
        overrides.set(ActionId::SplitRight, vec![]);
        let resolved = overrides.resolve();
        let split = resolved
            .iter()
            .find(|(a, _)| *a == ActionId::SplitRight)
            .unwrap();
        assert!(split.1.is_empty(), "empty array must mark action unbound");
    }

    #[test]
    fn unknown_keys_are_listed_but_do_not_break_resolve() {
        let json = r#"{
            "split-right": ["<Ctrl><Alt>r"],
            "totally-fake-action": ["<Ctrl>x"]
        }"#;
        let overrides: KeybindingOverrides = serde_json::from_str(json).unwrap();

        let unknown = overrides.unknown_keys();
        assert_eq!(unknown, vec!["totally-fake-action".to_string()]);

        // resolve() still yields the known override and all the defaults.
        let resolved = overrides.resolve();
        let split = resolved
            .iter()
            .find(|(a, _)| *a == ActionId::SplitRight)
            .unwrap();
        assert_eq!(split.1, vec!["<Ctrl><Alt>r".to_string()]);
        assert_eq!(resolved.len(), ActionId::all().len());
    }

    #[test]
    fn serde_round_trip_preserves_overrides() {
        let mut overrides = KeybindingOverrides::new();
        overrides.set(ActionId::Copy, vec!["<Ctrl>c".to_string()]);
        overrides.set(ActionId::Paste, vec![]);

        let s = serde_json::to_string(&overrides).unwrap();
        let back: KeybindingOverrides = serde_json::from_str(&s).unwrap();
        assert_eq!(overrides, back);
    }

    #[test]
    fn empty_object_deserializes_to_default() {
        let overrides: KeybindingOverrides = serde_json::from_str("{}").unwrap();
        assert!(overrides.is_empty());
        assert_eq!(overrides, KeybindingOverrides::default());
    }

    #[test]
    fn clear_removes_single_override() {
        let mut overrides = KeybindingOverrides::new();
        overrides.set(ActionId::Copy, vec!["<Ctrl>c".to_string()]);
        overrides.set(ActionId::Paste, vec!["<Ctrl>v".to_string()]);
        overrides.clear(ActionId::Copy);
        assert!(overrides.get(ActionId::Copy).is_none());
        assert!(overrides.get(ActionId::Paste).is_some());
    }

    #[test]
    fn copy_and_paste_are_excluded_from_editable_set() {
        let editable: Vec<ActionId> = ActionId::editable().collect();
        assert!(!editable.contains(&ActionId::Copy));
        assert!(!editable.contains(&ActionId::Paste));
        // Sanity: non-clipboard actions remain editable.
        assert!(editable.contains(&ActionId::SplitRight));
        assert!(editable.contains(&ActionId::QuitApp));
    }

    #[test]
    fn resolve_ignores_user_override_for_copy_and_paste() {
        let mut overrides = KeybindingOverrides::new();
        overrides.set(ActionId::Copy, vec!["<Ctrl>c".into()]);
        overrides.set(ActionId::Paste, vec!["<Ctrl>v".into()]);

        let resolved = overrides.resolve();
        let copy = resolved.iter().find(|(a, _)| *a == ActionId::Copy).unwrap();
        let paste = resolved
            .iter()
            .find(|(a, _)| *a == ActionId::Paste)
            .unwrap();
        assert_eq!(copy.1, vec!["<Ctrl><Shift>c".to_string()]);
        assert_eq!(paste.1, vec!["<Ctrl><Shift>v".to_string()]);
    }

    #[test]
    fn non_editable_keys_lists_copy_and_paste_when_set() {
        let mut overrides = KeybindingOverrides::new();
        overrides.set(ActionId::Copy, vec!["<Ctrl>c".into()]);
        overrides.set(ActionId::SplitRight, vec!["<Ctrl>r".into()]);
        let mut keys = overrides.non_editable_keys();
        keys.sort();
        assert_eq!(keys, vec!["copy".to_string()]);
    }

    /// Multi-accel actions (Prev workspace has three) must serialize as
    /// an array and round-trip without reordering surprises.
    #[test]
    fn multi_accel_action_round_trips() {
        let mut overrides = KeybindingOverrides::new();
        overrides.set(
            ActionId::PrevWorkspace,
            vec!["<Ctrl><Shift>Tab".into(), "<Ctrl>ISO_Left_Tab".into()],
        );
        let s = serde_json::to_string(&overrides).unwrap();
        let back: KeybindingOverrides = serde_json::from_str(&s).unwrap();
        assert_eq!(overrides, back);
    }
}
