# Terminal rendering

## What cmux does (from public docs)

- Built on libghostty, the renderer extracted from the Ghostty terminal.
  GPU-accelerated, OSC-rich, and reads the user's existing
  `~/.config/ghostty/config` for fonts/themes/colors.
- Each pane is a libghostty surface embedded in an AppKit view.

## What flowmux does

- Backend abstraction `flowmux_terminal::TerminalBackend` so the renderer
  is swappable.
- Default backend: VTE 2.91 / GTK4 (`vte4`). Mature, ships in every
  Ubuntu/GNOME box, handles OSC 9/99/777 and most CSI/SGR. Trades GPU
  acceleration for native GTK integration.
- Planned backend: libghostty embedded in a `gtk::Widget` via
  `gtk::GLArea` or libghostty's GTK embed path (Ghostty itself ships a
  GTK frontend, so the embedding API exists). Behind the `ghostty`
  feature flag.
- Ghostty config readers in `flowmux-config::ghostty` map font/theme keys
  onto whichever backend is active.

## Crates touched

- `flowmux-terminal` — backend trait + impls
- `flowmux` — wires the backend's widgets into the pane tree
- `flowmux-config` — ghostty config compatibility

## Open questions / risks

- libghostty's GTK embed lifecycle (window vs. widget) is the main
  unknown; if it expects to own the window we may need a child-process
  embed via Wayland subsurfaces or X11 reparenting.
- VTE doesn't expose all of Ghostty's renderer features (e.g. ligature
  handling, sixel, kitty graphics). Document parity gaps as we hit them.
