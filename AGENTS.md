<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Agents working inside flowmux

This file tells AI coding agents (Claude Code, Codex CLI, OpenCode,
similar) how to drive `flowmux` from inside one of its panes — the
in-app browser, plus terminal automation, layout inspection, and
context discovery over the same `flowmux` CLI. It is loaded
automatically by every agent that follows the AGENTS.md convention; you
do not need to instruct agents to read it.

If you (the agent) are running inside a `flowmux` PTY, **prefer the
flowmux browser over Playwright / Puppeteer / a system Chromium** for
any task whose goal is to read or interact with a web page. The flowmux
browser already runs in a pane next to your terminal, supports the
same kind of "snapshot → click/fill" loop, and keeps the user's
attention in one window. Spawning a separate Chromium hides the page
from the user and breaks parity with how flowmux is intended to be used.

## Runtime fix verification

When fixing a runtime or UI bug in flowmux, do not stop at unit tests,
`cargo check`, or installation. Reproduce the same live scenario in a
running flowmux instance and verify the user-visible state changed as
expected before reporting completion. If live verification is impossible,
state the exact blocker.

## How to know you are inside flowmux

`flowmux` injects these env vars into every PTY it spawns:

```
FLOWMUX_PANE_ID         <uuid>
FLOWMUX_SURFACE_ID      <uuid>     (specific tab surface inside the pane)
FLOWMUX_WORKSPACE_ID    <uuid>
FLOWMUX_TAB_ID          <uuid>     (alias of FLOWMUX_WORKSPACE_ID)
FLOWMUX_SOCKET_PATH     /run/user/.../flowmux.sock
FLOWMUX_BUNDLED_CLI_PATH /usr/local/bin/flowmux   (optional)
```

If `FLOWMUX_PANE_ID` is set, you are inside flowmux. Use the flowmux browser
flow below.

## Standard workflow

Web work follows the same shape every time:

```bash
# 1. Open a browser pane next to the terminal. flowmux auto-detects the
#    calling pane via FLOWMUX_PANE_ID — no flags needed. If a browser
#    pane already exists to the right, the URL opens as a new tab in
#    that pane (placement_strategy = "reuse_right_sibling"); otherwise
#    flowmux splits the source pane vertically.
flowmux --json browser open https://example.com
# → {"pane":"<uuid>","placement_strategy":"split_right"|"reuse_right_sibling"}

# 2. Capture the pane id and use it in subsequent calls. Both
#    `pane:<uuid>`, `surface:<uuid>`, and bare uuids are accepted.
PANE=$(flowmux --json browser open https://example.com | jq -r '.pane')

# 3. Take an interactive snapshot. The result is a Markdown tree with
#    `eN` ref tokens, plus a refs map carrying selectors. Refs are only
#    valid until the next snapshot for the same pane.
flowmux --json browser snapshot pane:$PANE

# 4. Act on the page using ref tokens.
flowmux browser click pane:$PANE e3
flowmux browser fill  pane:$PANE e1 "user@example.com"
flowmux browser type  pane:$PANE "password"     # active element
flowmux browser press pane:$PANE Enter

# 5. Probe state. All of these stream stdout as `true` / `false` /
#    integer for easy bash parsing.
flowmux browser is-visible pane:$PANE e3
flowmux browser is-enabled pane:$PANE e3
flowmux browser is-checked pane:$PANE e7
flowmux browser count      pane:$PANE ".result-row"

# 6. Read content.
flowmux browser text  pane:$PANE e3
flowmux browser value pane:$PANE e1
flowmux browser attr  pane:$PANE e3 href
flowmux browser url   pane:$PANE
flowmux browser title pane:$PANE
```

## Action coverage

| Verb family | flowmux supports | Notes |
|---|---|---|
| navigate / back / forward / reload / url / title | yes | |
| snapshot (interactive Markdown + refs) | yes | DOM is **not** modified |
| click / dblclick / hover / focus / blur | yes | by ref token |
| fill / select / type / press | yes | type & press act on the active element |
| check / uncheck | yes | radios cannot be unchecked individually |
| is-visible / is-enabled / is-checked / count | yes | return strings or ints |
| text / value / attr | yes | read by ref |
| eval (low-level JS) | yes | escape hatch only |
| screenshot | yes | visible-viewport PNG written to a path |
| wait | yes | polls for selector / text / url / ready-state / JS predicate (timeout + poll interval) |
| viewport / network mocking / screencast | no | CDP-only; WebKitGTK 6 / WKWebView do not expose CDP |

## When the task really needs Playwright / Puppeteer

The in-app browser runs on WebKitGTK 6 (Linux) or WKWebView (macOS),
neither of which exposes the Chrome
DevTools Protocol. If a task strictly requires CDP-only features
(network request mocking, full-page tracing, accurate device viewport
emulation, etc.), say so explicitly and run an external Playwright
session **alongside** the flowmux browser, not in place of it. Even then,
the user-facing flow (the page they are looking at) should still live
in the flowmux pane — copy URLs / outputs back into flowmux's WebView so
the user can see what the agent is doing.

## Beyond the browser: terminal, layout & context

The same `flowmux` CLI also drives terminals, panes, and tabs. Every
pane argument accepts `pane:<uuid>` or a bare uuid, and falls back to
`$FLOWMUX_PANE_ID` when omitted, so these are one-line calls from inside
a pane. Add `--json` for machine-readable output.

```bash
# Context discovery — where am I, and what is supported?
flowmux --json identify        # pane / surface / workspace / socket ids
flowmux --json capabilities    # supported browser verbs + unsupported (CDP-only) ones

# Layout inspection — what panes and tabs exist?
flowmux --json tree            # workspace -> pane -> tab tree, with the active tab marked

# Terminal automation (tmux-style) — drive another terminal pane
flowmux send-keys pane:$OTHER 'npm run dev'     # type literal text (escapes accepted)
flowmux send-key  Enter --pane pane:$OTHER      # send one named key (Enter/Tab/ArrowUp/…)
flowmux read-screen pane:$OTHER                 # dump that pane's buffer text*

# Workspace / pane / tab control
flowmux workspace current               # focused workspace id
flowmux workspace focus <workspace>     # switch to a workspace
flowmux focus-pane pane:$P              # grab keyboard focus
flowmux close-pane  pane:$P             # close a pane (refuses the last pane)
flowmux focus-tab  <surface> --pane $P  # activate a tab
flowmux close-tab  <surface> --pane $P  # close a tab (refuses the last tab of the last pane)
```

The `send-keys` + `read-screen` pair is the core terminal loop: launch
or feed a command in another pane, then read its output to decide what
to do next. `close-*` verbs refuse the case that would destroy a
workspace, so they never pop a confirmation dialog or block your call.

*`read-screen` reads the viewport directly from the VTE terminal buffer, so
it works in every build (no feature flag); it only returns not-supported for
a pane that has no terminal surface (e.g. a browser tab).

## DO NOT

- Do not invoke `playwright install`, `npx playwright open`,
  `puppeteer.launch()`, or system Chromium / Chrome / Firefox just to
  read a public URL. The flowmux browser already covers that flow.
- Do not stamp `data-flowmux-ref` or any other attribute on the live
  DOM. flowmux's snapshot intentionally keeps the page untouched —
  ref tokens are resolved server-side.
- Do not assume ref tokens survive across snapshots or page
  navigation. After a navigate / reload / `--snapshot-after` action
  call, take a fresh snapshot before the next ref-based action.

## Reference

- Skill: `.claude/skills/flowmux-browser/SKILL.md` (this same workflow,
  Claude Code's local format).
- Project rules: [`CLAUDE.md`](CLAUDE.md).
