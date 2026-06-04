<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
# Pure-Rust terminal — manual verification checklist

The `pure-rust-terminal` migration is code-complete and passes 119 GUI +
22 engine tests and an offscreen render check (see
`pure-rust-terminal-migration.md`). The items below need a human at the
GUI — they involve live IME composition, popup delivery, agent keystroke
response, and perf *feel*, none of which a headless agent can observe.

Each row maps a goal requirement to the commit(s) that defined the
behavior and what to look for. Build + run first:

```bash
cd <repo>            # branch: pure-rust-terminal
scripts/install-host.sh
# fully quit any running flowmux, then relaunch
```

## 1. Keybindings (단축키)
- [ ] `Ctrl+N` new workspace, `Ctrl+Shift+N` new window (`9713021`)
- [ ] `Ctrl+Shift+T` new tab, `Ctrl+Shift+B` new browser tab
- [ ] `Ctrl+Tab` next workspace; `Ctrl+Shift+Left/Right` prev/next tab (`c269a90`)
- [ ] `Ctrl+Shift+PageUp/Down` split; `Alt+arrows` directional focus
- [ ] `Alt+W` close pane (focus moves to sibling, `eec1441`)
- [ ] `Ctrl+Shift+W` whole-window quit with confirm dialog (`8f689e3`)
- [ ] `Alt+1..8` select tab/workspace; `Ctrl+Shift+K` copy pane path (`80d80a9`)
- [ ] `Shift+Enter` inserts newline without submitting (`ALT_ENTER_BYTES`)
- [ ] Custom bindings from `options.json` apply (`5edc596`)

## 2. Pane / focus / tabs / workspace (구조·포커스·탭·workspace)
- [ ] Split tree renders; focus border on active pane (`cec6526`)
- [ ] Directional focus (Alt+arrows) stays within the workspace
- [ ] Tab bar: add / activate / close / rename / reorder by drag
- [ ] Tab list scrolls when many tabs
- [ ] Drag a tab out → new window (`a4fe6ec`, `6365a95`)
- [ ] Right-click pane: Split Right / Split Down / Copy path / Close Pane
- [ ] Right-click "Show in folder" / "Copy path" / "Copy URL" (`d0d7164`, `957eb71`)
- [ ] Sidebar workspace select/hover styling (`fc182cd`, `5bbd8a6`)
- [ ] Split survives close of sibling (running shell/agent not killed)

## 3. Terminal UX — color / font / background (컬러·폰트·배경)
- [ ] Theme palette (16 ANSI), fg/bg, cursor color match the prior build
- [ ] Per-terminal font family + size override (`1f19eae`)
- [ ] Global zoom (`Ctrl++`/`Ctrl+-`) scales the font
- [ ] App-set colors via OSC 4 take effect (e.g. a themed `vim`)
- [ ] URL hover + `Ctrl+click` opens it in a browser tab

## 4. AI-agent key input (claude / codex)
- [ ] Run `claude` / `codex`; arrow keys, Enter, Tab, Esc behave
- [ ] Application-cursor apps (`vim`, `tig`) get correct arrow encoding
- [ ] Mouse works in `htop` / `vim` (click + wheel) — app mouse mode
- [ ] Wheel scrolls scrollback on the normal screen; drives the app on alt-screen
- [ ] Paste into an editor is bracketed (no accidental command execution)

## 5. Hangul / IME (한글 실시간 입력)
- [ ] Switch to Korean IME; compose 안녕하세요 — preedit shows inline
- [ ] Preedit visible even when the app hides the cursor (Claude prompt) (`e33b0e7`)
- [ ] Enter after a composing syllable submits "…요\n", not "…세\n요" (`6378666`, `79d703c`)
- [ ] `Shift+Enter` newline ordered after the composing syllable
- [ ] BackSpace decomposes a jamo while composing (`2ca5062`)
- [ ] CJK (日本語 / 中文) composes and renders

## 6. Notifications / alarms (알람)
- [ ] An agent emitting OSC 9 / 99 / 777 raises a desktop notification
- [ ] Bell popover / dock badge updates with unread count (`25eee54`)
- [ ] "All Clear" + per-row trash on the bell popover (`ea4aae8`, `a127fe9`)
- [ ] AttentionNeeded / Error pierce focus suppression (`7a62c5f`)
- [ ] System-notification toggle in options honored (`9c390d8`)

## 7. Performance (perf — the prior rollback's failure point)
- [ ] `tig`, `opencode`, `htop` alt-screen scroll is smooth (no visible lag),
      especially on a 22.04-class / weak GPU host
- [ ] Fast `cat large_file` / `yes` does not stutter the UI

## 8. Compatibility (호환성)
- [ ] 24.04 / 26.04: native build runs
- [ ] 22.04: installs + runs via Flatpak (GNOME 48 runtime)
- [ ] Browser pane (WebKitGTK) still loads pages (native and Flatpak)

> If anything here misbehaves, report it — the fix is a code change I can
> make; only the observation needed a human.
