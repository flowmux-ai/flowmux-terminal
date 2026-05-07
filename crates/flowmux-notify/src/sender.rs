// SPDX-License-Identifier: GPL-3.0-or-later
//! Send desktop notifications via `org.freedesktop.Notifications`.
//!
//! On Linux this is the standard FDO spec implemented by GNOME Shell,
//! KDE plasma, dunst, mako, etc. — the equivalent of macOS
//! UserNotifications used by cmux.

use flowmux_core::{Notification, NotificationLevel};
use zbus::{proxy, zvariant::Value, Connection};

/// FDO Notifications proxy. Spec: <https://specifications.freedesktop.org/notification-spec/>.
#[proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait FdoNotifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<&str>,
        hints: std::collections::HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

pub struct DesktopNotifier {
    conn: Connection,
}

impl DesktopNotifier {
    pub async fn connect() -> zbus::Result<Self> {
        Ok(Self {
            conn: Connection::session().await?,
        })
    }

    pub async fn send(&self, n: &Notification) -> zbus::Result<u32> {
        let proxy = FdoNotificationsProxy::new(&self.conn).await?;
        let mut hints = std::collections::HashMap::new();
        hints.insert("urgency", Value::U8(urgency_for(n.level)));
        hints.insert("desktop-entry", Value::Str("flowmux".into()));
        proxy
            .notify(
                "flowmux",
                0,
                "utilities-terminal",
                &n.title,
                &n.body,
                vec![],
                hints,
                expire_for(n.level),
            )
            .await
    }
}

fn urgency_for(level: NotificationLevel) -> u8 {
    // FDO urgency levels: 0 = low, 1 = normal, 2 = critical.
    match level {
        NotificationLevel::Info => 0,
        NotificationLevel::AttentionNeeded => 1,
        NotificationLevel::Error => 2,
    }
}

fn expire_for(level: NotificationLevel) -> i32 {
    // -1 lets the desktop apply its default; critical sticks until dismissed.
    match level {
        NotificationLevel::Error | NotificationLevel::AttentionNeeded => 0,
        NotificationLevel::Info => -1,
    }
}
