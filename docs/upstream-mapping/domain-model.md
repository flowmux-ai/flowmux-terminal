# Domain model

## What cmux does (from public docs)

- A "workspace" is a top-level container shown as a row in the sidebar,
  with a working directory, git branch + linked PR, listening ports,
  and the latest unread notification body.
- Each workspace contains one or more "surfaces" (tabs across the top
  of the workspace). A surface is either a terminal or a browser.
- A surface is a recursive binary split tree of "panes". Splits can be
  horizontal (`⌘⇧D`) or vertical (`⌘D`). Leaf panes hold either a PTY
  child or a browser view.
- Panes can be focused directionally (`⌥⌘ ←→↑↓`).

## What flowmux does

- Same data model, expressed in `flowmux-core::{Workspace, Surface, Pane}`.
  `Pane` is `Leaf | Split{direction, ratio, first, second}`; the leaf
  holds either a terminal PID or a browser URL.
- Persistence: `$XDG_DATA_HOME/flowmux/state.json` is the single source
  of truth for workspaces; the GUI rebuilds widgets from it on launch.
  (Implementation detail; ports cleanly across cmux version bumps.)
- IDs are UUIDv4 newtypes (`WorkspaceId`, `SurfaceId`, `PaneId`,
  `NotificationId`) so log output, IPC envelopes, and persistence all
  share one ID space.

## Crates touched

- `flowmux-core` — types
- `flowmux` — widget tree mirrors the pane tree
- `flowmux-ipc` — addresses panes/surfaces/workspaces by id

## Open questions / risks

- cmux's exact split-ratio rounding behavior on resize isn't documented;
  may need to match by observation when terminal rendering lands.
