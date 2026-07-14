// SPDX-License-Identifier: GPL-3.0-or-later
//! Notification storage, desktop delivery cleanup, and launcher badge publishing.

use super::*;

#[derive(Clone)]
pub(super) struct NotificationCoordinator {
    store: NotificationStore,
    notifier: Arc<tokio::sync::Mutex<Option<flowmux_notify::DesktopNotifier>>>,
    badge_publisher_busy: Rc<Cell<bool>>,
    badge_dirty: Rc<Cell<bool>>,
    tokio_handle: Option<tokio::runtime::Handle>,
}

impl std::ops::Deref for NotificationCoordinator {
    type Target = NotificationStore;

    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

impl NotificationCoordinator {
    pub(super) fn new(
        store: NotificationStore,
        tokio_handle: Option<tokio::runtime::Handle>,
    ) -> Self {
        Self {
            store,
            notifier: Arc::new(tokio::sync::Mutex::new(None)),
            badge_publisher_busy: Rc::new(Cell::new(false)),
            badge_dirty: Rc::new(Cell::new(false)),
            tokio_handle,
        }
    }

    pub(super) fn use_shared_notifier(
        &mut self,
        notifier: Arc<tokio::sync::Mutex<Option<flowmux_notify::DesktopNotifier>>>,
    ) {
        self.notifier = notifier;
    }

    pub(super) fn tokio_handle(&self) -> Option<tokio::runtime::Handle> {
        self.tokio_handle.clone()
    }

    pub(super) fn refresh_launcher_badge(&self) {
        if self.badge_publisher_busy.get() {
            self.badge_dirty.set(true);
            return;
        }
        self.badge_publisher_busy.set(true);
        self.badge_dirty.set(false);
        let notifier_cell = self.notifier.clone();
        let store = self.store.clone();
        let busy = self.badge_publisher_busy.clone();
        let dirty = self.badge_dirty.clone();
        let handle = self.tokio_handle.clone();
        glib::MainContext::default().spawn_local(async move {
            let _enter = handle.as_ref().map(|handle| handle.enter());
            let app_uri = format!(
                "application://{}.desktop",
                flowmux_notify::DESKTOP_FILE_BASENAME
            );
            loop {
                let Some(notifier) = ensure_desktop_notifier(&notifier_cell).await else {
                    dirty.set(false);
                    busy.set(false);
                    return;
                };
                let count = store.unread_count() as i64;
                if let Err(error) = notifier.update_launcher_count(&app_uri, count).await {
                    tracing::debug!(%error, count, "launcher entry update failed");
                }
                if !dirty.get() {
                    busy.set(false);
                    return;
                }
                dirty.set(false);
            }
        });
    }

    pub(super) fn close_desktop_notifications(&self, desktop_ids: Vec<String>) {
        if desktop_ids.is_empty() {
            return;
        }
        let notifier_cell = self.notifier.clone();
        let handle = self.tokio_handle.clone();
        glib::MainContext::default().spawn_local(async move {
            let _enter = handle.as_ref().map(|handle| handle.enter());
            let Some(notifier) = ensure_desktop_notifier(&notifier_cell).await else {
                return;
            };
            for desktop_id in desktop_ids {
                if let Err(error) = notifier.close(&desktop_id).await {
                    tracing::debug!(%error, %desktop_id, "close notification failed");
                }
            }
        });
    }
}
