# Tab drag-move across panes / workspaces / windows — design

Date: 2026-06-28

## Goal

Let a user move a pane tab (a `PaneSurface`) by dragging or via a right-click
menu:

1. Drag a tab onto another pane → append the tab at the **end** of that pane.
2. Drag a tab **between two tabs** → insert at the dropped position.
3. Drag a tab onto a **workspace** in the side panel → that workspace becomes
   selected while still mid-drag; the user can then drop into one of its panes
   or between its tabs.
4. Moving a tab (new pane or reordered) must **preserve its state** — the
   terminal PTY/scrollback and browser navigation must not reset.
5. Drop in **empty window space (outside any pane/workspace)** → open the tab in
   a **new window** (already implemented as tear-off; keep working).
6. **Right-click a tab → "Move"** item whose submenu lists every workspace title;
   titles reflect the **live** workspace set at click time. Selecting one moves
   the tab to that workspace.
7. **Selecting a workspace** moves focus to the **last tab of the first pane** of
   that workspace.

## Current state (what already exists)

- `Pane::{add_surface_to_leaf, reorder_surface_in_leaf, close_surface_in_leaf,
  set_active_surface, find_surface}` in `flowmux-core`. No cross-pane move.
- `attach_tab_dnd_handlers` (workspace_view.rs): per-tab `DragSource`
  (payload `pane_id|surface_id`) + `DropTargetAsync`. Drop **rejects cross-pane**
  today; reorder-within-pane works.
- Drag with no drop target → `on_tab_drag_to_new_window` → `TearOffSurface` →
  `take_surface_for_tearoff` (detaches the **live** widget; PTY survives) →
  `present_torn_off_surface` (new `adw::ApplicationWindow`). Requirement 5 done.
- `PaneRegistry` holds live handles: `terminals: HashMap<SurfaceId, PaneTerminal>`,
  `browsers`, plus `surface_stacks`, `pane_tab_containers`, `surface_tabs`,
  `pane_frames`, `pane_workspace`, `surface_workspace`.
- `attach_surface_to_pane`: incrementally appends a tab+content to an
  already-rendered pane — but it **builds new content** from the model. Not
  usable for a state-preserving move as-is.
- `activate_workspace` → `focus_first_leaf_of` (focuses first leaf, does not
  change the active tab).
- Tab context menu: plain `Popover` + `Button` rows, secondary button.

## Design

### Layer 1 — `flowmux-core` (model)

Add two `Pane` methods, mirroring the existing leaf walkers:

- `take_surface_from_leaf(target: PaneId, surface_id) -> Option<(PaneSurface, bool)>`
  Removes the `PaneSurface` from its leaf and returns it plus
  `leaf_now_empty`. Active-id fix-up identical to `close_surface_in_leaf`.
- `insert_surface_into_leaf(target: PaneId, surface: PaneSurface, index: usize) -> Option<SurfaceId>`
  Inserts at `index` (clamped to len), sets the leaf's `active` to it.

Unit-tested in `flowmux-core`: same-tree move, end-append, mid-insert, last-tab
removal flag, not-found, index clamping, self-move no-op.

### Layer 2 — `flowmux-daemon` (state store)

- `move_surface_to_pane(src_pane, surface, dst_pane, target_index) -> Option<MoveSurfaceOutcome>`
  Works for both same-workspace and cross-workspace:
  1. Locate the workspace + `Surface` tree owning `src_pane`; `take_surface_from_leaf`.
  2. Locate the tree owning `dst_pane`; `insert_surface_into_leaf` at index.
  3. If the source leaf emptied, collapse it (reuse the same `remove_leaf`
     collapse `PaneClose` uses) and report whether the source pane / workspace
     was removed.
  `MoveSurfaceOutcome { dst_workspace, src_workspace, src_pane_removed,
  src_workspace_removed }`.
- `move_surface_to_workspace(src_pane, surface, dst_workspace) -> Option<MoveSurfaceOutcome>`
  Convenience: `dst_pane` = first leaf of `dst_workspace`, index = end. Used by
  the "Move" menu and by a drop directly on a sidebar row.

Async unit tests cover same-/cross-workspace, end vs index, source-pane
collapse, source-workspace removal, missing ids.

### Layer 3 — GUI live widget move (`PaneRegistry` + `window.rs`)

New `PaneRegistry::detach_surface_for_move(src_pane, surface) -> Option<MovingSurface>`
— like `take_surface_for_tearoff` but **keeps the live handle**: it clones the
`PaneTerminal`/`BrowserPane` out before removing map entries and returns it in
`MovingSurface { content, focus, handle: MovingHandle::{Terminal|Browser}, title }`.

New `PaneRegistry::attach_moved_surface(dst_pane, dst_workspace, surface,
moving, tab_widget, label)` — mounts `content` into the dst stack under the
surface id, registers the handle under the dst pane, records
`surface_workspace`, inserts the tab widget into `surface_tabs`, activates.

`window.rs` gains two `GtkCommand`s + dispatch arms:

- `MoveSurfaceToPane { src_pane, surface, dst_pane, target_index, ack }`
- `MoveSurfaceToWorkspace { src_pane, surface, dst_workspace, ack }`

Dispatch (GTK thread):
1. `detach_surface_for_move` (live handle out).
2. `store.move_surface_to_pane/_to_workspace` (model update).
3. Rebuild the `PaneSurface` from the store, `build_surface_tab_widget` wired to
   the **dst** pane, `attach_moved_surface`, then reorder widget to
   `target_index`, activate + focus.
4. If `src_pane_removed` → existing close-pane incremental/rerender + focus.
   If `src_workspace_removed` → `drop_workspace`.
5. If the dst workspace/pane is **not currently rendered** (offscreen), fall
   back to `rerender_workspace(dst)` after the model update (still
   state-preserving for the moved widget because `build_panel` reuses the
   registered live handle by surface id).

### Layer 4 — drop targets

- **Tab→tab cross-pane:** in the existing per-tab drop handler, when
  `src_pane != target_pane`, send `MoveSurfaceToPane` instead of rejecting.
- **Tab→pane body (append end):** add a `DropTargetAsync` on each pane frame
  (`pane_frames`) accepting the tab MIME; on drop send `MoveSurfaceToPane` with
  `target_index = usize::MAX` (clamped to end).
- **Tab→sidebar workspace row:** add a `DropTarget` on each sidebar row
  accepting the tab MIME. `connect_enter`/motion → send `ActivateWorkspace`
  (selects the workspace mid-drag, requirement 3). A release **on the row**
  sends `MoveSurfaceToWorkspace` (drop directly on a workspace = append to its
  first pane). Continuing into the now-visible pane is handled by the pane/tab
  targets above.

### Layer 5 — right-click "Move" submenu

In `attach_tab_context_menu`, add a **Move** button. On click, query workspaces
live via a new callback `list_workspaces: Rc<dyn Fn() -> Vec<(WorkspaceId, String)>>`
(backed by the sidebar's known rows/names, kept in an `Rc<RefCell<…>>` the
sidebar updates on every `upsert`). Show a nested `Popover` with one flat button
per workspace (current workspace omitted/disabled) → `MoveSurfaceToWorkspace`.

### Layer 6 — workspace selection lands on last tab (requirement 7)

In `activate_workspace`: after setting the visible child, for the first `Surface`
of the workspace find its first leaf, set that leaf's `active` to its **last**
surface (store + `activate_surface` widget), then focus it.

## State preservation rationale

The terminal's PTY + `Vt` and the browser's `WebView` live inside the
`PaneTerminal`/`BrowserPane` handle and the single content widget. Moves
**re-parent that exact widget and re-register that exact handle** — nothing is
rebuilt — so shells/scrollback/navigation are untouched. This is the same
mechanism tear-off already proves out.

## Testing

- `flowmux-core`: model move unit tests (headless).
- `flowmux-daemon`: store move async tests (headless).
- `flowmux` GUI: `#[gtk::test]` mirroring `take_surface_for_tearoff_moves_widget…`
  — assert `detach_surface_for_move` keeps the handle and `attach_moved_surface`
  re-homes the same widget instance under the dst pane.
- Manual smoke after `cargo build --release` + local install: drag across panes,
  between tabs, onto a sidebar workspace, out of the window; "Move" menu; verify
  a running `vi`/shell keeps its state across a move.

## Out of scope / YAGNI

- No new IPC verbs (`flowmuxctl`) for move — UI-only feature.
- No multi-tab / multi-select drag.
- No drag of browser tabs into a terminal-only pane restriction (panes already
  hold mixed tabs).
