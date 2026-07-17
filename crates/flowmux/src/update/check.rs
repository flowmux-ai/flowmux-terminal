// SPDX-License-Identifier: GPL-3.0-or-later
//! Headless core of the self-update feature: release-tag parsing,
//! version comparison, and the git/install command plan. Everything
//! here is a pure function so it stays unit-testable without a
//! network, a git binary, or GTK.

use std::path::Path;

/// Semantic version parsed from a release tag or `CARGO_PKG_VERSION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    /// Parse `"0.7.0"` or `"v0.7.0"`. Anything that is not exactly
    /// three dot-separated integers (after an optional `v`) is not a
    /// release version and returns `None`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let mut parts = s.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Version(major, minor, patch))
    }

    /// The release tag name this version was published under.
    pub fn tag(&self) -> String {
        format!("v{self}")
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Extract release versions from `git ls-remote --tags` output.
/// Peeled `refs/tags/vX.Y.Z^{}` entries are ignored — the tag name is
/// all we need and it appears on the unpeeled line too.
pub fn parse_ls_remote(output: &str) -> Vec<Version> {
    output
        .lines()
        .filter_map(|line| line.split('\t').nth(1))
        .filter_map(|r| r.strip_prefix("refs/tags/"))
        .filter(|tag| !tag.ends_with("^{}"))
        .filter_map(Version::parse)
        .collect()
}

/// Newest release among `versions`, if any.
pub fn latest(versions: &[Version]) -> Option<Version> {
    versions.iter().copied().max()
}

/// True when `latest` is strictly newer than the running build.
pub fn update_available(current: &str, latest: Version) -> bool {
    Version::parse(current).is_some_and(|current| latest > current)
}

/// Repo-relative install script for the given `std::env::consts::OS`.
/// WSL reports `linux` and intentionally uses the same installer as Ubuntu.
pub fn install_script(os: &str) -> &'static str {
    if os == "macos" {
        "scripts/install-macos.sh"
    } else {
        "install.sh"
    }
}

/// PATH for the spawned install script. Launcher-started GUIs inherit
/// a session PATH without `~/.cargo/bin`, so a bare `cargo` in the
/// script can resolve to an outdated distro toolchain that cannot even
/// parse the lock file. Returns the PATH to override with: `home`'s
/// `.cargo/bin` prepended when it is absent from `path`, or `None`
/// when `path` already contains it (a shell resolving cargo through
/// that PATH is the behavior to reproduce, not to reorder) or `home`
/// is unknown.
pub fn install_path_env(home: Option<&Path>, path: &std::ffi::OsStr) -> Option<std::ffi::OsString> {
    let cargo_bin = home?.join(".cargo").join("bin");
    if std::env::split_paths(path).any(|entry| entry == cargo_bin) {
        return None;
    }
    if path.is_empty() {
        return Some(cargo_bin.into());
    }
    std::env::join_paths(std::iter::once(cargo_bin).chain(std::env::split_paths(path))).ok()
}

/// Ordered argv lists that bring the managed clone at `dir` to `tag`.
/// `clone_exists` decides between a fresh shallow clone and a shallow
/// tag fetch + detached checkout in the existing clone.
pub fn git_plan(clone_exists: bool, url: &str, dir: &Path, tag: &str) -> Vec<Vec<String>> {
    let dir = dir.display().to_string();
    let s = str::to_string;
    if clone_exists {
        vec![
            vec![
                s("git"),
                s("-C"),
                dir.clone(),
                s("fetch"),
                s("--depth"),
                s("1"),
                s("origin"),
                s("tag"),
                s(tag),
            ],
            vec![s("git"), s("-C"), dir, s("checkout"), s(tag)],
        ]
    } else {
        vec![vec![
            s("git"),
            s("clone"),
            s("--depth"),
            s("1"),
            s("--branch"),
            s(tag),
            s(url),
            dir,
        ]]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;

    #[test]
    fn version_parses_plain_and_v_prefixed() {
        assert_eq!(Version::parse("0.7.0"), Some(Version(0, 7, 0)));
        assert_eq!(Version::parse("v1.12.3"), Some(Version(1, 12, 3)));
    }

    #[test]
    fn version_rejects_non_release_strings() {
        assert_eq!(Version::parse(""), None);
        assert_eq!(Version::parse("v0.7"), None);
        assert_eq!(Version::parse("0.7.0-rc1"), None);
        assert_eq!(Version::parse("v0.7.0.1"), None);
        assert_eq!(Version::parse("main"), None);
    }

    #[test]
    fn version_orders_numerically_not_lexically() {
        assert!(Version(0, 10, 0) > Version(0, 9, 9));
        assert!(Version(1, 0, 0) > Version(0, 99, 99));
    }

    #[test]
    fn version_tag_round_trips() {
        assert_eq!(Version(0, 7, 1).tag(), "v0.7.1");
        assert_eq!(
            Version::parse(&Version(2, 0, 5).tag()),
            Some(Version(2, 0, 5))
        );
    }

    #[test]
    fn ls_remote_output_yields_versions_skipping_peeled_and_foreign_refs() {
        let out = "\
0123456789abcdef0123456789abcdef01234567\trefs/tags/v0.6.4
89abcdef0123456789abcdef0123456789abcdef\trefs/tags/v0.7.0
89abcdef0123456789abcdef0123456789abcdef\trefs/tags/v0.7.0^{}
aaaabbbbccccddddeeeeffff0000111122223333\trefs/tags/nightly
ffffeeeeddddccccbbbbaaaa9999888877776666\trefs/heads/main
";
        assert_eq!(
            parse_ls_remote(out),
            vec![Version(0, 6, 4), Version(0, 7, 0)]
        );
    }

    #[test]
    fn ls_remote_tolerates_empty_and_garbage_output() {
        assert_eq!(parse_ls_remote(""), Vec::new());
        assert_eq!(parse_ls_remote("not a ref line at all"), Vec::new());
    }

    #[test]
    fn latest_picks_max_version() {
        let versions = [Version(0, 6, 4), Version(0, 7, 0), Version(0, 6, 10)];
        assert_eq!(latest(&versions), Some(Version(0, 7, 0)));
        assert_eq!(latest(&[]), None);
    }

    #[test]
    fn update_available_only_for_strictly_newer_release() {
        assert!(update_available("0.7.0", Version(0, 7, 1)));
        assert!(!update_available("0.7.0", Version(0, 7, 0)));
        assert!(!update_available("0.7.1", Version(0, 7, 0)));
        // Unparseable local version (dev build oddity): never offer.
        assert!(!update_available("garbage", Version(9, 9, 9)));
    }

    #[test]
    fn install_script_matches_platform() {
        assert_eq!(install_script("macos"), "scripts/install-macos.sh");
        assert_eq!(install_script("linux"), "install.sh");
    }

    #[test]
    fn git_plan_clones_fresh_when_no_checkout_exists() {
        let dir = PathBuf::from("/home/u/.cache/flowmux/src");
        let plan = git_plan(false, "https://example.com/r.git", &dir, "v0.7.1");
        assert_eq!(
            plan,
            vec![vec![
                "git".to_string(),
                "clone".to_string(),
                "--depth".to_string(),
                "1".to_string(),
                "--branch".to_string(),
                "v0.7.1".to_string(),
                "https://example.com/r.git".to_string(),
                "/home/u/.cache/flowmux/src".to_string(),
            ]]
        );
    }

    #[test]
    fn install_path_env_prepends_missing_cargo_bin() {
        let home = PathBuf::from("/home/u");
        let path = OsString::from("/usr/local/bin:/usr/bin:/bin");
        assert_eq!(
            install_path_env(Some(&home), &path),
            Some(OsString::from(
                "/home/u/.cargo/bin:/usr/local/bin:/usr/bin:/bin"
            ))
        );
    }

    #[test]
    fn install_path_env_keeps_path_with_cargo_bin_untouched() {
        let home = PathBuf::from("/home/u");
        // Position must not matter: a shell that resolves cargo through
        // this PATH is the behavior to reproduce, not to reorder.
        let path = OsString::from("/usr/bin:/home/u/.cargo/bin:/bin");
        assert_eq!(install_path_env(Some(&home), &path), None);
    }

    #[test]
    fn install_path_env_without_home_is_noop() {
        assert_eq!(install_path_env(None, OsStr::new("/usr/bin")), None);
    }

    #[test]
    fn install_path_env_handles_empty_path() {
        let home = PathBuf::from("/home/u");
        assert_eq!(
            install_path_env(Some(&home), OsStr::new("")),
            Some(OsString::from("/home/u/.cargo/bin"))
        );
    }

    #[test]
    fn git_plan_fetches_tag_into_existing_checkout() {
        let dir = PathBuf::from("/home/u/.cache/flowmux/src");
        let plan = git_plan(true, "https://example.com/r.git", &dir, "v0.7.1");
        assert_eq!(
            plan,
            vec![
                vec![
                    "git".to_string(),
                    "-C".to_string(),
                    "/home/u/.cache/flowmux/src".to_string(),
                    "fetch".to_string(),
                    "--depth".to_string(),
                    "1".to_string(),
                    "origin".to_string(),
                    "tag".to_string(),
                    "v0.7.1".to_string(),
                ],
                vec![
                    "git".to_string(),
                    "-C".to_string(),
                    "/home/u/.cache/flowmux/src".to_string(),
                    "checkout".to_string(),
                    "v0.7.1".to_string(),
                ],
            ]
        );
    }
}
