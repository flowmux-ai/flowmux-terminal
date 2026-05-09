# IPC / socket API

## What cmux does (from public docs)

- A CLI (`cmux <verb>`) and a "socket API" let users create workspaces,
  split panes, send keystrokes, open URLs, and automate the in-app
  browser. cmux markets the whole thing as scriptable primitives.

## What flowmux does

- Single Unix socket at `$XDG_RUNTIME_DIR/flowmux.sock` (or
  `/tmp/flowmux-<user>.sock` fallback).
- Newline-delimited JSON envelopes: `{ id, kind: request|response|event, ... }`.
- Verbs in `flowmux_ipc::protocol::Request` mirror cmux's documented CLI.
  Verbs we have not implemented yet return `RpcError::Unimplemented`
  rather than disappearing — keeps user automation forward-compatible.
- Daemon side is hosted inside `flowmux`; the headless `flowmux` CLI is
  a thin client over the same socket. Either side can produce events
  (`Event::NotificationRaised`, `Event::PortListening`) so external
  tools can subscribe.

## Crates touched

- `flowmux-ipc` — protocol + client + server
- `flowmux-cli` — verbs → client calls
- `flowmux` — server handler

## Open questions / risks

- cmux's socket API may use a binary or framed format; if so, we'll
  add a second listener on a versioned path (`flowmux.sock.v2`) instead
  of breaking line-JSON consumers.
- Authentication: socket file mode `0600` is sufficient on a
  single-user desktop; revisit for multi-user systems.
