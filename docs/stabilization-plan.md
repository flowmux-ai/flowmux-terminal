<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# FlowMux stabilization plan

This fork tracks upstream `flowmux-ai/flowmux-terminal`. Development starts at
v0.7.0; v0.6.4 remains the installed regression baseline.

## Safety boundary

- Never test against a user's active FlowMux socket or persisted state.
- Use isolated `XDG_STATE_HOME`, `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and runtime directories.
- Preserve layout/scrollback semantics without representing a restored terminal as live.
- Do not install a development build until headless tests and isolated runtime tests pass.

## Phases

1. **Deterministic foundation:** add regression tests and document isolated runtime fixtures.
2. **Close/process lifecycle (P0):** bounded, non-freezing PTY shutdown and child reaping; verify children that ignore SIGHUP cannot hang pane/workspace close.
3. **Shift+Enter (P0):** emit Kitty CSI-u Shift+Enter when requested by the foreground application, retaining the legacy fallback otherwise.
4. **Restart restoration (P1):** distinguish restored layout/scrollback from live PTYs and automatically create or reconnect a usable terminal without dead panes.
5. **Move/title semantics (P1):** state-machine coverage for moving tabs/panes; title updates must not activate or create workspaces. Isolate OSC title changes from notification actions.
6. **Acceptance:** full checks, isolated GUI/runtime scenarios, two independent model-family reviews, then a controlled local install and migration/cleanup of old persisted workspaces.

## Initial evidence

- v0.6.4 and v0.7.0 hardcode Shift+Enter as `ESC CR` while Pi expects Kitty `ESC[13;2u`.
- `Pty::drop` sends SIGHUP and performs an unbounded blocking `waitpid`, which can freeze the GTK main thread.
- v0.7.0 already contains defensive state-store logic for tab moves; it needs scenario-level verification.
- Current title-update code does not directly activate a workspace; the observed symptom may involve notification/agent-status routing and needs isolated reproduction.
