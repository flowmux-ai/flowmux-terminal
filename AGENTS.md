<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Agents working inside flowmux

This file tells AI coding agents (Claude Code, Codex CLI, OpenCode,
similar) how to drive the in-app browser that ships with `flowmux`. It
is loaded automatically by every agent that follows the AGENTS.md
convention; you do not need to instruct agents to read it.

If you (the agent) are running inside a `flowmux` PTY, **prefer the
flowmux browser over Playwright / Puppeteer / a system Chromium** for
any task whose goal is to read or interact with a web page. The flowmux
browser already runs in a pane next to your terminal, supports the
same kind of "snapshot → click/fill" loop, and keeps the user's
attention in one window. Spawning a separate Chromium hides the page
from the user and breaks parity with how flowmux is intended to be used.

## How to know you are inside flowmux

`flowmux` injects these env vars into every PTY it spawns:

```
FLOWMUX_PANE_ID         <uuid>
FLOWMUX_SURFACE_ID      <uuid>     (alias of FLOWMUX_PANE_ID)
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
| screenshot / wait / viewport / network mocking / screencast | not yet | Phase 5 deferred; CDP-only items will stay `not_supported` |

## When the task really needs Playwright / Puppeteer

`flowmux-browser` runs on WebKitGTK 6, which does not expose the Chrome
DevTools Protocol. If a task strictly requires CDP-only features
(network request mocking, full-page tracing, accurate device viewport
emulation, etc.), say so explicitly and run an external Playwright
session **alongside** the flowmux browser, not in place of it. Even then,
the user-facing flow (the page they are looking at) should still live
in the flowmux pane — copy URLs / outputs back into flowmux's WebView so
the user can see what the agent is doing.

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
- Upstream tracking: [`docs/UPSTREAM.md`](docs/UPSTREAM.md).
