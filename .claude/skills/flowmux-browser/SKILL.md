---
name: flowmux-browser
description: Drive the in-app browser pane that ships with flowmux. Use when you need to open URLs, take page snapshots, or interact with web pages from inside a flowmux terminal pane — instead of spawning Playwright / Puppeteer / a system Chromium.
---

# flowmux browser automation

If you are running in a terminal that `flowmux-app` spawned (the
`FLOWMUX_PANE_ID` env var is set), prefer the in-app browser for any
read / interact-with-a-page task. The browser pane lives next to the
terminal you are in, so the user can see what you do.

## Detect

```bash
[ -n "$FLOWMUX_PANE_ID" ] && echo "inside flowmux"
```

When this is true, use the workflow below. When it is false,
`flowmux` is not running — fall back to whatever the user expects
(curl / Playwright / etc.).

## Standard loop

```bash
# Open. If a browser pane already exists to the right, the URL is
# added there as a tab; otherwise flowmux splits the source pane.
PANE=$(flowmux --json browser open https://example.com | jq -r '.pane')

# Take an interactive snapshot. Returns a Markdown tree with `eN`
# refs, plus a refs map carrying selectors. The DOM stays untouched.
flowmux --json browser snapshot pane:$PANE

# Act using ref tokens.
flowmux browser click pane:$PANE e3
flowmux browser fill  pane:$PANE e1 "user@example.com"
flowmux browser type  pane:$PANE "password"      # active element
flowmux browser press pane:$PANE Enter

# Probe state — stdout is "true" / "false" / integer.
flowmux browser is-visible pane:$PANE e3
flowmux browser is-enabled pane:$PANE e3
flowmux browser is-checked pane:$PANE e7
flowmux browser count      pane:$PANE ".result-row"

# Read page content.
flowmux browser text  pane:$PANE e3
flowmux browser value pane:$PANE e1
flowmux browser attr  pane:$PANE e3 href
flowmux browser url   pane:$PANE
flowmux browser title pane:$PANE
```

## Identifiers

`pane:<uuid>`, `surface:<uuid>`, and bare uuids are interchangeable on
the CLI. Use whichever the previous `--json` response gave you.

`--json` toggles single-line JSON output for easy parsing
(`jq -r .pane`); without it, output is human-friendly indented JSON.

## Ref token lifetime

- Refs are scoped to one snapshot per pane. Take a fresh
  `snapshot --interactive` after navigation, reload, or any DOM
  mutation (forms submitted, modals opened).
- Both `e3` and `@e3` resolve.
- A ref-not-found error means: take a new snapshot first.

## What flowmux does not (yet) do

CDP-only features are intentionally not exposed — viewport/device
emulation, network mocking, full-page tracing, screencast, raw input
injection. WebKitGTK does not implement those. If a task strictly
needs them, say so before reaching for Playwright, and keep the
user-visible page in flowmux's pane anyway (mirror URLs / outputs back
in via `flowmux browser open`).

## Anti-patterns

- Do not call `playwright install`, `npx playwright open`,
  `puppeteer.launch`, or system `chromium` / `chrome` to read a
  public URL when `FLOWMUX_PANE_ID` is set.
- Do not modify the page DOM yourself. The snapshot intentionally
  does not stamp `data-flowmux-ref` or any other attribute — the server
  resolves ref tokens to selectors on its side.
- Do not assume a `eN` token from a previous snapshot is still valid
  after the page changed.
