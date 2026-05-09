# Notifications

## What cmux does (from public docs)

- Picks up terminal escape sequences OSC 9 / 99 / 777 emitted by tools
  like Claude Code, Codex, OpenCode, etc.
- Surfaces them in three places:
  1. A blue ring around the pane that emitted the notification.
  2. A badge on the workspace's sidebar row.
  3. A native desktop notification (macOS UserNotifications).
- `cmux notify "<body>"` CLI writes one of these escape sequences so
  user-supplied agent hooks can produce notifications without writing
  raw bytes.
- `⌘⇧U` jumps focus to the most-recent unread.

## What flowmux does

- `flowmux-notify::osc::parse_osc` parses the public escape-sequence
  payloads and returns an `OscNotification { title, body, level }`.
- Level inference is a heuristic on the body text (keywords like
  "waiting" / "needs your input" / "error") so we light up the pane
  ring for attention-class notifications without relying on
  application-specific signaling.
- Desktop delivery goes through `org.freedesktop.Notifications` via
  zbus (the libnotify protocol). Urgency hint set from level.
- `flowmux notify` CLI writes the same well-known OSC 9 sequence to its
  controlling tty so it works whether or not the flowmux daemon is
  attached — keeps user hooks portable to bare terminals.

## Crates touched

- `flowmux-notify` — parser + D-Bus sender
- `flowmux-ipc` — `Request::Notify` verb
- `flowmux` — pane ring CSS + sidebar badge

## Open questions / risks

- cmux likely uses a sound on attention-needed notifications; FDO
  notifications support `sound-name` / `sound-file` hints — verify
  which sound to use; default to `dialog-information`.
- OSC 99 options: cmux may treat specific keys (`urgency=`, `id=`)
  specially; we ignore unknown opts today and re-parse them when we
  need them.
