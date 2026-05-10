// SPDX-License-Identifier: GPL-3.0-or-later
//! Send desktop notifications via `org.freedesktop.Notifications`.
//!
//! On Linux this is the standard FDO spec implemented by GNOME Shell,
//! KDE plasma, dunst, mako, etc. — the equivalent of macOS
//! UserNotifications used by cmux.

use flowmux_core::{Notification, NotificationLevel};
use std::collections::HashMap;
use zbus::{proxy, zvariant::Value, Connection};

/// Object path the Unity LauncherEntry signal is broadcast on. Dock
/// implementations (Ubuntu Dock, Dash-to-Dock, KDE Plasma, plank) match
/// on the interface name + `com.canonical.Unity.LauncherEntry::Update`
/// member, so the path itself just needs to be unique-ish and stable
/// across emissions for the same app.
const LAUNCHER_ENTRY_PATH: &str = "/com/canonical/unity/launcherentry/flowmux";
const LAUNCHER_ENTRY_INTERFACE: &str = "com.canonical.Unity.LauncherEntry";
const LAUNCHER_ENTRY_MEMBER: &str = "Update";

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

    fn close_notification(&self, id: u32) -> zbus::Result<()>;
}

#[derive(Clone)]
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

    /// Tell the FDO notification daemon to close (and silently
    /// withdraw) the notification with `desktop_id`. We use this to
    /// drop dock/launcher counters once the user has acknowledged the
    /// alert in flowmux's own bell popover — without it, AttentionNeeded
    /// toasts (which we send with `expire_timeout = 0`) would otherwise
    /// linger forever in the GNOME / KDE notification center.
    pub async fn close(&self, desktop_id: u32) -> zbus::Result<()> {
        let proxy = FdoNotificationsProxy::new(&self.conn).await?;
        proxy.close_notification(desktop_id).await
    }

    /// Publish the unread-notification count to the dock badge via the
    /// `com.canonical.Unity.LauncherEntry::Update` D-Bus signal. Ubuntu
    /// Dock, Dash-to-Dock, KDE Plasma and plank all listen for this
    /// signal. `count <= 0` hides the badge by sending
    /// `count-visible = false`.
    ///
    /// `app_uri` should be `application://<desktop-file-name>.desktop`
    /// (e.g. `application://com.flowmux.App.desktop`) so the dock can
    /// associate the badge with our launcher icon.
    pub async fn update_launcher_count(&self, app_uri: &str, count: i64) -> zbus::Result<()> {
        let visible = count > 0;
        let mut props: HashMap<&str, Value<'_>> = HashMap::new();
        props.insert("count", Value::I64(count.max(0)));
        props.insert("count-visible", Value::Bool(visible));
        // `urgent = true` makes some docks bounce / glow the icon. We
        // mirror visibility so the icon goes back to neutral once the
        // count hits zero.
        props.insert("urgent", Value::Bool(visible));
        self.conn
            .emit_signal(
                None::<&str>,
                LAUNCHER_ENTRY_PATH,
                LAUNCHER_ENTRY_INTERFACE,
                LAUNCHER_ENTRY_MEMBER,
                &(app_uri, props),
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
