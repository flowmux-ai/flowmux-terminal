// SPDX-License-Identifier: GPL-3.0-or-later
//! Tokio side of self-update: the periodic release check and the
//! install runner executing the [`check`] command plan. This layer is
//! deliberately thin — every decision (version compare, command
//! argv, script name) comes from the unit-tested [`check`] module.

use super::check::{self, Version};
use super::{Event, Stage, AVAILABLE};
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
                *AVAILABLE.lock().unwrap() = Some(latest);
                if tx.send(Event::Available(latest)).await.is_err() {
                    return; // banner gone, window closing
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "release check failed"),
        }
    }
}

async fn check_once() -> anyhow::Result<Option<Version>> {
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
    run_logged(
        vec!["bash".to_string(), script.to_string()],
        Some(dir),
        &log,
    )
    .await
}

async fn run_plan(plan: Vec<Vec<String>>, log: &std::fs::File) -> anyhow::Result<()> {
    for argv in plan {
        run_logged(argv, None, log).await?;
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
