// SPDX-License-Identifier: GPL-3.0-or-later
//! Installation-origin detection and the update action it permits.

use super::check::Version;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOrigin {
    Deb,
    Source,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateGate {
    SourceBuild,
    ReleasePage,
}

pub fn classify_install_origin(
    executable: &Path,
    home: Option<&Path>,
    dpkg_owns_executable: bool,
) -> InstallOrigin {
    if dpkg_owns_executable
        || executable.starts_with("/usr/bin")
        || executable.starts_with("/usr/lib")
    {
        return InstallOrigin::Deb;
    }
    if home.is_some_and(|home| {
        executable.starts_with(home.join(".local/bin"))
            || executable.starts_with(home.join(".cargo/bin"))
    }) {
        return InstallOrigin::Source;
    }
    InstallOrigin::Unknown
}

pub fn install_origin() -> InstallOrigin {
    let Ok(executable) = std::env::current_exe() else {
        return InstallOrigin::Unknown;
    };
    let dpkg_owns_executable = cfg!(target_os = "linux")
        && std::process::Command::new("dpkg")
            .arg("-S")
            .arg(&executable)
            .output()
            .is_ok_and(|output| output.status.success());
    classify_install_origin(
        &executable,
        std::env::var_os("HOME").as_deref().map(Path::new),
        dpkg_owns_executable,
    )
}

pub fn update_gate(origin: InstallOrigin) -> UpdateGate {
    match origin {
        InstallOrigin::Source => UpdateGate::SourceBuild,
        InstallOrigin::Deb | InstallOrigin::Unknown => UpdateGate::ReleasePage,
    }
}

pub fn release_page_url(version: Version) -> String {
    format!(
        "https://github.com/flowmux-ai/flowmux-terminal/releases/tag/{}",
        version.tag()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_origin_classifies_deb_source_and_unknown_paths() {
        let home = Path::new("/home/alice");
        assert_eq!(
            classify_install_origin(Path::new("/usr/bin/flowmux"), Some(home), false),
            InstallOrigin::Deb
        );
        assert_eq!(
            classify_install_origin(
                Path::new("/home/alice/.local/bin/flowmux"),
                Some(home),
                false
            ),
            InstallOrigin::Source
        );
        assert_eq!(
            classify_install_origin(Path::new("/opt/flowmux/bin/flowmux"), Some(home), false),
            InstallOrigin::Unknown
        );
        assert_eq!(
            classify_install_origin(Path::new("/opt/flowmux"), Some(home), true),
            InstallOrigin::Deb
        );
    }

    #[test]
    fn only_source_installs_can_run_the_source_builder() {
        assert_eq!(update_gate(InstallOrigin::Source), UpdateGate::SourceBuild);
        assert_eq!(update_gate(InstallOrigin::Deb), UpdateGate::ReleasePage);
        assert_eq!(update_gate(InstallOrigin::Unknown), UpdateGate::ReleasePage);
    }
}
