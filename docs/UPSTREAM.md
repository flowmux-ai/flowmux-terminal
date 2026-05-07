# Upstream tracking policy

flowmux is an unofficial GPL-3.0-or-later Linux reimplementation of cmux. We
do not vendor cmux source. Instead we pin a known-good upstream commit and
reimplement features for the Linux/GTK4 stack, recording the mapping per
feature.

## Pinned upstream

| Field            | Value                                   |
|------------------|-----------------------------------------|
| Repository       | https://github.com/manaflow-ai/cmux     |
| Pinned commit    | `ca228efb7692b29c695fbe2471b56a7396f14c91` |
| Pinned date      | 2026-05-07T01:46:45-07:00 |
| License          | GPL-3.0-or-later (dual; we take the GPL path) |

The public baseline is recorded in this file. `.upstream-cmux/PINNED`
(created by `scripts/sync-upstream.sh`, gitignored) is local checkout
metadata, not a vendored copy.

## Sync workflow

When upstream cmux ships changes we want to track:

```bash
scripts/sync-upstream.sh           # fetch upstream, diff CHANGELOG/README
scripts/sync-upstream.sh --since v1.2.3   # diff from a tagged release
```

The script writes a report to `docs/upstream-mapping/_inbox/<date>.md`
listing:

- new entries in cmux `CHANGELOG.md` since the last pin
- changed sections in cmux `README.md` (high-level feature changes)
- new top-level files / directories (often new features)

We then:

1. Triage the report — categorize entries as `port`, `skip` (macOS-only),
   `defer`, or `n/a`.
2. For each `port`: open / update the corresponding file in
   `docs/upstream-mapping/<feature>.md`, write the spec in our own
   words from cmux's public docs (README, cmux.com/docs, CHANGELOG),
   then implement.
3. Bump the pinned commit/date in this file and in `.upstream-cmux/PINNED`.

## What we do NOT copy

- cmux Swift / Objective-C / AppKit source — we reimplement on GTK4.
- cmux's `LICENSE` text — we use the canonical GPL-3.0 from gnu.org and
  preserve the upstream copyright notice in `NOTICE`.
- cmux assets (icons, screenshots, sounds, marketing copy).
- The `cmux` name as a binary or package name — our binary is `flowmux`.

## What we DO carry forward

- The `cmux.json` schema (so user configs port unchanged).
- Public CLI surface (`flowmux <verb>` mirrors `cmux <verb>` where the
  verb makes sense on Linux).
- Socket API verbs (so existing user automation keeps working).
- OSC notification sequence semantics (9 / 99 / 777) as they are
  documented terminal escape codes, not cmux-specific code.

## Upstream features and their Linux replacement

See [`docs/upstream-mapping/`](upstream-mapping/) for the per-feature
matrix. Top-level macOS replacements:

| cmux (macOS)              | flowmux (Linux)                              |
|---------------------------|--------------------------------------------|
| AppKit / SwiftUI          | GTK4 + libadwaita (gtk4-rs)                |
| libghostty (Metal)        | vte-2.91-gtk4 first; libghostty/GTK later  |
| WebKit (in-app browser)   | WebKitGTK 6.0                              |
| UserNotifications         | libnotify via zbus (org.freedesktop.Notifications) |
| Sparkle (auto-update)     | distro packages (.deb / Flatpak); no in-app updater |
| `~/Library/Application Support/cmux` | `$XDG_CONFIG_HOME/flowmux`, `$XDG_DATA_HOME/flowmux` |
| Keychain (cookie import)  | libsecret / Secret Service                 |
| `cmd-*` shortcuts         | `super-*` (configurable, default GNOME)    |
