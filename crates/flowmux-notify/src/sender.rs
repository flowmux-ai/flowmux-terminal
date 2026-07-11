// SPDX-License-Identifier: GPL-3.0-or-later
//! Send desktop notifications via `org.gtk.Notifications`.
//!
//! On Linux this is the modern, GNOME-backed path. Compared to the
//! legacy `org.freedesktop.Notifications` interface:
//!
//! * GNOME Shell binds every entry to the calling `app_id` (matched
//!   against the installed `.desktop` basename), so the dock badge
//!   counter — which Ubuntu Dock derives from the per-app entries in
//!   `Main.messageTray` — increments / decrements in lockstep with our
//!   `AddNotification` / `RemoveNotification` calls.
//! * `RemoveNotification` actually destroys the `MessageTray.Source`
//!   notification on GNOME 46, so both the entry in the message tray
//!   (Super+V) **and** the dock badge drop the moment we acknowledge.
//!   The legacy FDO `CloseNotification` only dismissed the live toast
//!   on this stack — the history entry persisted and the dock badge
//!   stayed pinned, which is the exact regression that motivated the
//!   switch.
//!
//! The id type is a client-chosen `String`, not the `u32` the FDO
//! daemon used to return. We generate a UUID per notification so a
//! later `close(&id)` round-trip is unambiguous even when several
//! notifications fly in parallel.

use flowmux_core::{Notification, NotificationLevel};
use std::collections::HashMap;
use zbus::{proxy, zvariant::Value, Connection};

/// Object path the Unity LauncherEntry signal is broadcast on. Ubuntu
/// Dock, Dash-to-Dock, KDE Plasma and plank all match on the interface
/// name + `com.canonical.Unity.LauncherEntry::Update` member. GNOME's
/// `org.gtk.Notifications.RemoveNotification` clears the message-tray
/// dot, but Ubuntu Dock's per-app *number circle* is still driven by
/// this Unity-vintage signal — without it the launcher count stays
/// pinned at the last published value after the user acknowledges.
const LAUNCHER_ENTRY_PATH: &str = "/com/canonical/unity/launcherentry/flowmux";
const LAUNCHER_ENTRY_INTERFACE: &str = "com.canonical.Unity.LauncherEntry";
const LAUNCHER_ENTRY_MEMBER: &str = "Update";

/// Basename of the installed desktop file (`com.flowmux.App.desktop`)
/// without the `.desktop` extension. Used as the `app_id` argument on
/// every `AddNotification` / `RemoveNotification` call so GNOME Shell
/// binds the notification to flowmux's launcher icon. A drift between
/// this string and the real desktop file name lands the dock badge on
/// a non-existent app id and the user is left with a stuck counter.
///
/// Keep this in lockstep with `crates/flowmux/src/main.rs::APP_ID`
/// and `resources/desktop/com.flowmux.App.desktop`.
pub const DESKTOP_FILE_BASENAME: &str = "com.flowmux.App";

/// `org.gtk.Notifications` proxy. Implemented by gnome-shell on GNOME
/// and by the GApplication backend elsewhere. The interface is the
/// in-process wire `g_application_send_notification` /
/// `g_application_withdraw_notification` use, so calling it directly
/// from zbus has the same observable behaviour as routing through a
/// `Gio.Application`.
#[proxy(
    interface = "org.gtk.Notifications",
    default_service = "org.gtk.Notifications",
    default_path = "/org/gtk/Notifications"
)]
trait GtkNotifications {
    fn add_notification(
        &self,
        app_id: &str,
        id: &str,
        notification: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;

    fn remove_notification(&self, app_id: &str, id: &str) -> zbus::Result<()>;
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

    /// Send a notification. Returns the client-chosen id that future
    /// `close` calls must use; on GNOME this is the same id the
    /// `MessageTray.Source` records, so a later `RemoveNotification`
    /// drops both the tray entry and the dock badge in lockstep.
    pub async fn send(&self, n: &Notification) -> zbus::Result<String> {
        let proxy = GtkNotificationsProxy::new(&self.conn).await?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut notif: HashMap<&str, Value<'_>> = HashMap::new();
        notif.insert("title", Value::Str(n.title.as_str().into()));
        notif.insert("body", Value::Str(n.body.as_str().into()));
        // GApplication notification priority maps cleanly to our levels:
        //   * Info            → "normal"
        //   * NeedsInput     → "high"   (lifts the banner above the rest)
        //   * Error           → "urgent" (sticky on GNOME — keeps the toast
        //                       visible until the user clicks).
        notif.insert("priority", Value::Str(priority_for(n.level).into()));
        // The "icon" hint takes a serialized GIcon. We omit it: GNOME
        // falls back to the launcher icon (resolved via app_id) which
        // is exactly the visual association we want, and serializing a
        // GIcon by hand is more failure surface than this is worth.
        proxy
            .add_notification(DESKTOP_FILE_BASENAME, &id, notif)
            .await?;
        Ok(id)
    }

    /// Withdraw a previously sent notification. On GNOME this destroys
    /// the `MessageTray.Source` entry and removes the message-tray dot
    /// next to the launcher icon. The *number circle* on Ubuntu Dock is
    /// driven by [`Self::update_launcher_count`] instead — call both
    /// when you want the entire dock indicator to converge with the
    /// in-app unread count. Idempotent: an unknown id is a benign
    /// no-op on the server side.
    pub async fn close(&self, desktop_id: &str) -> zbus::Result<()> {
        let proxy = GtkNotificationsProxy::new(&self.conn).await?;
        proxy
            .remove_notification(DESKTOP_FILE_BASENAME, desktop_id)
            .await
    }

    /// Publish the unread-notification count to the dock badge via the
    /// `com.canonical.Unity.LauncherEntry::Update` D-Bus signal. Ubuntu
    /// Dock, Dash-to-Dock, KDE Plasma and plank all listen for this
    /// signal to drive their per-app number circle. `count <= 0` hides
    /// the badge by sending `count-visible = false`.
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

fn priority_for(level: NotificationLevel) -> &'static str {
    match level {
        NotificationLevel::Info | NotificationLevel::TurnCompleted => "normal",
        NotificationLevel::NeedsInput => "high",
        NotificationLevel::Error => "urgent",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app_id we pass to `AddNotification` must match the installed
    /// `.desktop` basename; otherwise GNOME Shell binds the notification
    /// to a non-existent launcher and Ubuntu Dock's per-app counter
    /// never finds it, leaving the badge stuck after we ack.
    #[test]
    fn desktop_file_basename_matches_installed_desktop_file() {
        assert_eq!(
            DESKTOP_FILE_BASENAME, "com.flowmux.App",
            "DESKTOP_FILE_BASENAME must match resources/desktop/<basename>.desktop \
             and the GApplication application_id; otherwise the dock badge survives ack",
        );
    }

    /// Pin the level→priority mapping so a future refactor that flips
    /// Info/TurnCompleted ↔ NeedsInput does not silently downgrade agent toasts
    /// to "normal" and lose the elevated banner placement on GNOME.
    #[test]
    fn priority_for_levels_maps_to_gtk_notifications_strings() {
        assert_eq!(priority_for(NotificationLevel::Info), "normal");
        assert_eq!(priority_for(NotificationLevel::TurnCompleted), "normal");
        assert_eq!(priority_for(NotificationLevel::NeedsInput), "high");
        assert_eq!(priority_for(NotificationLevel::Error), "urgent");
    }
}
