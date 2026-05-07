// SPDX-License-Identifier: GPL-3.0-or-later
//! Chromium-family browsers (Chrome, Chromium, Brave, Edge, Arc, ...).
//!
//! Profile layouts share a SQLite file at:
//!
//!   <browser-config>/Default/Cookies
//!
//! On Linux the `encrypted_value` BLOB is wrapped with a key the
//! browser stores in the Secret Service (libsecret). Until we ship a
//! libsecret-backed unwrapper, this source detects the file but
//! returns [`Error::EncryptedValuesUnsupported`] from `list_cookies`
//! so the GUI can show a clear status string instead of silently
//! exporting empty values.

use crate::cookie::Cookie;
use crate::source::{BrowserId, Error, Source};
use std::path::PathBuf;

pub struct Chromium {
    id: BrowserId,
}

impl Chromium {
    pub fn new(id: BrowserId) -> Self {
        Self { id }
    }

    fn config_dir(&self) -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        let path = match self.id {
            BrowserId::Chrome => home.join(".config/google-chrome/Default/Cookies"),
            BrowserId::Chromium => home.join(".config/chromium/Default/Cookies"),
            BrowserId::Brave => home.join(".config/BraveSoftware/Brave-Browser/Default/Cookies"),
            BrowserId::Edge => home.join(".config/microsoft-edge/Default/Cookies"),
            BrowserId::Arc => home.join(".config/arc/User Data/Default/Cookies"),
            _ => return None,
        };
        Some(path)
    }
}

impl Source for Chromium {
    fn id(&self) -> BrowserId {
        self.id
    }

    fn detect(&self) -> Option<PathBuf> {
        self.config_dir().filter(|p| p.exists())
    }

    fn list_cookies(&self, _domain_filter: Option<&str>) -> Result<Vec<Cookie>, Error> {
        // Verify the file is at least present so callers can distinguish
        // "browser not installed" from "encryption gate".
        let path = self.detect().ok_or_else(|| {
            Error::ProfileNotFound(PathBuf::from(format!(
                "Chromium-family ({:?}) cookies db",
                self.id
            )))
        })?;
        // The schema is intentionally not parsed yet — listing without
        // unwrapping `encrypted_value` would yield empty strings, which
        // is worse than a hard error for agent automation.
        tracing::warn!(
            path = %path.display(),
            "Chromium cookies db detected but encrypted_value is not yet unwrapped"
        );
        Err(Error::EncryptedValuesUnsupported)
    }
}
