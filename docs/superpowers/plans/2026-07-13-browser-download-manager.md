# Browser Download Manager Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a shared Linux/macOS browser download manager with correct terminal states, file actions, bounded scrolling, history removal, clear-all, and accurate concurrent active counts.

**Architecture:** A new `ui::browser_downloads` module owns a pure lifecycle state machine and the shared GTK popover. WebKitGTK and WKDownload files become thin native-event adapters that retain only platform handles and forward destination, progress, failure, finish, and cancellation events to shared item handles.

**Tech Stack:** Rust 2021, GTK4 0.9, WebKitGTK 6 bindings, objc2 WKDownload bindings, existing flowmux file-opening and show-in-folder helpers.

## Global Constraints

- Download history remains scoped to one browser pane and is not persisted.
- Removing history never deletes the downloaded file.
- `Clear all` removes terminal entries only and keeps active downloads visible.
- Completed row primary-click opens the file; a separate folder button opens its containing directory.
- Linux and macOS must share lifecycle and GTK list behavior.
- Runtime UI verification must use the in-app flowmux browser on Linux.
- Do not modify or delete unrelated untracked files already present in the worktree.

---

### Task 1: Pure Download Lifecycle and Active-count Rules

**Files:**
- Create: `crates/flowmux/src/ui/browser_downloads.rs`
- Modify: `crates/flowmux/src/ui/mod.rs:1-16`
- Test: `crates/flowmux/src/ui/browser_downloads.rs` inline `#[cfg(test)]` module

**Interfaces:**
- Produces: `DownloadPhase`, `DownloadLifecycle::request_cancel`, `DownloadLifecycle::finish`, `DownloadLifecycle::fail`, and `DownloadLifecycle::is_terminal`.
- Produces: `DownloadCollection` testable bookkeeping with `insert`, `finish`, `fail`, `request_cancel`, `remove_terminal`, `clear_terminal`, `active_count`, and `len`.
- Consumes: no platform-native objects or GTK widgets.

- [ ] **Step 1: Register the module and write lifecycle failure tests**

Add `mod browser_downloads;` to `ui/mod.rs`, create the module, and add tests with these exact assertions:

```rust
#[test]
fn cancelled_finish_never_becomes_complete() {
    let mut lifecycle = DownloadLifecycle::default();
    assert!(lifecycle.request_cancel());
    assert!(lifecycle.finish());
    assert_eq!(lifecycle.phase(), &DownloadPhase::Cancelled);
}

#[test]
fn cancelled_failure_is_reported_as_cancelled() {
    let mut lifecycle = DownloadLifecycle::default();
    lifecycle.request_cancel();
    assert!(lifecycle.fail("network stopped".into()));
    assert_eq!(lifecycle.phase(), &DownloadPhase::Cancelled);
}

#[test]
fn failure_cannot_be_overwritten_by_finished() {
    let mut lifecycle = DownloadLifecycle::default();
    assert!(lifecycle.fail("connection reset".into()));
    assert!(!lifecycle.finish());
    assert_eq!(
        lifecycle.phase(),
        &DownloadPhase::Failed("connection reset".into())
    );
}

#[test]
fn normal_finish_is_complete() {
    let mut lifecycle = DownloadLifecycle::default();
    assert!(lifecycle.finish());
    assert_eq!(lifecycle.phase(), &DownloadPhase::Complete);
}
```

- [ ] **Step 2: Run the new tests and verify RED**

Run: `rtk cargo test -p flowmux ui::browser_downloads::tests -- --nocapture`

Expected: compilation fails because `DownloadLifecycle` and `DownloadPhase` are not defined.

- [ ] **Step 3: Implement the minimal lifecycle**

Implement these exact types and transition rules:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
enum DownloadPhase {
    InProgress,
    Cancelling,
    Complete,
    Cancelled,
    Failed(String),
}

impl DownloadPhase {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Complete | Self::Cancelled | Self::Failed(_))
    }
}

#[derive(Clone, Debug)]
struct DownloadLifecycle {
    phase: DownloadPhase,
}

impl Default for DownloadLifecycle {
    fn default() -> Self {
        Self { phase: DownloadPhase::InProgress }
    }
}

impl DownloadLifecycle {
    fn phase(&self) -> &DownloadPhase { &self.phase }

    fn request_cancel(&mut self) -> bool {
        if self.phase != DownloadPhase::InProgress { return false; }
        self.phase = DownloadPhase::Cancelling;
        true
    }

    fn finish(&mut self) -> bool {
        if self.phase.is_terminal() { return false; }
        self.phase = if self.phase == DownloadPhase::Cancelling {
            DownloadPhase::Cancelled
        } else {
            DownloadPhase::Complete
        };
        true
    }

    fn fail(&mut self, error: String) -> bool {
        if self.phase.is_terminal() { return false; }
        self.phase = if self.phase == DownloadPhase::Cancelling {
            DownloadPhase::Cancelled
        } else {
            DownloadPhase::Failed(error)
        };
        true
    }
}
```

- [ ] **Step 4: Verify lifecycle GREEN**

Run: `rtk cargo test -p flowmux ui::browser_downloads::tests -- --nocapture`

Expected: all four lifecycle tests pass.

- [ ] **Step 5: Add collection RED tests**

Add tests for overlapping and terminal-entry management:

```rust
#[test]
fn overlapping_downloads_decrement_active_count_once() {
    let mut collection = DownloadCollection::default();
    let first = collection.insert();
    let second = collection.insert();
    assert_eq!(collection.active_count(), 2);
    assert!(collection.finish(first));
    assert_eq!(collection.active_count(), 1);
    assert!(!collection.finish(first));
    assert_eq!(collection.active_count(), 1);
    assert!(collection.fail(second, "offline".into()));
    assert_eq!(collection.active_count(), 0);
}

#[test]
fn clear_terminal_keeps_active_entries() {
    let mut collection = DownloadCollection::default();
    let active = collection.insert();
    let finished = collection.insert();
    collection.finish(finished);
    assert_eq!(collection.clear_terminal(), vec![finished]);
    assert_eq!(collection.len(), 1);
    assert!(!collection.remove_terminal(active));
    assert_eq!(collection.active_count(), 1);
}
```

- [ ] **Step 6: Run collection tests and verify RED**

Run: `rtk cargo test -p flowmux ui::browser_downloads::tests -- --nocapture`

Expected: compilation fails because `DownloadCollection` is not defined.

- [ ] **Step 7: Implement collection bookkeeping**

Use a monotonically increasing `u64` id, a `HashMap<u64, DownloadLifecycle>`, and an explicit `active_count`. `finish` and `fail` decrement only when their lifecycle transition returns true. `clear_terminal` sorts returned ids for deterministic tests, removes only terminal entries, and leaves active entries untouched.

- [ ] **Step 8: Verify collection GREEN and commit**

Run: `rtk cargo test -p flowmux ui::browser_downloads::tests -- --nocapture`

Expected: all lifecycle and collection tests pass.

Commit:

```bash
rtk git add crates/flowmux/src/ui/mod.rs crates/flowmux/src/ui/browser_downloads.rs
rtk git commit -m "test(browser): define download lifecycle rules"
```

---

### Task 2: Shared GTK Download Popover

**Files:**
- Modify: `crates/flowmux/src/ui/browser_downloads.rs`
- Test: `crates/flowmux/src/ui/browser_downloads.rs` inline tests

**Interfaces:**
- Consumes: Task 1 `DownloadCollection` and existing `ui::show_in_folder::open_directory`.
- Produces: cloneable `pub(crate) struct DownloadManager` with `new`, `button`, `add`, `active_count`, `entry_count`, and `clear_terminal`.
- Produces: cloneable `pub(crate) struct DownloadItem` with `set_destination`, `set_progress`, `request_cancel`, `finish`, `fail`, and test-visible `status_text`.
- Changes: `file_browser::open_file(path: &Path)` remains `pub(crate)` and is reused directly.

- [ ] **Step 1: Write GTK construction and item-state RED tests**

Add tests that initialize GTK with the repository's existing test environment and assert:

```rust
#[test]
fn manager_uses_bounded_vertical_scroller() {
    let manager = DownloadManager::new();
    assert_eq!(manager.scroll_policy(), gtk::PolicyType::Never);
    assert_eq!(manager.max_content_height(), 420);
    assert!(manager.propagates_natural_height());
}

#[test]
fn clear_all_keeps_active_download_rows() {
    let manager = DownloadManager::new();
    let active = manager.add(|| {});
    let complete = manager.add(|| {});
    complete.finish();
    assert_eq!(manager.entry_count(), 2);
    manager.clear_terminal();
    assert_eq!(manager.entry_count(), 1);
    assert_eq!(manager.active_count(), 1);
    active.fail("cleanup".into());
}
```

- [ ] **Step 2: Run tests and verify RED**

Run: `rtk cargo test -p flowmux ui::browser_downloads::tests -- --nocapture`

Expected: compilation fails because `DownloadManager` and `DownloadItem` are not defined.

- [ ] **Step 3: Build the manager shell**

Implement a cloneable manager backed by `Rc<DownloadManagerInner>`. Construct:

```rust
let button = gtk::MenuButton::builder()
    .icon_name("folder-download-symbolic")
    .tooltip_text("Downloads")
    .build();
let clear = gtk::Button::with_label("Clear all");
clear.add_css_class("flat");
let scroll = gtk::ScrolledWindow::new();
scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
scroll.set_max_content_height(420);
scroll.set_propagate_natural_height(true);
```

The popover root contains the right-aligned clear button and the scroller. The scroller contains a vertical list whose empty label is `No downloads yet`.

- [ ] **Step 4: Add rows and terminal rendering**

`DownloadManager::add` creates a row with a wide primary button, folder button (`folder-open-symbolic`), cancel button (`process-stop-symbolic`), and remove button (`edit-delete-symbolic`). Store the destination in `Rc<RefCell<Option<PathBuf>>>` and store row widgets by collection id.

Render exact status strings:

```text
Preparing download…
Cancelling…
Complete
Cancelled
Failed: <message>
File not found
```

Only complete items enable the primary and folder buttons. Only terminal items show the remove button. Only active items show cancel. Every terminal transition updates clear sensitivity, empty visibility, and the active-count tooltip.

Expose `status_text` under `#[cfg(test)]` by reading the row's progress/status label so adapter-order tests assert rendered behavior rather than private widget structure.

- [ ] **Step 5: Wire file actions and history actions**

Primary-click checks `destination.is_file()` and calls `crate::ui::file_browser::open_file`. Folder-click checks the destination and calls `crate::ui::show_in_folder::open_directory` on its parent. A missing destination logs a warning and changes the visible status to `File not found`.

Remove deletes the terminal row from the list and collection, without touching disk. Clear iterates `DownloadCollection::clear_terminal`, removes the matching rows, and keeps active rows.

- [ ] **Step 6: Verify GREEN, full local module tests, and commit**

Run: `rtk cargo test -p flowmux ui::browser_downloads::tests -- --nocapture`

Expected: all shared lifecycle, collection, and GTK manager tests pass.

Run: `rtk cargo check -p flowmux`

Expected: exit 0.

Commit:

```bash
rtk git add crates/flowmux/src/ui/browser_downloads.rs
rtk git commit -m "feat(browser): add shared download list UI"
```

---

### Task 3: WebKitGTK Adapter

**Files:**
- Modify: `crates/flowmux/src/ui/browser_pane_webkit.rs:255-397`
- Test: shared manager tests plus Linux runtime scenario in Task 5

**Interfaces:**
- Consumes: `DownloadManager::new`, `DownloadManager::button`, and `DownloadManager::add`.
- Produces: native WebKitGTK callbacks forwarding into `DownloadItem` methods.

- [ ] **Step 1: Add and run the WebKitGTK event-order regression test**

Before changing the native adapter, extend the shared test to exercise the exact failed-then-finished order WebKitGTK emits:

```rust
#[test]
fn failed_then_finished_adapter_order_preserves_failure() {
    let manager = DownloadManager::new();
    let item = manager.add(|| {});
    item.fail("connection reset".into());
    item.finish();
    assert_eq!(item.status_text(), "Failed: connection reset");
    assert_eq!(manager.active_count(), 0);
}
```

- [ ] **Step 2: Run the adapter-order test**

Run: `rtk cargo test -p flowmux failed_then_finished_adapter_order_preserves_failure -- --exact`

Expected: the test passes against the shared manager built in Task 2. The previously captured live Linux scenario remains the RED evidence for the old native adapter: cancel removes the partial file but the old row renders `Complete`.

- [ ] **Step 3: Replace the duplicated Linux popover**

Create one `DownloadManager`, append `manager.button()` to the browser chrome, and remove the existing local `downloads`, `downloads_list`, `downloads_empty`, and per-row construction block.

In `network_session.connect_download_started`:

```rust
let native_for_cancel = download.clone();
let item = manager.add(move || native_for_cancel.cancel());
```

Forward callbacks exactly once:

- destination callback: create directory, choose `available_download_path`, call `item.set_destination(&destination)`, then `download.set_destination`;
- progress callback: `item.set_progress(download.estimated_progress())`;
- failed callback: `item.fail(error.to_string())`;
- finished callback: `item.finish()`.

- [ ] **Step 4: Verify Linux adapter compilation and tests**

Run: `rtk cargo test -p flowmux ui::browser_downloads::tests -- --nocapture`

Expected: all download-manager tests pass.

Run: `rtk cargo check -p flowmux`

Expected: exit 0 with no warnings introduced by the adapter.

- [ ] **Step 5: Commit the Linux adapter**

```bash
rtk git add crates/flowmux/src/ui/browser_pane_webkit.rs crates/flowmux/src/ui/browser_downloads.rs
rtk git commit -m "fix(browser): route Linux downloads through shared manager"
```

---

### Task 4: WKDownload Adapter

**Files:**
- Modify: `crates/flowmux/src/ui/browser_pane_macos.rs:65-77`
- Modify: `crates/flowmux/src/ui/browser_pane_macos.rs:217-366`
- Modify: `crates/flowmux/src/ui/browser_pane_macos.rs:480-494`
- Modify: `crates/flowmux/src/ui/browser_pane_macos.rs:933-943`
- Test: shared lifecycle tests and available macOS target compilation

**Interfaces:**
- Consumes: the same shared manager/item API as Task 3.
- Produces: `DownloadUi { item: DownloadItem, handle: Rc<RefCell<Option<Retained<WKDownload>>>> }` for active native downloads.

- [ ] **Step 1: Replace macOS duplicated widgets with the shared manager**

Change `BrowserNavigationDelegateIvars` to store `download_manager: DownloadManager` and the native active-download map. Construct one manager beside the browser chrome and pass its clone into the delegate initializer.

- [ ] **Step 2: Forward WKDownload lifecycle events**

In `begin_download`, create the retained handle first, pass a cancellation closure to `download_manager.add`, poll progress into `item.set_progress`, and store `{ item, handle }` by native download key.

In destination selection call `item.set_destination(&destination)`. In `finish_download`, remove the native entry and call `item.finish()` for success or `item.fail(error)` for error. The shared manager owns active counts and terminal rendering.

- [ ] **Step 3: Verify shared tests and host build**

Run: `rtk cargo test -p flowmux ui::browser_downloads::tests -- --nocapture`

Expected: all shared tests pass on Linux because the shared module is platform-neutral.

Run: `rtk cargo check -p flowmux`

Expected: exit 0 for the host target.

- [ ] **Step 4: Attempt macOS target checking when available**

Run: `rtk rustup target list --installed`

If an Apple target is installed, run `rtk cargo check -p flowmux --target <installed-apple-target>` and require exit 0. If none is installed, record that exact limitation and inspect the macOS-only changed source for signature consistency without claiming a compiled macOS result.

- [ ] **Step 5: Commit the macOS adapter**

```bash
rtk git add crates/flowmux/src/ui/browser_pane_macos.rs
rtk git commit -m "feat(browser): share download management on macOS"
```

---

### Task 5: Full Verification and Live Linux Scenario

**Files:**
- Modify only if a verified defect is found: files changed in Tasks 1-4
- Runtime artifacts: `/tmp/flowmux-download-manager-*` and temporary files in `~/Downloads`

**Interfaces:**
- Consumes: completed implementation from Tasks 1-4 and the flowmux browser CLI.
- Produces: test output, screenshots, filesystem evidence, and a clean post-test environment.

- [ ] **Step 1: Run formatting, source checks, and the full suite**

Run:

```bash
rtk cargo fmt --all -- --check
rtk cargo check -p flowmux
rtk cargo test -p flowmux
rtk git diff --check
```

Expected: formatting and checks exit 0; the full flowmux suite reports zero failures.

- [ ] **Step 2: Install the current debug/release binary used by the live instance**

Build the current branch with the repository's normal build command, then launch a separate disposable flowmux instance/socket if the existing process cannot safely reload the binary. Do not replace the user's running session without preserving it.

- [ ] **Step 3: Start a deterministic local download server**

Serve:

- a small attachment named `flowmux-runtime-download.txt`;
- the same name a second time to prove collision suffixing; and
- a throttled attachment named `flowmux-runtime-cancel.bin` long enough to cancel.

Use an in-process Python HTTP server command without writing fixtures into the repository.

- [ ] **Step 4: Verify successful and overlapping downloads**

Open the server in a flowmux browser pane, trigger two downloads, and verify:

- both rows appear;
- the tooltip reports the correct positive active count while transfers overlap;
- both complete rows remain visible;
- files are saved as the base filename and `(1)` collision filename; and
- hashes match the served payload.

- [ ] **Step 5: Verify cancellation state**

Start the throttled download, press cancel through the real GTK popover, wait for terminal callbacks, and verify:

- row text is `Cancelled`, never `Complete`;
- the active count returns to zero; and
- no final or `.wkdownload` partial file remains.

- [ ] **Step 6: Verify file actions**

Click a completed row and observe the registered default application process/window open the file. Click its folder button and observe the containing file-manager directory open. Capture process/window and screenshot evidence for both actions.

- [ ] **Step 7: Verify removal, clear-all, and scrolling**

Create enough terminal rows to exceed 420 pixels and verify the popover remains bounded with vertical scrolling. Remove one row and confirm the file stays on disk. Press `Clear all`, confirm active rows would be retained, and confirm the empty label returns after no entries remain.

- [ ] **Step 8: Clean runtime artifacts and re-run the completion gate**

Stop the local server, close the temporary browser pane/instance, and remove only the uniquely named test downloads. Then run:

```bash
rtk cargo check -p flowmux
rtk cargo test -p flowmux
rtk git diff --check
rtk git status --short
```

Expected: checks pass, test artifacts are gone, and status shows only intentional tracked implementation commits plus the user's pre-existing unrelated untracked paths.
