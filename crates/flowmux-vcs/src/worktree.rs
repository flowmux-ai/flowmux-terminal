// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeChanges {
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
}

impl WorktreeChanges {
    pub fn is_clean(&self) -> bool {
        self.staged == 0 && self.unstaged == 0 && self.untracked == 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: String,
    pub commit_subject: Option<String>,
    pub commit_time: Option<i64>,
    pub changes: Option<WorktreeChanges>,
    pub is_main: bool,
    pub is_current: bool,
    pub is_bare: bool,
    pub lock_reason: Option<String>,
    pub prunable_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeList {
    pub repository_root: PathBuf,
    pub current_worktree: PathBuf,
    pub items: Vec<WorktreeInfo>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorktreeListError {
    #[error("not a git repository: {0}")]
    NotRepository(PathBuf),
    #[error("git is not available")]
    GitUnavailable,
    #[error("git command failed: {0}")]
    CommandFailed(String),
    #[error("invalid git worktree porcelain: {0}")]
    InvalidPorcelain(String),
}

#[derive(Debug, thiserror::Error)]
pub enum RemoveWorktreeError {
    #[error("worktree contains changes: {0}")]
    RequiresForce(String),
    #[error("worktree is locked: {0}")]
    Locked(String),
    #[error("git worktree remove failed: {0}")]
    CommandFailed(String),
}

pub async fn list_worktrees(start: &Path) -> Result<WorktreeList, WorktreeListError> {
    let repo =
        gix::discover(start).map_err(|_| WorktreeListError::NotRepository(start.to_path_buf()))?;
    let current_worktree = repo
        .work_dir()
        .map(Path::to_path_buf)
        .ok_or_else(|| WorktreeListError::NotRepository(start.to_path_buf()))?;
    let current_worktree = normalize_existing_path(&current_worktree);

    let output = git_output(
        &current_worktree,
        &["worktree", "list", "--porcelain", "-z"],
    )
    .await
    .map_err(map_list_io)?;
    if !output.status.success() {
        return Err(WorktreeListError::CommandFailed(stderr_text(
            &output.stderr,
        )));
    }

    let raw = parse_worktree_porcelain(&output.stdout)?;
    let mut items = Vec::with_capacity(raw.len());
    for (index, row) in raw.into_iter().enumerate() {
        let path = normalize_existing_path(&row.path);
        let (changes, commit_subject, commit_time) = if row.is_bare {
            (None, None, None)
        } else {
            let changes = read_changes(&path).await;
            let (subject, time) = read_commit(&current_worktree, &row.head).await;
            (changes, subject, time)
        };
        items.push(WorktreeInfo {
            is_main: index == 0,
            is_current: path == current_worktree,
            path,
            branch: row.branch,
            head: row.head,
            commit_subject,
            commit_time,
            changes,
            is_bare: row.is_bare,
            lock_reason: row.lock_reason,
            prunable_reason: row.prunable_reason,
        });
    }
    items.sort_by(|left, right| {
        right
            .is_current
            .cmp(&left.is_current)
            .then_with(|| left.branch.cmp(&right.branch))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(WorktreeList {
        repository_root: current_worktree.clone(),
        current_worktree,
        items,
    })
}

pub async fn remove_worktree(
    repository_root: &Path,
    path: &Path,
    force: bool,
) -> Result<(), RemoveWorktreeError> {
    let path = normalize_existing_path(path);
    let mut command = tokio::process::Command::new("git");
    command
        .current_dir(repository_root)
        .env("LC_ALL", "C")
        .arg("worktree")
        .arg("remove");
    if force {
        command.arg("--force");
    }
    let output = command
        .arg(&path)
        .output()
        .await
        .map_err(|error| RemoveWorktreeError::CommandFailed(error.to_string()))?;
    if output.status.success() {
        return Ok(());
    }

    let message = stderr_text(&output.stderr);
    if !force {
        if let Ok(list) = list_worktrees(repository_root).await {
            if let Some(item) = list
                .items
                .iter()
                .find(|item| item.path.as_path() == path.as_path())
            {
                if let Some(reason) = &item.lock_reason {
                    return Err(RemoveWorktreeError::Locked(reason.clone()));
                }
                if item
                    .changes
                    .as_ref()
                    .is_some_and(|changes| !changes.is_clean())
                {
                    return Err(RemoveWorktreeError::RequiresForce(message));
                }
            }
        }
    }
    Err(RemoveWorktreeError::CommandFailed(message))
}

async fn git_output(dir: &Path, args: &[&str]) -> Result<std::process::Output, std::io::Error> {
    tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .output()
        .await
}

fn map_list_io(error: std::io::Error) -> WorktreeListError {
    if error.kind() == std::io::ErrorKind::NotFound {
        WorktreeListError::GitUnavailable
    } else {
        WorktreeListError::CommandFailed(error.to_string())
    }
}

fn normalize_existing_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn stderr_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim()
        .chars()
        .take(2_000)
        .collect()
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, WorktreeListError> {
    use std::os::unix::ffi::OsStringExt;

    Ok(std::ffi::OsString::from_vec(bytes.to_vec()).into())
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, WorktreeListError> {
    String::from_utf8(bytes.to_vec())
        .map(PathBuf::from)
        .map_err(|error| WorktreeListError::InvalidPorcelain(format!("invalid path: {error}")))
}

async fn read_changes(path: &Path) -> Option<WorktreeChanges> {
    let output = git_output(
        path,
        &["status", "--porcelain=v2", "-z", "--untracked-files=normal"],
    )
    .await
    .ok()?;
    output
        .status
        .success()
        .then(|| parse_status_porcelain(&output.stdout))
}

async fn read_commit(repository_root: &Path, head: &str) -> (Option<String>, Option<i64>) {
    if head.is_empty() {
        return (None, None);
    }
    let output =
        match git_output(repository_root, &["show", "-s", "--format=%s%x00%ct", head]).await {
            Ok(output) if output.status.success() => output,
            _ => return (None, None),
        };
    let mut fields = output.stdout.split(|byte| *byte == 0);
    let subject = fields
        .next()
        .map(|value| String::from_utf8_lossy(value).trim().to_string())
        .filter(|value| !value.is_empty());
    let time = fields
        .next()
        .and_then(|value| String::from_utf8_lossy(value).trim().parse().ok());
    (subject, time)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RawWorktree {
    path: PathBuf,
    branch: Option<String>,
    head: String,
    is_bare: bool,
    lock_reason: Option<String>,
    prunable_reason: Option<String>,
}

fn parse_worktree_porcelain(bytes: &[u8]) -> Result<Vec<RawWorktree>, WorktreeListError> {
    let mut rows = Vec::new();
    let mut current: Option<RawWorktree> = None;

    for field in bytes.split(|byte| *byte == 0) {
        if field.is_empty() {
            if let Some(row) = current.take() {
                if row.path.as_os_str().is_empty() {
                    return Err(WorktreeListError::InvalidPorcelain(
                        "record has no worktree path".into(),
                    ));
                }
                rows.push(row);
            }
            continue;
        }

        let (key, value) = field
            .iter()
            .position(|byte| *byte == b' ')
            .map(|index| (&field[..index], &field[index + 1..]))
            .unwrap_or((field, &[]));
        if key == b"worktree" {
            if current.is_some() {
                return Err(WorktreeListError::InvalidPorcelain(
                    "worktree record missing separator".into(),
                ));
            }
            current = Some(RawWorktree {
                path: path_from_bytes(value)?,
                ..RawWorktree::default()
            });
            continue;
        }

        let row = current.as_mut().ok_or_else(|| {
            WorktreeListError::InvalidPorcelain(format!(
                "field {:?} appeared before worktree",
                String::from_utf8_lossy(key)
            ))
        })?;
        match key {
            b"HEAD" => row.head = String::from_utf8_lossy(value).into_owned(),
            b"branch" => {
                let value = value.strip_prefix(b"refs/heads/").unwrap_or(value);
                row.branch = Some(String::from_utf8_lossy(value).into_owned())
            }
            b"detached" => row.branch = None,
            b"bare" => row.is_bare = true,
            b"locked" => row.lock_reason = Some(String::from_utf8_lossy(value).into_owned()),
            b"prunable" => row.prunable_reason = Some(String::from_utf8_lossy(value).into_owned()),
            _ => {}
        }
    }

    if let Some(row) = current {
        rows.push(row);
    }
    Ok(rows)
}

fn parse_status_porcelain(bytes: &[u8]) -> WorktreeChanges {
    let fields: Vec<&[u8]> = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect();
    let mut changes = WorktreeChanges::default();
    let mut index = 0;
    while index < fields.len() {
        let field = fields[index];
        match field.first().copied() {
            Some(b'?') => changes.untracked += 1,
            Some(kind @ (b'1' | b'2' | b'u')) => {
                if field.len() >= 4 {
                    if field[2] != b'.' {
                        changes.staged += 1;
                    }
                    if field[3] != b'.' {
                        changes.unstaged += 1;
                    }
                }
                if kind == b'2' {
                    index += 1;
                }
            }
            _ => {}
        }
        index += 1;
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("LC_ALL", "C")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn committed_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["-c", "init.defaultBranch=main", "init"]);
        git(dir.path(), &["config", "user.name", "Worktree Test"]);
        git(
            dir.path(),
            &["config", "user.email", "worktree@test.invalid"],
        );
        std::fs::write(dir.path().join("README"), "initial\n").unwrap();
        git(dir.path(), &["add", "README"]);
        git(
            dir.path(),
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "initial subject",
            ],
        );
        dir
    }

    #[tokio::test]
    async fn list_enriches_current_and_linked_worktrees() {
        let repo = committed_repo();
        let linked_parent = tempfile::tempdir().unwrap();
        let linked = linked_parent.path().join("linked");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature/list",
                linked.to_str().unwrap(),
            ],
        );
        std::fs::write(linked.join("README"), "modified\n").unwrap();
        std::fs::write(linked.join("staged.txt"), "staged\n").unwrap();
        git(&linked, &["add", "staged.txt"]);
        std::fs::write(linked.join("untracked.txt"), "new\n").unwrap();

        let list = list_worktrees(&repo.path().join(".")).await.unwrap();
        assert_eq!(list.items.len(), 2);
        assert!(list.items[0].is_main);
        assert!(list.items[0].is_current);
        let feature = list
            .items
            .iter()
            .find(|item| item.branch.as_deref() == Some("feature/list"))
            .unwrap();
        assert_eq!(feature.commit_subject.as_deref(), Some("initial subject"));
        assert_eq!(
            feature.changes,
            Some(WorktreeChanges {
                staged: 1,
                unstaged: 1,
                untracked: 1,
            })
        );

        let from_linked = list_worktrees(&linked).await.unwrap();
        assert!(from_linked.items[0].is_current);
        assert_eq!(from_linked.items[0].branch.as_deref(), Some("feature/list"));
        assert!(from_linked.items.iter().any(|item| item.is_main));
        git(
            repo.path(),
            &["worktree", "remove", "--force", linked.to_str().unwrap()],
        );
    }

    #[tokio::test]
    async fn missing_checkout_is_retained_when_optional_status_enrichment_fails() {
        let repo = committed_repo();
        let linked_parent = tempfile::tempdir().unwrap();
        let linked = linked_parent.path().join("missing");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature/missing",
                linked.to_str().unwrap(),
            ],
        );
        std::fs::remove_dir_all(&linked).unwrap();

        let list = list_worktrees(repo.path()).await.unwrap();
        let missing = list
            .items
            .iter()
            .find(|item| item.branch.as_deref() == Some("feature/missing"))
            .expect("checkout metadata must remain visible");
        assert_eq!(missing.changes, None);
    }

    #[tokio::test]
    async fn dirty_worktree_requires_force_and_branch_survives_force_removal() {
        let repo = committed_repo();
        let linked_parent = tempfile::tempdir().unwrap();
        let linked = linked_parent.path().join("dirty");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature/remove",
                linked.to_str().unwrap(),
            ],
        );
        std::fs::write(linked.join("dirty.txt"), "unsaved\n").unwrap();

        let error = remove_worktree(repo.path(), &linked, false)
            .await
            .unwrap_err();
        assert!(matches!(error, RemoveWorktreeError::RequiresForce(_)));
        assert!(linked.exists());

        remove_worktree(repo.path(), &linked, true).await.unwrap();
        assert!(!linked.exists());
        let branch = std::process::Command::new("git")
            .args(["show-ref", "--verify", "refs/heads/feature/remove"])
            .current_dir(repo.path())
            .status()
            .unwrap();
        assert!(branch.success(), "removal must retain the branch");
    }

    #[tokio::test]
    async fn clean_worktree_removal_does_not_need_force() {
        let repo = committed_repo();
        let linked_parent = tempfile::tempdir().unwrap();
        let linked = linked_parent.path().join("clean");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature/clean",
                linked.to_str().unwrap(),
            ],
        );
        remove_worktree(repo.path(), &linked, false).await.unwrap();
        assert!(!linked.exists());
        let branch = std::process::Command::new("git")
            .args(["show-ref", "--verify", "refs/heads/feature/clean"])
            .current_dir(repo.path())
            .status()
            .unwrap();
        assert!(branch.success(), "safe removal must retain the branch");
    }

    #[tokio::test]
    async fn locked_worktree_is_rejected_without_force() {
        let repo = committed_repo();
        let linked_parent = tempfile::tempdir().unwrap();
        let linked = linked_parent.path().join("locked");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature/locked",
                linked.to_str().unwrap(),
            ],
        );
        git(
            repo.path(),
            &[
                "worktree",
                "lock",
                "--reason",
                "in use",
                linked.to_str().unwrap(),
            ],
        );

        let error = remove_worktree(repo.path(), &linked, false)
            .await
            .unwrap_err();
        assert!(matches!(error, RemoveWorktreeError::Locked(reason) if reason == "in use"));
        assert!(linked.exists());

        git(
            repo.path(),
            &["worktree", "unlock", linked.to_str().unwrap()],
        );
        git(
            repo.path(),
            &["worktree", "remove", linked.to_str().unwrap()],
        );
    }

    #[test]
    fn cleanliness_requires_all_change_counts_to_be_zero() {
        assert!(WorktreeChanges::default().is_clean());
        assert!(!WorktreeChanges {
            staged: 1,
            ..WorktreeChanges::default()
        }
        .is_clean());
        assert!(!WorktreeChanges {
            unstaged: 1,
            ..WorktreeChanges::default()
        }
        .is_clean());
        assert!(!WorktreeChanges {
            untracked: 1,
            ..WorktreeChanges::default()
        }
        .is_clean());
    }

    #[test]
    fn parses_branch_detached_locked_prunable_and_bare_records() {
        let input = b"worktree /repo/main\0HEAD 1111111111111111111111111111111111111111\0branch refs/heads/main\0\0\
worktree /repo/feature space\0HEAD 2222222222222222222222222222222222222222\0detached\0locked in use\0\0\
worktree /repo/gone\0HEAD 3333333333333333333333333333333333333333\0branch refs/heads/gone\0prunable gitdir file points to non-existent location\0\0\
worktree /repo/bare.git\0bare\0\0";

        let rows = parse_worktree_porcelain(input).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].path, PathBuf::from("/repo/main"));
        assert_eq!(rows[0].branch.as_deref(), Some("main"));
        assert_eq!(rows[1].branch, None);
        assert_eq!(rows[1].lock_reason.as_deref(), Some("in use"));
        assert_eq!(
            rows[2].prunable_reason.as_deref(),
            Some("gitdir file points to non-existent location")
        );
        assert!(rows[3].is_bare);
        assert_eq!(rows[3].head, "");
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_worktree_paths() {
        use std::os::unix::ffi::OsStrExt;

        let input = b"worktree /repo/nonutf-\xff\0HEAD 1111111111111111111111111111111111111111\0branch refs/heads/feature\0\0";
        let rows = parse_worktree_porcelain(input).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path.as_os_str().as_bytes(), b"/repo/nonutf-\xff");
    }

    #[test]
    fn rejects_fields_before_the_first_worktree_record() {
        let error = parse_worktree_porcelain(b"HEAD abc\0\0").unwrap_err();
        assert!(matches!(error, WorktreeListError::InvalidPorcelain(_)));
    }

    #[test]
    fn status_parser_counts_index_worktree_untracked_and_rename_once() {
        let input = b"1 M. N... 100644 100644 100644 a a staged.txt\0\
1 .M N... 100644 100644 100644 b b modified.txt\0\
1 MM N... 100644 100644 100644 c c both.txt\0\
2 R. N... 100644 100644 100644 d d R100 renamed.txt\0old.txt\0\
? new.txt\0";
        assert_eq!(
            parse_status_porcelain(input),
            WorktreeChanges {
                staged: 3,
                unstaged: 2,
                untracked: 1,
            }
        );
    }
}
