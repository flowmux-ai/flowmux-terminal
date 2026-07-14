// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;

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

        let text = std::str::from_utf8(field).map_err(|error| {
            WorktreeListError::InvalidPorcelain(format!("field is not UTF-8: {error}"))
        })?;
        let (key, value) = text.split_once(' ').unwrap_or((text, ""));
        if key == "worktree" {
            if current.is_some() {
                return Err(WorktreeListError::InvalidPorcelain(
                    "worktree record missing separator".into(),
                ));
            }
            current = Some(RawWorktree {
                path: PathBuf::from(value),
                ..RawWorktree::default()
            });
            continue;
        }

        let row = current.as_mut().ok_or_else(|| {
            WorktreeListError::InvalidPorcelain(format!("field {key:?} appeared before worktree"))
        })?;
        match key {
            "HEAD" => row.head = value.to_string(),
            "branch" => {
                row.branch = Some(
                    value
                        .strip_prefix("refs/heads/")
                        .unwrap_or(value)
                        .to_string(),
                )
            }
            "detached" => row.branch = None,
            "bare" => row.is_bare = true,
            "locked" => row.lock_reason = Some(value.to_string()),
            "prunable" => row.prunable_reason = Some(value.to_string()),
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
