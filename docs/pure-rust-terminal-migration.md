<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
# Pure-Rust terminal migration (drop VTE + Flatpak)

Branch: `pure-rust-terminal`. Goal: replace the VTE GTK4 widget with a
self-owned terminal built on a pure-Rust VT engine + a custom GTK4
renderer, remove the VTE dependency and the Flatpak packaging, keep the
WebKitGTK browser working, and preserve **every** existing behavior
across Ubuntu 22.04 / 24.04 / 26.04.

## Why

Every chronic terminal bug is a VTE *widget* behavior fought with
workarounds: drag-select dies on repaint (needs a patched VTE bundle),
the Hangul/IME preedit class (4 separate fixes), the 22.04 IBus nav-key
drop. VTE is also the most distro-fragile dependency: `libvte-2.91-gtk4`
is absent on 22.04, overridden by a patched build elsewhere, and a plain
`cargo build` silently relinks the system VTE and regresses drag-select.

A self-compiled pure-Rust VT engine makes terminal behavior **identical
on every distro** (statically linked) and deletes the libvte dependency
from the matrix entirely.

## ⚠️ The rollback lesson (read first)

This migration was **already attempted and reverted**. History:

- `557ffb2 refactor(terminal): remove VTE dependency` → libghostty-vt +
  custom `GskRenderNode` renderer (peaked at `terminal_pane.rs` 2863 LOC
  + `terminal_surface.rs` 114 LOC, recoverable from git).
- A full series followed: IME input, key encoding map, CJK glyph
  baseline alignment, wide-cell overpaint fix, scrollbar + smart paging,
  TextNode batching, rpath embedding, Zig CI.
- `21b8ea9 refactor(terminal): roll back to VTE and drop libghostty` —
  **reverted the whole thing.**

Rollback cause, verbatim from the commit: the custom renderer *"traded
VTE's mature pipeline (glyph atlas, dirty-row tracking, optimized cairo
path)"* for engine correctness, and on older hosts (the 22.04 Flatpak
target) *"the missing optimizations showed up as visible lag in
alt-screen TUIs like tig and opencode"*, and *"the cell-width / advance
pairing across renderer revisions never settled cleanly."*

**Conclusion: the engine was never the problem. The renderer was.** The
VT engine choice (libghostty / wezterm / alacritty) is low-risk — all
three are mature. The high-risk, must-get-right component is the GTK4
renderer. The previous attempt failed because it rebuilt the full
`O(rows × cols)` node tree every frame with no glyph atlas and no
dirty-row tracking. Any new attempt that does not fix that fails the
same way on the same 22.04 target.

### Non-negotiable renderer requirements (the actual hard problem)

1. **Glyph atlas / cache** — shape + rasterize each glyph once, reuse.
   Never re-shape per frame. (GSK's GL renderer keeps a texture atlas
   for `TextNode`s; the trap last time was rebuilding the *node tree*
   every frame, not the atlas.)
2. **Dirty-row tracking** — cache per-row render nodes; rebuild only
   rows the engine marked dirty. Reuse cached nodes for clean rows.
3. **Stable cell geometry** — fixed monospace advance, CJK = 2 cells,
   deterministic fractional-advance handling. This is what "never
   settled cleanly" before.
4. **Validate perf on the weakest target first** — a 22.04-class host
   running `tig` / `opencode` alt-screen scroll must be smooth *before*
   VTE is removed. This is the gate, not an afterthought.

## Engine choice

User directive: **pure Rust**. libghostty-vt is Zig (C ABI) and adds a
Zig toolchain to CI on all three distros — rejected on the
"high compatibility / minimal toolchain" constraint.

`wezterm-term` is **not published on crates.io** (only a third-party
`tattoy-wezterm-term` fork), so it would mean a git/forked dependency —
rejected. Chosen engine: **`alacritty_terminal` 0.26** (Apache-2.0,
GPL-3.0-compatible, purpose-built as a library, used by egui_term /
dioxus-terminal / shadow-terminal). Decisive extra: it ships **built-in
damage / dirty-line tracking** (`TermDamage`) — the exact renderer
requirement (#2 above) whose absence caused the previous rollback. PTY
via `portable-pty` 0.9 (or alacritty's own `tty` module). Renderer: GTK4
`gtk::Snapshot` / `GskRenderNode` with the caching above (reuse the
recoverable old renderer structure, fix its perf cause), driven off
`TermDamage` so only changed lines rebuild.

Input / IME / key-encoding logic from the old libghostty
`terminal_pane.rs` (2863 LOC at `21b8ea9^`) is **recoverable and
reusable** — it already solved IME input, the key-encoding map, and CJK
alignment against a non-VTE backend. Port that logic; write the renderer
fresh.

## WebKit browser — unaffected

The WebKitGTK 6.0 WebView is a separate GTK4 widget. Swapping VTE for our
own render widget does not touch the browser pane; both coexist in the
same GTK4 app. On 22.04 the Flatpak bundles GTK4 + WebKit; once VTE is
gone the Flatpak manifest only loses its VTE build module. (Flatpak is
being removed entirely per the goal — see Sequencing.)

## Parity checklist (derived from commit history)

Nothing below may regress. Each item cites the commit(s) that introduced
or fixed it.

### 1. Keybindings — config + per-binding behavior
- User-customizable shortcuts via `options.json` (`5edc596`); re-install
  accelerators on options OK (`9228952`); defer install until GTK init
  (`6488872`).
- Defaults to pin (from `keybindings.rs` tests):
  - `Ctrl+N` new workspace; `Ctrl+Shift+N` new window (`9713021`).
  - `Ctrl+Shift+T` new surface (tab); `Ctrl+Shift+B` new browser tab.
  - `Ctrl+Tab` next workspace; must NOT claim `Shift+Tab`.
  - `Ctrl+Shift+Left/Right` prev/next tab (`c269a90`).
  - `Ctrl+Shift+PageUp/Down` splits; `Alt+arrows` directional focus.
  - `Alt+W` close pane (`eec1441` sibling-focus after close);
    `Ctrl+Shift+W` whole-window quit w/ confirm dialog (`8f689e3`).
  - `Alt+1..Alt+8` one-shot tab/workspace select.
  - `Ctrl+Shift+K` copy focused pane path (`80d80a9`).
  - copy/paste excluded from rebinding (`3678774`); tab icons option
    (`a226a2f`).

### 2. Pane / focus / tabs / workspace
- Pane split tree; directional focus move (Alt+arrows); focus border +
  active-tab + selected-workspace styling (`cec6526`, `fc182cd`,
  `221f9f6` CSS tests).
- Suppress libadwaita selected-row tint on sidebar (`5bbd8a6`);
  workspace text full-row width overlapping close button (`fffec6d`).
- Tab tear-off into live windows: drag end opens window (`47b1613`),
  no-target opens window (`d67699f`), wrap in flowmux chrome (`6365a95`),
  private drag payload (`fc5dc59`).
- Split seeds new pane cwd from focused tab, browser-aware fallback
  (`0f0dc5a`).
- Right-click "Copy path" / "Copy URL" (`957eb71`); "Show in folder"
  (`d0d7164`).
- Scrollbar overlay + smart PgUp/PgDn paging (`6e4017f`, `c4d20de`);
  own the scrollbar adjustment, force-show on 22.04 (`b102a8a`);
  snap viewport to bottom on PTY input (`9e12edb`); scroll-on-keystroke
  snap-back (in `21b8ea9` theme change).
- Empty-state "FlowMux / No workspaces yet" (`9ecc0d5`).

### 3. Terminal UX — color / font / background
- Per-terminal font family + size overrides (`1f19eae`).
- Palette / cursor / font / scrollback / no audible bell applied to the
  widget; Ghostty-default palette as built-in fallback (`44ae617`).
- Ghostty config parsing for theme reuse (`flowmux-config/ghostty.rs`).
- Background color, cursor style — must match current VTE config output.
- URL hyperlink highlighting (regex match) — needs an in-house URL
  detector now (was VTE's `Regex` API).

### 4. AI-agent key input (claude / codex / opencode)
- DECCKM-aware / application-cursor-key encoding (`fe2602b` unify key
  dispatch; `d0e9bea` ghostty key encoding map test).
- Preserve cursor modes through pty tee (`e4d038f`).
- Forward wheel to mouse-tracking apps (`b191bce`); mouse tracking
  reporting modes.
- Login shell spawn (`3931da0`); TERM/COLORTERM forwarding (`0e2fa01`).
- OSC 7 cwd + OSC 2 title tracking.
- Bracketed paste.

### 5. Hangul / IME input + key combos
- Show CJK preedit by forcing the ibus immodule (`f5c1b7e`); force ibus
  GTK4 immodule when env unset (`e68b1e3`, WSL).
- Render inline Hangul preedit while the app hides the cursor — repaint
  on keystroke (`e33b0e7`). *(Becomes natural: we own preedit drawing.)*
- `IBUS_ENABLE_SYNC_MODE` so Enter commits preedit first (`bad2d61`);
  commit composing syllable before Enter's newline (`6378666`);
  order Shift+Enter newline behind composing syllable (`79d703c`).
- Keep BackSpace/Delete on IM path so jamo decompose works (`2ca5062`).
- 22.04 Flatpak IBus nav-key bypass (`9417dad`, `1823df1`, `e4111cd`,
  `a2664f0` flush preedit before bypass, `6c62d69` ShortcutController).
  *(Flatpak is being removed; keep the native-22.04 path if still
  relevant, drop the Flatpak-specific bypass.)*
- Shift+Enter → ESC+CR translation.
- **Most of this class collapses** once flowmux owns the preedit
  rendering and key path instead of fighting VTE's IM hooks. Re-verify,
  don't blindly port the workarounds.

### 6. Notifications
- OSC 9 / 99 / 777 parsing lives in `flowmux-notify` (`osc.rs`) — widget
  independent. Snooped via `flowmuxctl pty-tee` proxy (`da8c05f`) — also
  widget independent, preserved for free.
- D-Bus delivery via `org.gtk.Notifications` keeping dock count alive
  (`363b987`); dock badge synced to unread count (`25eee54`, `cd4f568`,
  `ee0cb26`); per-row trash (`a127fe9`) + "All Clear" (`ea4aae8`).
- AttentionNeeded / Error pierce focus suppression (`7a62c5f`).
- System-notification toggle in options (`9c390d8`).
- Pane resolution from active tab title when hook arrives context-less
  (`53b9182`).

### Also present (don't lose)
- Push-to-talk voice input / ASR (`47dd7b9` + hardening) — independent of
  terminal widget, verify untouched.
- Web Inspector popup (`826c7f7`) — browser, untouched.

## Sequencing (app must never be left broken)

1. **Phase 1 — engine + PTY.** Add `alacritty_terminal` + `portable-pty` to
   `flowmux-terminal`; real `TerminalBackend` that spawns a PTY, pumps
   bytes through the parser, exposes a dirty-tracked grid + scrollback.
   Unit-test against the existing key-encoding map test.
2. **Phase 2 — renderer.** GTK4 widget subclass rendering the grid via
   `Snapshot`/`GskRenderNode` with **per-row node cache + dirty rows +
   glyph atlas + stable advance**. Cursor, selection (own state, no
   deselect-on-output), wide/CJK cells.
3. **Phase 3 — perf gate.** `tig` / `opencode` alt-screen scroll on a
   22.04-class host must be smooth. **Do not proceed past this gate
   until met** — this is the exact failure of the last attempt.
4. **Phase 4 — input/IME.** Port key dispatch + IME from old libghostty
   `terminal_pane.rs`; re-verify the Hangul cases (preedit render,
   Enter/Shift+Enter ordering, jamo decompose) on the new path.
5. **Phase 5 — feature parity.** Walk the checklist above; wire
   selection/copy, URL detection, scrollbar/smart-paging, OSC 7/2,
   mouse tracking, fonts/palette from theme.
6. **Phase 6 — remove VTE.** Drop `vte4` dep, `scripts/build-vte.sh`,
   `scripts/install-host.sh`, `packaging/vte-patches/`, the
   `libvte-2.91-gtk4` runtime dep in `cargo-deb.toml`, and the VTE
   memory of `cargo build` regression. `cargo check --workspace` clean.
7. **Phase 7 — remove Flatpak.** Delete `packaging/flatpak/`,
   Flatpak-specific code paths (flatpak-spawn shell bridge, Flatpak IBus
   bypass), CI Flatpak steps.
8. **Phase 8 — README + docs.** Rewrite build prerequisites (no
   libgtk-vte, no Zig, no Flatpak), the 22.04 section, and the patched-
   VTE section. Update CLAUDE.md (its libghostty/zig claims are already
   stale and must reflect the alacritty_terminal reality).

## Implemented on `pure-rust-terminal` so far

All headless-tested or compile-verified; **none wired into `window.rs`
yet** (VTE pane still live, app unbroken):

- `flowmux-terminal/src/engine.rs` — `TermEngine`: PTY + parser + grid +
  damage + event sink + selection API. Headless test spawns a shell and
  reads its output from the grid.
- `flowmux-terminal/src/render.rs` — `FrameSnapshot` + `ThemePalette`;
  color resolution (named/indexed/spec, INVERSE/DIM, OSC-4-aware,
  theme-fallback, 256-cube), selection membership. 8 unit tests.
- `flowmux/src/ui/terminal_render.rs` — `TerminalRenderArea` widget:
  per-row node cache (only changed rows rebuild), cell-locked geometry
  (narrow runs + wide CJK), bg batching, bold/italic/underline/strikeout,
  cursor (block/beam/underline, themed), selection highlight.
- `flowmux/src/ui/terminal_pane_native.rs` — wires engine ↔ renderer ↔
  input ↔ event loop; resize → grid/PTY; theme palette + font; **drag
  selection + copy + paste** (survives repaint by construction).
- `theme.rs::native_palette()` — `ResolvedTheme` → `ThemePalette`.

### Swap + removal landed

- `terminal_pane_native.rs` is now a drop-in for the old VTE pane
  (`spawn(PaneCallbacks)`, container+scrollbar, focus, right-click menu,
  bell/exit/title callbacks, theme/font, drag-select+copy+paste,
  app-cursor-aware key encoding, OSC 9/99/777 via the pty-tee argv wrap).
- `PaneRegistry.terminals` holds `TerminalPaneNative`; `workspace_view`,
  `window`, `keybindings`, `theme` all use it. **VTE pane deleted**
  (`terminal_pane.rs` removed; shared bits → `pane_common.rs`).
- `vte4`/libvte removed from `Cargo.toml`, CI apt, and cargo-deb
  depends. `scripts/build-vte.sh` + `packaging/vte-patches/` deleted;
  `install-host.sh` is a plain release build. Only the pure-Rust `vte`
  *parser* (via alacritty) remains — no GTK VTE widget.
- `packaging/flatpak/` deleted; README 22.04 → backports PPA;
  CLAUDE.md/README architecture text corrected.

### IME/Hangul — implemented (needs GUI verification)

`terminal_pane_native::wire_keyboard` sets an `IMMulticontext` on the key
controller (`set_im_context`): `commit` writes finished text to the PTY,
`preedit_changed` shows the composing string inline at the caret via
`TerminalRenderArea::set_preedit` (drawn even when the app hid the cursor
— `FrameSnapshot::caret` is always populated), `preedit_end` clears it,
and focus enter/leave drive `focus_in`/`focus_out`. Commit is synchronous,
so an Enter that finalises a syllable writes the text before the newline.
Hangul real-time typing / Enter / Shift+Enter still need a GUI pass.

### Runtime verification done (this env has DISPLAY=:1 + dbus-run-session)

The full GUI test suite runs at runtime, not just compiles:
`dbus-run-session -- cargo test -p flowmux` → **118 pass**, and
`cargo test -p flowmux-terminal` → **22 pass**. This exercises pane
structure, splits, directional focus, workspace activation/ops, tab
title logic, close/tear-off behavior on the live GTK stack.

Three regressions the runtime run caught and fixed (main/VTE passed 129
here, so these were swap-introduced):

1. Reader thread was detached → use-after-free on `Term` at teardown.
   Fix: store the `EventLoop::spawn` handle, join on `Drop`.
2. Unit tests forked a real shell per pane (VTE's async spawn never
   forked in tests) → process/thread explosion. Fix:
   `TermEngine::stub` (no PTY/thread) under `cfg!(test)`.
3. `IMMulticontext::set_client_widget` teardown SIGSEGV'd when panes
   were destroyed. Fix: skip IM client wiring under `cfg!(test)`, and in
   real builds detach via `connect_unrealize → set_client_widget(None)`.

**Visual render verified headlessly.** `terminal_render::render_image_tests
::render_hangul_and_colors_to_png` renders a Hangul + ANSI-color frame
through the *real* per-row renderer to a PNG via an offscreen
`gsk::CairoRenderer` (no window/WM/user-session). Confirmed correct:
Hangul + CJK glyphs, ANSI fg/bg colors, bold, box-drawing, cursor, dark
theme. Known minor: wide-cell (Hangul) advance is slightly loose vs the
glyph's natural width — a monospace-CJK font-tuning nit, not a defect.

Still needs a **human at a GUI** (cannot be checked headlessly): IME
*composition* path (typing jamo through ibus → preedit → commit), perf
feel on a weak 22.04 host, live agent key interaction, notification
popups appearing.

### Feature-complete (all implemented + compile/runtime tested)

- Wheel scrollback + alt-screen wheel-forward (cursor keys to TUIs).
- Bracketed-paste wrap (DECSET 2004).
- Mouse-tracking report to apps (SGR + legacy X10), with local
  selection/right-click-menu suppressed while the app holds the mouse.
- URL Ctrl-click → `on_open_url` (`url_at` unit-tested).

Also done: scrollbar *thumb* bound to the scrollback offset (updates on
output/scroll, draggable), and Shift+PgUp/PgDn smart paging of the
scrollback.

### No codeable items remain

Every behavior in the parity checklist is implemented and compile/runtime
tested. The only outstanding work is **human GUI observation** of live
behavior — Hangul IME composition through ibus, agent keystroke response,
notification popups appearing, and perf *feel* on a weak 22.04 host —
which a headless agent cannot perform. See `VERIFICATION.md`.
  (engine `display_offset` scroll API).
- URL detection + Ctrl-click (`on_open_url` is wired in callbacks but the
  native pane does not yet detect links).
- Mouse-tracking forward to apps (report modes), bracketed paste wrap.
- **Human GUI verification on 22.04/24.04/26.04**: Hangul typing, agent
  keys (claude/codex), drag-select, perf on a weak host, notifications,
  every keybinding/tab/workspace behavior.
- Optional: strip inert `is_flatpak_sandbox()` branches from
  paths/hook_install/show_in_folder/main.

## Open risks
- Renderer perf on 22.04 (the rollback cause) — gated in Phase 3.
- IME parity on a hand-rolled key path — gated in Phase 4.
- 22.04 native is **not feasible** and Flatpak is removed, so **22.04 is
  unsupported**; target is 24.04+. Evidence: gtk4-rs 0.9's lowest feature
  is `v4_10` (needs GTK ≥ 4.10) while jammy ships GTK 4.6, and jammy has no
  GTK4 WebKit (only GTK3 webkit2gtk 4.0/4.1). Supporting jammy would mean
  a 2-major gtk4-rs downgrade + UI rewrite + dropping the browser — out of
  scope. 24.04 / 26.04 use native packages.
