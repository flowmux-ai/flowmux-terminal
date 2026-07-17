// SPDX-License-Identifier: GPL-3.0-or-later
//! Git + linked-PR detection for the workspace sidebar.
//!
//! `inspect(root)` returns a [`flowmux_core::GitInfo`] populated from the
//! local repo (via gix) and, if `gh` is on PATH and the user is logged
//! in, the linked PR for the current branch (via `gh pr view --json`).
//!
//! We do not fail if `gh` is missing or unauthenticated — the linked PR
//! is purely a sidebar enrichment.

use flowmux_core::{GitInfo, LinkedPr, PrState};
use std::path::Path;
use tracing::warn;

pub mod worktree;

#[derive(Debug, thiserror::Error)]
pub enum VcsError {
    #[error("not a git repository: {0}")]
    NotARepo(std::path::PathBuf),
    #[error("gix: {0}")]
    Gix(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Inspect a directory. Returns `None` if it isn't a git repository.
pub async fn inspect(root: &Path) -> Result<Option<GitInfo>, VcsError> {
    let local = match local_info(root) {
        Ok(Some(info)) => info,
        Ok(None) => return Ok(None),
        Err(e) => return Err(e),
    };
    let pr = linked_pr(root, &local.branch).await.unwrap_or(None);
    Ok(Some(GitInfo {
        branch: local.branch,
        remote_url: local.remote_url,
        linked_pr: pr,
    }))
}

struct Local {
    branch: String,
    remote_url: Option<String>,
}

fn local_info(root: &Path) -> Result<Option<Local>, VcsError> {
    let repo = match gix::discover(root) {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    let head = repo.head().map_err(|e| VcsError::Gix(e.to_string()))?;
    let branch = head
        .referent_name()
        .map(|n| n.shorten().to_string())
        .unwrap_or_else(|| {
            // Detached HEAD — use a short OID.
            head.id()
                .map(|id| id.shorten_or_id().to_string())
                .unwrap_or_else(|| "HEAD".into())
        });
    let remote_url = repo.find_remote("origin").ok().and_then(|r| {
        r.url(gix::remote::Direction::Fetch)
            .map(|u| u.to_bstring().to_string())
    });
    Ok(Some(Local { branch, remote_url }))
}

async fn linked_pr(root: &Path, branch: &str) -> Result<Option<LinkedPr>, VcsError> {
    // std::process on the blocking pool, not tokio::process — see
    // `worktree::git_output` for the GLib SIGCHLD conflict this avoids.
    let root_owned = root.to_path_buf();
    let branch_owned = branch.to_string();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new("gh")
            .args([
                "pr",
                "view",
                &branch_owned,
                "--json",
                "number,state,url,isDraft",
            ])
            .current_dir(&root_owned)
            .output()
    })
    .await
    .map_err(std::io::Error::other)
    .and_then(|r| r);
    let out = match out {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            // gh exits non-zero for "no PR for this branch" — that's fine.
            tracing::debug!(stderr = %String::from_utf8_lossy(&o.stderr), "no linked PR");
            return Ok(None);
        }
        Err(e) => {
            warn!(error = %e, "gh CLI not available; skipping PR enrichment");
            return Ok(None);
        }
    };

    #[derive(serde::Deserialize)]
    struct Raw {
        number: u64,
        state: String,
        url: String,
        #[serde(default, rename = "isDraft")]
        is_draft: bool,
    }

    let raw: Raw = match serde_json::from_slice(&out.stdout) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "gh output parse failed");
            return Ok(None);
        }
    };

    let state = if raw.is_draft {
        PrState::Draft
    } else {
        match raw.state.as_str() {
            "OPEN" => PrState::Open,
            "MERGED" => PrState::Merged,
            "CLOSED" => PrState::Closed,
            _ => PrState::Open,
        }
    };

    Ok(Some(LinkedPr {
        number: raw.number,
        state,
        url: raw.url,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn git(dir: &Path, args: &[&str]) {
        let status = StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[tokio::test]
    async fn inspect_returns_none_outside_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(inspect(dir.path()).await.unwrap().is_none());
    }

    #[test]
    fn local_info_reads_branch_and_origin_remote() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["-c", "init.defaultBranch=main", "init"]);
        git(
            dir.path(),
            &["remote", "add", "origin", "https://example.com/flowmux.git"],
        );

        let info = local_info(dir.path()).unwrap().unwrap();
        assert_eq!(info.branch, "main");
        assert_eq!(
            info.remote_url.as_deref(),
            Some("https://example.com/flowmux.git")
        );
    }

    #[test]
    fn local_info_reports_no_remote_when_origin_missing() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["-c", "init.defaultBranch=main", "init"]);

        let info = local_info(dir.path()).unwrap().unwrap();
        assert_eq!(info.branch, "main");
        assert_eq!(info.remote_url, None);
    }

    #[test]
    fn local_info_walks_upward_to_discover_repo_from_subdir() {
        // gix::discover walks parents; a request from inside the repo's
        // worktree must still resolve to the same repo, not "not a repo".
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["-c", "init.defaultBranch=main", "init"]);
        let nested = dir.path().join("nested/deeper");
        std::fs::create_dir_all(&nested).unwrap();
        let info = local_info(&nested).unwrap().unwrap();
        assert_eq!(info.branch, "main");
    }

    #[test]
    fn local_info_reports_short_oid_for_detached_head() {
        // After init + commit + checkout to the commit's OID, HEAD becomes
        // detached and `branch` should fall back to a short OID instead of
        // a branch name. The value can vary across git versions, so we
        // assert on shape rather than equality.
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["-c", "init.defaultBranch=main", "init"]);
        git(dir.path(), &["config", "user.name", "T"]);
        git(dir.path(), &["config", "user.email", "t@t.test"]);
        std::fs::write(dir.path().join("README"), "hello").unwrap();
        git(dir.path(), &["add", "README"]);
        git(
            dir.path(),
            &["-c", "commit.gpgsign=false", "commit", "-m", "init"],
        );
        // Resolve to the commit OID and detach.
        let oid = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let oid = String::from_utf8(oid.stdout).unwrap();
        let oid = oid.trim();
        git(dir.path(), &["checkout", "--detach", oid]);

        let info = local_info(dir.path()).unwrap().unwrap();
        // Detached → branch holds either a short OID, the literal
        // "HEAD" fallback, or the full OID — never an empty string.
        assert!(!info.branch.is_empty());
        assert!(
            info.branch == "HEAD" || oid.starts_with(&info.branch) || info.branch == oid,
            "unexpected detached branch label: {:?}",
            info.branch
        );
    }
}
