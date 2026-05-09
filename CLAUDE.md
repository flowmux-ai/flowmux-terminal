<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# flowmux Project Rules

This document records the conventions to follow when working on flowmux.

## Terminology

Use the terms below in all human-readable text: UI labels, notifications,
errors, tooltips, commit messages, README text, and user-facing discussion.
Keep code identifiers in their existing English form.

| Term | Meaning | Matching Code Identifier |
|---|---|---|
| side panel | The full left-side panel area | `Sidebar` |
| workspace | Each side-panel item and unit of work | `Workspace` |
| workspace name | The name shown for each side-panel item | `Workspace.name` |
| pane | A split window inside the main content area | `Pane` |
| tab | A terminal shown inside a pane | `PaneSurface` (`SurfaceKind::Terminal`) |
| browser tab | A browser shown inside a pane at the same level as a tab | `PaneSurface` (`SurfaceKind::Browser`) |
| tab name | The name shown in the pane tab bar | `PaneSurface.title` |

### Rules

- Use these terms consistently in user-facing text.
- Keep existing English code identifiers. Only rename identifiers after an
  explicit decision.
- In comments, use the terminology above for behavior descriptions and the
  exact identifier name when referring to a concrete type, field, or function.
- Do not mix multiple terms for the same concept in one document or view.
