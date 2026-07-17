// SPDX-License-Identifier: GPL-3.0-or-later
//! Tokio side of self-update: the periodic release check and the
//! install runner executing the [`check`] command plan. This layer is
//! deliberately thin — every decision (version compare, command
//! argv, script name) comes from the unit-tested [`check`] module.

use super::check::{self, Version};
use super::{Event, Stage};
use anyhow::Context;
use std::path::PathBuf;
use std::process::Stdio;

const REPO_URL: &str = "https://github.com/flowmux-ai/flowmux-terminal.git";
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Managed source checkout, independent of any user clone.
fn clone_dir() -> Option<PathBuf> {
    flowmux_config::paths::host_visible_cache_dir().map(|d| d.join("src"))
}

/// Combined log of the last update attempt (git + install script).
pub fn log_path() -> Option<PathBuf> {
    flowmux_config::paths::host_visible_cache_dir().map(|d| d.join("update.log"))
}

/// Check for a newer release now and then every 24 h, announcing hits
/// on `tx`. Failures (offline, no git) are logged and stay silent —
/// the banner simply never appears.
pub async fn check_loop(tx: async_channel::Sender<Event>) {
    let mut tick = tokio::time::interval(CHECK_INTERVAL);
    loop {
        tick.tick().await; // first tick fires immediately = startup check
        match check_once().await {
            Ok(Some(latest)) => {
                if tx.send(Event::Available(latest)).await.is_err() {
                    return; // banner gone, window closing
                }
            }
            Ok(None) => {
                if tx.send(Event::Current).await.is_err() {
                    return; // banner gone, window closing
                }
            }
            Err(e) => tracing::warn!(error = %e, "release check failed"),
        }
    }
}

pub(crate) async fn check_once() -> anyhow::Result<Option<Version>> {
    // std::process on the blocking pool, not tokio::process — GLib's
    // child watch owns SIGCHLD in the GUI process, so tokio's child
    // wait never wakes on macOS (see flowmux-vcs `git_output`).
    let output = tokio::task::spawn_blocking(|| {
        std::process::Command::new("git")
            .args(["ls-remote", "--tags", REPO_URL])
            .stdin(Stdio::null())
            .output()
    })
    .await
    .context("join ls-remote task")?
    .context("run git ls-remote")?;
    if !output.status.success() {
        anyhow::bail!(
            "git ls-remote failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let versions = check::parse_ls_remote(&String::from_utf8_lossy(&output.stdout));
    Ok(check::latest(&versions)
        .filter(|latest| check::update_available(env!("CARGO_PKG_VERSION"), *latest)))
}

/// Bring the managed clone to `version` and run the platform install
/// script, reporting progress and the final outcome on `tx`.
pub async fn run_install(version: Version, tx: async_channel::Sender<Event>) {
    let outcome = run_install_inner(version, &tx).await;
    let event = match outcome {
        Ok(()) => Event::Done(version),
        Err(e) => {
            tracing::warn!(error = %e, "self-update failed");
            Event::Failed(format!("{e:#}"))
        }
    };
    let _ = tx.send(event).await;
}

async fn run_install_inner(
    version: Version,
    tx: &async_channel::Sender<Event>,
) -> anyhow::Result<()> {
    let dir = clone_dir().context("HOME is unset")?;
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).context("create cache dir")?;
    }
    let log =
        std::fs::File::create(log_path().context("HOME is unset")?).context("create update.log")?;

    let _ = tx.send(Event::Stage(Stage::Fetching)).await;
    let tag = version.tag();
    let clone_exists = dir.join(".git").is_dir();
    if run_plan(check::git_plan(clone_exists, REPO_URL, &dir, &tag), &log)
        .await
        .is_err()
        && clone_exists
    {
        // A stale or corrupt managed clone must not block the update:
        // wipe it and retry once from a fresh shallow clone.
        std::fs::remove_dir_all(&dir).context("reset managed clone")?;
        run_plan(check::git_plan(false, REPO_URL, &dir, &tag), &log).await?;
    }

    let _ = tx.send(Event::Stage(Stage::Installing)).await;
    let script = check::install_script(std::env::consts::OS);
    // A launcher-started GUI has no ~/.cargo/bin on PATH, so the
    // script's bare `cargo` would hit an outdated distro toolchain.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let path = check::install_path_env(
        home.as_deref(),
        &std::env::var_os("PATH").unwrap_or_default(),
    );
    run_logged(
        vec!["bash".to_string(), script.to_string()],
        Some(dir),
        path.map(|p| ("PATH", p)),
        &log,
    )
    .await
}

async fn run_plan(plan: Vec<Vec<String>>, log: &std::fs::File) -> anyhow::Result<()> {
    for argv in plan {
        run_logged(argv, None, None, log).await?;
    }
    Ok(())
}

/// Run one command with stdout/stderr appended to the update log, so
/// a failure is diagnosable from `update.log` without re-running.
///
/// std::process on the blocking pool, not tokio::process — GLib's
/// child watch owns SIGCHLD in the GUI process, so tokio's child wait
/// never wakes on macOS (see flowmux-vcs `git_output`).
async fn run_logged(
    argv: Vec<String>,
    cwd: Option<std::path::PathBuf>,
    env: Option<(&'static str, std::ffi::OsString)>,
    log: &std::fs::File,
) -> anyhow::Result<()> {
    let label = argv.join(" ");
    let stdout = Stdio::from(log.try_clone().context("clone log handle")?);
    let stderr = Stdio::from(log.try_clone().context("clone log handle")?);
    let run_label = label.clone();
    let status = tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        if let Some((key, value)) = env {
            cmd.env(key, value);
        }
        cmd.status()
    })
    .await
    .with_context(|| format!("join {run_label}"))?
    .with_context(|| format!("run {label}"))?;
    if !status.success() {
        anyhow::bail!("{label} exited with {status}");
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::process::Command;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn deferred_macos_swap_waits_for_the_running_app() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("FlowMux.app");
        let staged = temp.path().join(".FlowMux.app.pending");
        let backup = temp.path().join(".FlowMux.app.previous");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(destination.join("version"), "old").unwrap();
        std::fs::write(staged.join("version"), "new").unwrap();

        let mut host = Command::new("sleep").arg("30").spawn().unwrap();
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/deferred-macos-app-swap.sh");
        let mut swap = Command::new("sh")
            .arg(script)
            .arg(host.id().to_string())
            .arg(&staged)
            .arg(&destination)
            .arg(&backup)
            .spawn()
            .unwrap();

        thread::sleep(Duration::from_millis(300));
        assert_eq!(
            std::fs::read_to_string(destination.join("version")).unwrap(),
            "old"
        );
        assert!(staged.is_dir());
        assert!(swap.try_wait().unwrap().is_none());

        host.kill().unwrap();
        host.wait().unwrap();
        for _ in 0..100 {
            if let Some(status) = swap.try_wait().unwrap() {
                assert!(status.success());
                assert_eq!(
                    std::fs::read_to_string(destination.join("version")).unwrap(),
                    "new"
                );
                assert!(!staged.exists());
                assert!(!backup.exists());
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = swap.kill();
        panic!("deferred app swap did not finish");
    }
}
