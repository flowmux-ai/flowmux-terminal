// SPDX-License-Identifier: GPL-3.0-or-later
//! In-process notification log shown in the sidebar's bell popover.
//!
//! flowmux already forwards every `Request::Notify` to the desktop via
//! `org.gtk.Notifications` (flowmux-notify). The GUI also keeps
//! a small in-memory transcript so the user can scroll past
//! notifications even after the OS toast fades — pressing the bell
//! button at the top of the sidebar opens a popover listing them.
//!
//! Each entry remembers the source `PaneId` / `WorkspaceId`, so
//! clicking a popover row routes back to that pane (cmux parity:
//! `openNotification → focusTabFromNotification`). Clicking also flips
//! `read = true`, which the popover renders with reduced opacity.

use flowmux_core::{NotificationId, NotificationLevel, PaneId, SurfaceId, WorkspaceId};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

/// Cap on retained entries. A chatty agent (Claude Code's per-step OSC
/// 9 stream) can otherwise grow this unbounded over a long session.
/// 200 matches cmux's `TerminalNotificationStore` policy.
const MAX_RETAINED: usize = 200;

/// Suppress a fresh entry when an entry with the same `(pane, surface,
/// level)` arrived within this window. Codex / Claude Code emit Stop
/// twice per agent turn from our perspective — once as OSC 9/99 (snooped
/// by `flowmuxctl pty-tee`) and once as the lifecycle hook spawn
/// (`flowmuxctl hooks <agent> stop`). Both legitimately fire the
/// daemon's `Request::Notify`, so the bell popover would otherwise show
/// two rows per completion. Eight seconds covers the worst observed
/// skew between the in-band OSC and the hook process spawn (cold-start
/// node / python interpreter on the first call of a session). Genuine
/// back-to-back completions on the same tab are rare on that scale, so
/// the wider window is the cheaper trade-off than the user-visible
/// double toast.
const DUP_WINDOW: chrono::Duration = chrono::Duration::milliseconds(8000);

#[derive(Debug, Clone)]
pub struct NotificationEntry {
    pub id: NotificationId,
    pub title: String,
    pub body: String,
    pub level: NotificationLevel,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// True once the user has seen this entry — either by clicking it in
    /// the popover or by it being delivered while its workspace+pane was
    /// already focused. Renders dimmed.
    pub read: bool,
    /// Source pane for the click-to-focus route. `None` when the
    /// notifier did not specify a pane (e.g. global toasts).
    pub pane: Option<PaneId>,
    /// Specific tab inside `pane` that triggered the event so the
    /// click router can switch tabs when the user is currently
    /// looking at a different surface in the same pane.
    pub surface: Option<SurfaceId>,
    /// Workspace of `pane`, resolved by the IPC handler before the entry
    /// reached the GUI thread. Without it we cannot route clicks back to
    /// the right side-panel row.
    pub workspace: Option<WorkspaceId>,
    /// `org.gtk.Notifications` id we passed to `AddNotification` for
    /// the desktop toast. Stored so that — once the user reads this
    /// entry inside the bell popover — flowmux can issue
    /// `RemoveNotification(desktop_id)` so the GNOME message-tray
    /// entry vanishes and the dock badge shrinks in lockstep.
    /// `None` when the toast was suppressed (e.g. source pane already
    /// focused) or when the notifications daemon was unreachable.
    pub desktop_id: Option<String>,
}

/// Thread-local notification store backing the sidebar bell popover and
/// the click-to-focus router.
///
/// The store is `!Send` on purpose — only the GTK main thread writes to
/// it. Background paths (IPC handler, OSC reader) reach it via
/// `GtkCommand::AddNotification` so the bridge is the single
/// serialization point.
#[derive(Clone, Default)]
pub struct NotificationStore {
    inner: Rc<RefCell<VecDeque<NotificationEntry>>>,
}

impl NotificationStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a fresh entry. When the buffer is full the oldest entry
    /// is dropped to keep memory bounded under chatty agent streams.
    /// Returns the assigned id so the caller can later reference it,
    /// or `None` when the entry was suppressed as a near-duplicate of
    /// an entry pushed inside [`DUP_WINDOW`] (see the constant for
    /// why both the OSC sniffer and the lifecycle hook fire on the
    /// same Stop event).
    pub fn push(
        &self,
        title: String,
        body: String,
        level: NotificationLevel,
        pane: Option<PaneId>,
        surface: Option<SurfaceId>,
        workspace: Option<WorkspaceId>,
    ) -> Option<NotificationId> {
        let now = chrono::Utc::now();
        let mut entries = self.inner.borrow_mut();
        // Same (pane, surface) inside DUP_WINDOW → drop. Body and
        // level are intentionally NOT part of the key: the OSC path
        // carries the raw agent message ("Codex finished — review
        // the diff") at the heuristic-inferred level (Info when the
        // text lacks "waiting"/"approval"/…), while the lifecycle
        // hook path carries our formatted summary ("Codex ready /
        // task complete") at hard-coded AttentionNeeded. Both fire
        // for the same Stop event, so matching on pane+surface is
        // the only key that catches the duplicate; including level
        // re-introduced the 2× toast we are trying to suppress.
        //
        // Pane-less notifications (`flowmuxctl notify` with no --pane,
        // global toasts) skip the dedupe — they can legitimately fire
        // back-to-back from unrelated callers, and we have no other
        // signal to tell them apart.
        if pane.is_some() && surface.is_some() {
            if let Some(last) = entries
                .iter()
                .rev()
                .find(|e| e.pane == pane && e.surface == surface)
            {
                if now.signed_duration_since(last.created_at) < DUP_WINDOW {
                    return None;
                }
            }
        }
        let id = NotificationId::new();
        if entries.len() >= MAX_RETAINED {
            entries.pop_front();
        }
        entries.push_back(NotificationEntry {
            id,
            title,
            body,
            level,
            created_at: now,
            read: false,
            pane,
            surface,
            workspace,
            desktop_id: None,
        });
        Some(id)
    }

    /// Flip `read = true` for `id`. Returns `true` when the entry was
    /// found and changed (so callers can avoid redundant re-renders).
    pub fn mark_read(&self, id: NotificationId) -> bool {
        let mut entries = self.inner.borrow_mut();
        if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
            if !e.read {
                e.read = true;
                return true;
            }
        }
        false
    }

    /// Record the desktop notification id assigned to `id`. The IPC
    /// handler calls this after the daemon returns `Response::Notified`
    /// so the popover can later ask the daemon to withdraw the toast
    /// when the user reads it inside flowmux.
    ///
    /// Returns [`SetDesktopIdResult`] so the caller can detect the IPC
    /// race where the user already swept the entry to read (by opening
    /// the bell popover or activating the source workspace) before the
    /// daemon's `Notify` reply arrived. In that case
    /// `mark_all_unread_read` / `mark_workspace_read` had no
    /// `desktop_id` to return, so the toast would otherwise stay alive
    /// in the message tray and the dock badge would not shrink.
    pub fn set_desktop_id(&self, id: NotificationId, desktop_id: String) -> SetDesktopIdResult {
        let mut entries = self.inner.borrow_mut();
        match entries.iter_mut().find(|e| e.id == id) {
            Some(e) => {
                e.desktop_id = Some(desktop_id);
                if e.read {
                    SetDesktopIdResult::Stale
                } else {
                    SetDesktopIdResult::Stored
                }
            }
            None => SetDesktopIdResult::Unknown,
        }
    }

    /// Flip every unread entry to `read = true` and return the
    /// `desktop_id` of each one that previously carried a desktop
    /// notification. The caller forwards those ids to
    /// `Request::CloseDesktopNotification` so
    /// `org.gtk.Notifications.RemoveNotification` drops the matching
    /// message-tray entries and the dock badge converges. Returns an
    /// empty vec when nothing changed — handy for skipping a no-op IPC
    /// roundtrip.
    pub fn mark_all_unread_read(&self) -> Vec<String> {
        let mut entries = self.inner.borrow_mut();
        let mut closed = Vec::new();
        for e in entries.iter_mut() {
            if !e.read {
                e.read = true;
                if let Some(did) = e.desktop_id.take() {
                    closed.push(did);
                }
            }
        }
        closed
    }

    /// Mark every unread entry whose source `workspace` matches `ws` as
    /// read and return their `desktop_id`s. Used by the side-panel row
    /// activation path so that selecting a workspace also acknowledges
    /// the notifications it produced — the dock badge then drops by
    /// exactly the count of acknowledged entries instead of staying
    /// pinned to the cumulative total.
    ///
    /// Entries with `workspace = None` (global notifications) are
    /// untouched; only the bell popover sweep clears those.
    pub fn mark_workspace_read(&self, ws: WorkspaceId) -> Vec<String> {
        let mut entries = self.inner.borrow_mut();
        let mut closed = Vec::new();
        for e in entries.iter_mut() {
            if !e.read && e.workspace == Some(ws) {
                e.read = true;
                if let Some(did) = e.desktop_id.take() {
                    closed.push(did);
                }
            }
        }
        closed
    }

    pub fn find(&self, id: NotificationId) -> Option<NotificationEntry> {
        self.inner.borrow().iter().find(|e| e.id == id).cloned()
    }

    /// Drop the entry with `id` from the in-memory transcript. Used by
    /// the per-row trash button in the bell popover so the user can
    /// clear individual entries without acknowledging the rest. Returns
    /// [`RemoveOutcome`] so the caller can decide whether to also
    /// withdraw the matching desktop toast: only entries that were
    /// unread (and carried a `desktop_id`) need a follow-up
    /// `CloseDesktopNotifications`.
    pub fn remove(&self, id: NotificationId) -> RemoveOutcome {
        let mut entries = self.inner.borrow_mut();
        let Some(idx) = entries.iter().position(|e| e.id == id) else {
            return RemoveOutcome::Unknown;
        };
        let entry = entries
            .remove(idx)
            .expect("position just confirmed it exists");
        if entry.read {
            // Read entries don't move the unread count and never had a
            // pending desktop toast — pure transcript-only delete.
            RemoveOutcome::RemovedRead
        } else {
            RemoveOutcome::RemovedUnread {
                desktop_id: entry.desktop_id,
            }
        }
    }

    /// Drop every entry from the in-memory transcript. Returns the
    /// `desktop_id`s that the FDO notifications daemon still has open
    /// (i.e. unread entries that had been handed a toast id), so the
    /// caller can withdraw the matching toasts in one batch via
    /// `CloseDesktopNotifications`. Read entries had no live toast and
    /// contribute nothing here. Used by the bell-popover "All Clear"
    /// header button.
    pub fn clear_all(&self) -> Vec<String> {
        let mut entries = self.inner.borrow_mut();
        let mut desktop_ids = Vec::new();
        for entry in entries.iter() {
            if !entry.read {
                if let Some(did) = entry.desktop_id.clone() {
                    desktop_ids.push(did);
                }
            }
        }
        entries.clear();
        desktop_ids
    }

    /// Return a fresh `Vec` snapshot in insertion order. The caller owns
    /// the list and may render newest-first.
    pub fn entries(&self) -> Vec<NotificationEntry> {
        self.inner.borrow().iter().cloned().collect()
    }

    /// Number of unread entries. Surfaced to tests and the UI for
    /// invariants ("after I acked, this is 0"). The dock badge is now
    /// derived by the desktop from `org.gtk.Notifications` per-app
    /// state, so this value does not need to be re-published anywhere.
    pub fn unread_count(&self) -> usize {
        self.inner.borrow().iter().filter(|e| !e.read).count()
    }
}

/// Outcome of [`NotificationStore::remove`]. Tells the dispatcher
/// whether the deleted entry was unread and whether it had a live
/// desktop toast that still needs to be withdrawn from the message
/// tray.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// Entry was found, was already read, and was dropped. No
    /// `unread_count()` change, no desktop toast to close.
    RemovedRead,
    /// Entry was found, was unread, and was dropped. `desktop_id` is
    /// `Some` when the notifications daemon had handed us a toast id;
    /// caller should withdraw it so the message tray (and dock badge)
    /// drops the row in lockstep with the bell popover.
    RemovedUnread { desktop_id: Option<String> },
    /// No entry with that id exists (already deleted, never existed,
    /// or aged out under the cap). Safe to ignore.
    Unknown,
}

/// Outcome of [`NotificationStore::set_desktop_id`]. Lets the IPC
/// dispatcher react to the case where the user marked the entry read
/// (popover open / workspace activation) before the daemon's `Notify`
/// reply arrived — in that case the dock badge would otherwise stay
/// inflated forever.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetDesktopIdResult {
    /// Entry was unread; `desktop_id` is now stored and will flow
    /// through the next `mark_*_read` sweep.
    Stored,
    /// Entry was already marked read by the time the desktop id
    /// arrived. Caller should immediately close the FDO toast.
    Stale,
    /// Entry id was unknown (aged out under the cap). Safe to ignore.
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> NotificationStore {
        NotificationStore::new()
    }

    #[test]
    fn push_returns_id_that_resolves_back_to_entry() {
        let s = store();
        let id = s
            .push(
                "Claude".into(),
                "done".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        let found = s.find(id).expect("entry should be findable by id");
        assert_eq!(found.title, "Claude");
        assert_eq!(found.body, "done");
        assert!(!found.read);
    }

    #[test]
    fn mark_read_is_idempotent_and_flips_only_once() {
        let s = store();
        let id = s
            .push(
                "x".into(),
                "y".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        assert!(s.mark_read(id), "first mark_read should report a change");
        assert!(!s.mark_read(id), "second mark_read on same id is a no-op");
        assert!(s.find(id).unwrap().read);
    }

    #[test]
    fn mark_read_with_unknown_id_is_safe_noop() {
        let s = store();
        assert!(!s.mark_read(NotificationId::new()));
        assert_eq!(s.unread_count(), 0);
    }

    #[test]
    fn entries_preserve_insertion_order_and_unread_count_tracks_reads() {
        let s = store();
        let a = s
            .push(
                "a".into(),
                "".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        let b = s
            .push(
                "b".into(),
                "".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        let c = s
            .push(
                "c".into(),
                "".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        let entries = s.entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].id, a);
        assert_eq!(entries[1].id, b);
        assert_eq!(entries[2].id, c);
        assert_eq!(s.unread_count(), 3);
        s.mark_read(b);
        assert_eq!(s.unread_count(), 2);
        s.mark_read(a);
        s.mark_read(c);
        assert_eq!(s.unread_count(), 0);
    }

    #[test]
    fn push_records_pane_and_workspace_for_click_routing() {
        let s = store();
        let pane = PaneId::new();
        let surface = SurfaceId::new();
        let ws = WorkspaceId::new();
        let id = s
            .push(
                "claude".into(),
                "ready".into(),
                NotificationLevel::AttentionNeeded,
                Some(pane),
                Some(surface),
                Some(ws),
            )
            .expect("push must record an entry");
        let e = s.find(id).unwrap();
        assert_eq!(e.pane, Some(pane));
        assert_eq!(e.surface, Some(surface));
        assert_eq!(e.workspace, Some(ws));
        assert_eq!(e.level, NotificationLevel::AttentionNeeded);
    }

    #[test]
    fn push_drops_oldest_entry_once_cap_is_reached() {
        let s = store();
        // Insert MAX_RETAINED + 5 — the first 5 should age out and the
        // newest 5 must keep their order at the tail.
        let mut ids = Vec::new();
        for i in 0..(MAX_RETAINED + 5) {
            ids.push(s.push(
                format!("entry {i}"),
                "".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            ));
        }
        let entries = s.entries();
        assert_eq!(entries.len(), MAX_RETAINED);
        // The first 5 ids should no longer be findable.
        for stale in &ids[..5] {
            assert!(s.find(stale.expect("test push must succeed")).is_none());
        }
        // The newest id should still be at the tail.
        assert_eq!(
            entries.last().unwrap().id,
            ids.last().unwrap().expect("test push must succeed")
        );
    }

    #[test]
    fn duplicate_push_for_same_pane_creates_distinct_entries() {
        let s = store();
        let pane = PaneId::new();
        let ws = WorkspaceId::new();
        let a = s
            .push(
                "claude".into(),
                "step 1".into(),
                NotificationLevel::Info,
                Some(pane),
                None,
                Some(ws),
            )
            .expect("push must record an entry");
        let b = s
            .push(
                "claude".into(),
                "step 2".into(),
                NotificationLevel::Info,
                Some(pane),
                None,
                Some(ws),
            )
            .expect("push must record an entry");
        assert_ne!(a, b);
        assert_eq!(s.entries().len(), 2);
    }

    #[test]
    fn set_desktop_id_attaches_id_to_existing_entry() {
        let s = store();
        let id = s
            .push(
                "Claude".into(),
                "ready".into(),
                NotificationLevel::AttentionNeeded,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        assert_eq!(
            s.set_desktop_id(id, "did-42".into()),
            SetDesktopIdResult::Stored
        );
        assert_eq!(s.find(id).unwrap().desktop_id.as_deref(), Some("did-42"));
    }

    #[test]
    fn set_desktop_id_for_unknown_id_is_safe_noop() {
        let s = store();
        // No panic, no insert — just silently ignored. Mirrors the
        // mark_read contract for entries that aged out under the cap.
        assert_eq!(
            s.set_desktop_id(NotificationId::new(), "did-7".into()),
            SetDesktopIdResult::Unknown
        );
        assert_eq!(s.entries().len(), 0);
    }

    #[test]
    fn set_desktop_id_after_mark_read_reports_stale_so_caller_can_close() {
        // Simulates the IPC race: the user opened the popover and
        // mark_all_unread_read flipped the entry to read before the
        // daemon's `Notify` reply (which carries the desktop_id) made
        // it back to the GUI. Without Stale, the desktop toast would
        // never be withdrawn and the dock badge would stay inflated.
        let s = store();
        let id = s
            .push(
                "Claude".into(),
                "ready".into(),
                NotificationLevel::AttentionNeeded,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        // Sweep first (no desktop_id yet → empty vec).
        assert!(s.mark_all_unread_read().is_empty());
        // Late-arriving desktop id should report Stale.
        assert_eq!(
            s.set_desktop_id(id, "did-99".into()),
            SetDesktopIdResult::Stale
        );
        assert_eq!(s.find(id).unwrap().desktop_id.as_deref(), Some("did-99"));
    }

    #[test]
    fn mark_all_unread_read_flips_unread_and_returns_their_desktop_ids() {
        let s = store();
        let a = s
            .push(
                "a".into(),
                "".into(),
                NotificationLevel::AttentionNeeded,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        let b = s
            .push(
                "b".into(),
                "".into(),
                NotificationLevel::AttentionNeeded,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        let c = s
            .push(
                "c".into(),
                "".into(),
                NotificationLevel::AttentionNeeded,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        // Entry `a` carries no desktop_id (e.g. toast was suppressed),
        // so it should be marked read but NOT show up in the close
        // list. `b` and `c` should both round-trip their ids.
        let _ = s.set_desktop_id(b, "did-11".into());
        let _ = s.set_desktop_id(c, "did-22".into());
        // Pre-mark `b` as already read so the loop must skip it and
        // not double-emit its desktop_id.
        s.mark_read(b);
        let to_close = s.mark_all_unread_read();
        assert_eq!(s.unread_count(), 0);
        // Order follows insertion order; only the previously-unread
        // ones with a desktop_id should appear.
        assert_eq!(to_close, vec!["did-22".to_string()]);
        // `a` still has no desktop_id but is now read.
        assert!(s.find(a).unwrap().read);
        assert!(s.find(c).unwrap().read);
    }

    #[test]
    fn mark_workspace_read_only_flips_unread_entries_for_that_workspace() {
        let s = store();
        let ws_a = WorkspaceId::new();
        let ws_b = WorkspaceId::new();
        let a1 = s
            .push(
                "claude".into(),
                "step 1".into(),
                NotificationLevel::AttentionNeeded,
                None,
                None,
                Some(ws_a),
            )
            .expect("push must record an entry");
        let a2 = s
            .push(
                "claude".into(),
                "step 2".into(),
                NotificationLevel::AttentionNeeded,
                None,
                None,
                Some(ws_a),
            )
            .expect("push must record an entry");
        let b1 = s
            .push(
                "codex".into(),
                "step 1".into(),
                NotificationLevel::AttentionNeeded,
                None,
                None,
                Some(ws_b),
            )
            .expect("push must record an entry");
        let global = s
            .push(
                "ready".into(),
                "global".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        let _ = s.set_desktop_id(a1, "did-11".into());
        let _ = s.set_desktop_id(a2, "did-12".into());
        let _ = s.set_desktop_id(b1, "did-21".into());
        // ws_a sweep: only a1 / a2 close, b1 + global stay unread.
        let closed = s.mark_workspace_read(ws_a);
        assert_eq!(closed, vec!["did-11".to_string(), "did-12".to_string()]);
        assert!(s.find(a1).unwrap().read);
        assert!(s.find(a2).unwrap().read);
        assert!(!s.find(b1).unwrap().read);
        assert!(!s.find(global).unwrap().read);
        assert_eq!(s.unread_count(), 2);
        // Repeat sweep is a no-op; nothing left to close for ws_a.
        assert!(s.mark_workspace_read(ws_a).is_empty());
    }

    #[test]
    fn mark_workspace_read_skips_entries_with_no_workspace() {
        let s = store();
        let ws = WorkspaceId::new();
        let _global = s
            .push(
                "info".into(),
                "global".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        // Activating a workspace must never collaterally clear global
        // notifications — only the bell popover sweep does that.
        assert!(s.mark_workspace_read(ws).is_empty());
        assert_eq!(s.unread_count(), 1);
    }

    #[test]
    fn mark_all_unread_read_returns_empty_when_nothing_changed() {
        let s = store();
        // No entries → empty list, no panic.
        assert!(s.mark_all_unread_read().is_empty());
        // Insert and mark read; second sweep is a no-op.
        let id = s
            .push(
                "x".into(),
                "".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        assert_eq!(
            s.set_desktop_id(id, "did-99".into()),
            SetDesktopIdResult::Stored
        );
        assert_eq!(s.mark_all_unread_read(), vec!["did-99".to_string()]);
        assert!(s.mark_all_unread_read().is_empty());
    }

    #[test]
    fn remove_unread_entry_drops_it_and_reports_desktop_id_for_toast_close() {
        let s = store();
        let a = s
            .push(
                "a".into(),
                "".into(),
                NotificationLevel::AttentionNeeded,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        let b = s
            .push(
                "b".into(),
                "".into(),
                NotificationLevel::AttentionNeeded,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        let _ = s.set_desktop_id(b, "did-77".into());

        // Trash button on `b` (unread, has desktop_id) — must report
        // the id so the dispatcher can call CloseDesktopNotifications.
        assert_eq!(
            s.remove(b),
            RemoveOutcome::RemovedUnread {
                desktop_id: Some("did-77".into())
            },
            "deleting an unread entry must surface its desktop_id so the desktop toast is withdrawn"
        );
        assert!(
            s.find(b).is_none(),
            "removed entry must no longer be findable"
        );
        assert_eq!(
            s.unread_count(),
            1,
            "unread_count must drop by exactly one after removing an unread entry"
        );

        // Trash button on `a` (unread, no desktop_id captured) — still
        // unread/dropped, but desktop_id is None so caller skips the
        // FDO close roundtrip.
        assert_eq!(
            s.remove(a),
            RemoveOutcome::RemovedUnread { desktop_id: None },
            "unread entry without a desktop_id still reports RemovedUnread but skips toast close"
        );
        assert_eq!(s.unread_count(), 0);
        assert_eq!(s.entries().len(), 0);
    }

    #[test]
    fn remove_read_entry_drops_it_without_touching_unread_count() {
        let s = store();
        let a = s
            .push(
                "a".into(),
                "".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        let b = s
            .push(
                "b".into(),
                "".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        s.mark_read(a);
        let unread_before = s.unread_count();

        assert_eq!(
            s.remove(a),
            RemoveOutcome::RemovedRead,
            "deleting an already-read entry must report RemovedRead so the dispatcher skips the badge republish"
        );
        assert!(s.find(a).is_none());
        assert_eq!(
            s.unread_count(),
            unread_before,
            "removing a read entry must not change unread_count"
        );
        assert_eq!(s.entries().len(), 1);
        assert_eq!(s.entries()[0].id, b, "remaining order must be preserved");
    }

    #[test]
    fn remove_unknown_id_is_safe_noop() {
        let s = store();
        let id = s
            .push(
                "x".into(),
                "".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        // Already-deleted / never-existed id must round-trip Unknown
        // so the dispatcher skips refresh work and FDO close calls.
        assert_eq!(s.remove(NotificationId::new()), RemoveOutcome::Unknown);
        assert_eq!(s.entries().len(), 1);
        // First delete succeeds, second on the same id is Unknown.
        assert!(matches!(s.remove(id), RemoveOutcome::RemovedUnread { .. }));
        assert_eq!(s.remove(id), RemoveOutcome::Unknown);
    }

    #[test]
    fn remove_preserves_insertion_order_of_surrounding_entries() {
        let s = store();
        let a = s
            .push(
                "a".into(),
                "".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        let b = s
            .push(
                "b".into(),
                "".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        let c = s
            .push(
                "c".into(),
                "".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        let _ = s.remove(b);
        let ids: Vec<_> = s.entries().iter().map(|e| e.id).collect();
        assert_eq!(
            ids,
            vec![a, c],
            "removing a middle entry must collapse the gap without reordering survivors"
        );
    }

    #[test]
    fn clear_all_drops_every_entry_and_returns_unread_desktop_ids() {
        let s = store();
        // a: unread + desktop id  → returned for toast withdrawal
        // b: read + desktop id    → no toast to withdraw
        // c: unread, no desktop id → silently dropped
        let a = s
            .push(
                "a".into(),
                "".into(),
                NotificationLevel::AttentionNeeded,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        let b = s
            .push(
                "b".into(),
                "".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        let _c = s
            .push(
                "c".into(),
                "".into(),
                NotificationLevel::Info,
                None,
                None,
                None,
            )
            .expect("push must record an entry");
        let _ = s.set_desktop_id(a, "did-a".into());
        let _ = s.set_desktop_id(b, "did-b".into());
        assert!(s.mark_read(b));

        let ids = s.clear_all();
        assert_eq!(ids, vec!["did-a".to_string()]);
        assert!(s.entries().is_empty());
        assert_eq!(s.unread_count(), 0);
    }

    #[test]
    fn clear_all_on_empty_store_returns_empty_and_does_not_panic() {
        let s = store();
        assert!(s.clear_all().is_empty());
        assert!(s.entries().is_empty());
    }

    #[test]
    fn push_records_distinct_surfaces_within_same_pane() {
        let s = store();
        let pane = PaneId::new();
        let tab_a = SurfaceId::new();
        let tab_b = SurfaceId::new();
        let id_a = s
            .push(
                "Claude".into(),
                "tab A done".into(),
                NotificationLevel::Info,
                Some(pane),
                Some(tab_a),
                None,
            )
            .expect("push must record an entry");
        let id_b = s
            .push(
                "Codex".into(),
                "tab B done".into(),
                NotificationLevel::Info,
                Some(pane),
                Some(tab_b),
                None,
            )
            .expect("push must record an entry");
        assert_eq!(s.find(id_a).unwrap().surface, Some(tab_a));
        assert_eq!(s.find(id_b).unwrap().surface, Some(tab_b));
    }
}
