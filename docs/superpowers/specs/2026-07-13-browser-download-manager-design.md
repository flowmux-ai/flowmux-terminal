# Browser Download Manager Design

## Goal

Make the browser download popover release-ready on Linux and macOS by fixing
terminal status handling and adding file actions, bounded scrolling, per-item
removal, clear-all behavior, and accurate active-download tracking.

## Scope

The download manager remains local to one browser pane and lives only for that
pane's lifetime. It does not persist history across application restarts and it
does not move or delete downloaded files when history entries are removed.

The implementation must provide:

- distinct in-progress, complete, cancelled, and failed states;
- cancellation that never appears as a successful completion;
- failure that cannot be overwritten by a later generic finished callback;
- an accurate active-download count when downloads overlap;
- primary-click opening for completed files;
- a separate containing-folder action for completed files;
- per-item removal after a download reaches a terminal state;
- a clear-all action that removes terminal entries but keeps active entries;
- a vertically scrolling list with a bounded natural height; and
- equivalent behavior for WebKitGTK and WKDownload integrations.

## Architecture

Add `crates/flowmux/src/ui/browser_downloads.rs` as the shared download manager.
It owns the GTK download button, popover, header, scrolled list, empty state,
rows, and the platform-independent lifecycle state. Linux and macOS browser
panes keep responsibility for WebKit-specific callbacks and download handles.

Each platform creates one shared manager for a browser pane. Starting a native
download creates a manager item and supplies a cancellation callback. Native
callbacks then provide the destination, progress, success, or error events to
that item. The item is the only code allowed to choose the final UI state and
to update the manager's active count.

The pure lifecycle decision logic is kept separate from GTK mutations so it can
be unit-tested without synthesizing native WebKit objects.

## Lifecycle

An item begins in `InProgress`. Pressing cancel records a cancellation request,
changes the visible text to `Cancelling…`, disables the cancel button, and calls
the platform cancellation closure.

Terminal callbacks follow these rules:

- a normal finished callback becomes `Complete`;
- a finished callback after a cancellation request becomes `Cancelled`;
- an error callback becomes `Failed(message)`, unless cancellation was already
  requested, in which case it becomes `Cancelled`;
- after an item enters any terminal state, later callbacks cannot change it;
- the manager decrements its active count exactly once per terminal item.

This ordering handles WebKitGTK's generic `finished` signal arriving after a
failure or cancellation without relabeling the row as successful.

## User Interface

The popover contains a header and a `gtk::ScrolledWindow`. The scroller has no
horizontal scrollbar, propagates natural height, and caps its content height at
420 pixels. The header shows `Clear all`; it is sensitive only when at least one
terminal entry exists.

Each row contains:

1. a wide primary button holding filename and progress/status;
2. a containing-folder button;
3. a cancel button while active; and
4. a remove-from-list button after reaching a terminal state.

For a completed item, the primary button opens the file with flowmux's existing
default-application path and the folder button opens its containing directory.
For active, failed, or cancelled items, both file actions remain insensitive.
The remove button removes only the history row. `Clear all` removes every
terminal row and retains active rows.

The empty label is visible only when no rows remain. The browser toolbar button
tooltip is `Downloads — N in progress` while `N > 0`; otherwise it is
`Downloads`. Failed rows keep their failure text in the list rather than using
the global tooltip as the sole error indicator.

## File Actions

Completed-file opening reuses `ui::file_browser::open_file`, avoiding a second
default-application implementation. Containing-folder display uses
`ui::show_in_folder::open_directory` on the destination's parent directory.
On macOS this follows the project's existing Finder integration; on Linux it
uses the existing WSL, Flatpak, or native file-manager path.

If the destination no longer exists, the action does not launch an application.
It logs a warning and changes the row status to `File not found` while leaving
the entry removable. This does not change the historical download result.

## Platform Integration

`browser_pane_webkit.rs` connects WebKitGTK's `download-started`, destination,
progress, failed, and finished signals to the shared item. Its platform closure
calls `webkit6::Download::cancel`.

`browser_pane_macos.rs` retains only the native WKDownload handle and shared item
for each active native download. Destination, progress polling, success, error,
and cancellation delegate callbacks update the shared item. The existing native
download identity map continues to prevent duplicate delegate registration.

## Testing

Test-first unit coverage must prove:

- cancel request followed by finished yields cancelled;
- cancel request followed by error yields cancelled;
- failure followed by finished remains failed;
- successful finish becomes complete;
- terminal callbacks decrement the active count once;
- overlapping downloads retain an accurate count as each finishes;
- clear-all retains active entries and removes all terminal entries;
- individual removal cannot remove an active entry; and
- the shared GTK manager constructs a bounded vertical scroller.

Existing filename sanitization and collision tests remain unchanged. The full
`flowmux` test suite and `cargo check -p flowmux` must pass.

Runtime verification on Linux must download two colliding filenames, observe
both rows and the active count, cancel a throttled download and observe
`Cancelled`, open a completed file, open its containing folder, remove one row,
clear all remaining terminal rows, and confirm the empty state returns. Test
downloads and temporary browser panes must be cleaned up afterward.

macOS runtime verification is not possible from the Linux host; macOS coverage
is established through shared logic tests plus successful macOS-target source
compilation when an installed target/toolchain is available. If unavailable,
the exact limitation is reported rather than claiming live verification.

## Non-goals

- persistent cross-session download history;
- pausing or resuming downloads;
- changing the configured download directory;
- deleting downloaded files from disk through the history UI; and
- operating-system download notifications.
