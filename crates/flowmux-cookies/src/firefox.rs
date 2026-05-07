// SPDX-License-Identifier: GPL-3.0-or-later
//! Firefox stores cookies at:
//!
//!   ~/.mozilla/firefox/<profile>/cookies.sqlite
//!
//! Schema (firefox 100+):
//!
//!   CREATE TABLE moz_cookies (
//!     id INTEGER PRIMARY KEY,
//!     originAttributes TEXT NOT NULL,
//!     name TEXT, value TEXT, host TEXT, path TEXT,
//!     expiry INTEGER, lastAccessed INTEGER, creationTime INTEGER,
//!     isSecure INTEGER, isHttpOnly INTEGER,
//!     inBrowserElement INTEGER, sameSite INTEGER, ...
//!   );
//!
//! Values are plaintext.

use crate::cookie::{Cookie, SameSite};
use crate::source::{BrowserId, Error, Source};
use std::path::{Path, PathBuf};

pub struct Firefox;

impl Firefox {
    pub fn new() -> Self {
        Self
    }
}

impl Default for Firefox {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for Firefox {
    fn id(&self) -> BrowserId {
        BrowserId::Firefox
    }

    fn detect(&self) -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        let base = home.join(".mozilla/firefox");
        if !base.is_dir() {
            return None;
        }
        // Look for the default profile (or first one with a cookies db).
        for entry in std::fs::read_dir(&base).ok()?.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            let cookies = p.join("cookies.sqlite");
            if cookies.exists() {
                return Some(cookies);
            }
        }
        None
    }

    fn list_cookies(&self, domain_filter: Option<&str>) -> Result<Vec<Cookie>, Error> {
        let path = self.detect().ok_or_else(|| {
            Error::ProfileNotFound(PathBuf::from("~/.mozilla/firefox/<profile>/cookies.sqlite"))
        })?;
        list_cookies_from_path(&path, domain_filter)
    }
}

fn list_cookies_from_path(path: &Path, domain_filter: Option<&str>) -> Result<Vec<Cookie>, Error> {
    // Open read-only to avoid locking the live profile.
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let mut sql = String::from(
        "SELECT host, name, value, path, expiry, isSecure, isHttpOnly, sameSite \
         FROM moz_cookies",
    );
    if domain_filter.is_some() {
        sql.push_str(" WHERE host LIKE ?1");
    }
    let mut stmt = conn.prepare(&sql)?;
    let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Cookie> {
        let expiry: i64 = row.get(4).unwrap_or(0);
        let same_site: i64 = row.get(7).unwrap_or(0);
        Ok(Cookie {
            host: row.get(0)?,
            name: row.get(1)?,
            value: row.get(2)?,
            path: row.get(3)?,
            expires_at: if expiry > 0 {
                chrono::DateTime::from_timestamp(expiry, 0)
            } else {
                None
            },
            secure: row.get::<_, i64>(5)? != 0,
            http_only: row.get::<_, i64>(6)? != 0,
            same_site: match same_site {
                1 => SameSite::Lax,
                2 => SameSite::Strict,
                3 => SameSite::None,
                _ => SameSite::NoRestriction,
            },
        })
    };
    let rows: Vec<Cookie> = match domain_filter {
        Some(f) => stmt
            .query_map([format!("%{f}%")], map)?
            .filter_map(Result::ok)
            .collect(),
        None => stmt.query_map([], map)?.filter_map(Result::ok).collect(),
    };
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fixture_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE moz_cookies (
                host TEXT,
                name TEXT,
                value TEXT,
                path TEXT,
                expiry INTEGER,
                isSecure INTEGER,
                isHttpOnly INTEGER,
                sameSite INTEGER
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO moz_cookies VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                ".example.com",
                "sid",
                "abc",
                "/",
                1_700_000_000_i64,
                1_i64,
                0_i64,
                1_i64,
            ),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO moz_cookies VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                ".other.test",
                "pref",
                "dark",
                "/ui",
                0_i64,
                0_i64,
                1_i64,
                2_i64,
            ),
        )
        .unwrap();
    }

    #[test]
    fn reads_firefox_cookies_from_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("cookies.sqlite");
        fixture_db(&db);

        let cookies = list_cookies_from_path(&db, None).unwrap();
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0].host, ".example.com");
        assert_eq!(cookies[0].name, "sid");
        assert_eq!(cookies[0].value, "abc");
        assert_eq!(cookies[0].path, "/");
        assert!(cookies[0].expires_at.is_some());
        assert!(cookies[0].secure);
        assert!(!cookies[0].http_only);
        assert_eq!(cookies[0].same_site, SameSite::Lax);
    }

    #[test]
    fn filters_firefox_cookies_by_domain_substring() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("cookies.sqlite");
        fixture_db(&db);

        let cookies = list_cookies_from_path(&db, Some("other")).unwrap();
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].host, ".other.test");
        assert_eq!(cookies[0].same_site, SameSite::Strict);
        assert_eq!(cookies[0].expires_at, None);
        assert!(!cookies[0].secure);
        assert!(cookies[0].http_only);
    }
}
