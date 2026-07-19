// SPDX-License-Identifier: GPL-3.0-or-later
//! Ignore-aware workspace file indexing and text search.

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::{DirEntry, WalkBuilder};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use thiserror::Error;

pub const DEFAULT_MAX_SEARCH_RESULTS: usize = 500;
pub const DEFAULT_MAX_SEARCH_FILE_BYTES: u64 = 2 * 1024 * 1024;

const GENERATED_DIRECTORIES: &[&str] = &[".git", "node_modules", "target"];

#[derive(Clone, Debug, Default)]
pub struct SearchCancellation(Arc<AtomicBool>);

impl SearchCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchDocument {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub use_regex: bool,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub max_results: usize,
    pub max_file_bytes: u64,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            case_sensitive: false,
            whole_word: false,
            use_regex: false,
            include: Vec::new(),
            exclude: Vec::new(),
            max_results: DEFAULT_MAX_SEARCH_RESULTS,
            max_file_bytes: DEFAULT_MAX_SEARCH_FILE_BYTES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSearchMatch {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub length: u32,
    pub preview: String,
    pub preview_column: u32,
    pub preview_length: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSearchResult {
    pub matches: Vec<WorkspaceSearchMatch>,
    pub truncated: bool,
    pub cancelled: bool,
}

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("invalid search pattern: {0}")]
    InvalidPattern(#[from] regex::Error),
    #[error("invalid search glob '{pattern}': {source}")]
    InvalidGlob {
        pattern: String,
        #[source]
        source: globset::Error,
    },
}

pub fn index_workspace_files(
    root: &Path,
    limit: usize,
    cancellation: &SearchCancellation,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in workspace_walker(root) {
        if cancellation.is_cancelled() || files.len() >= limit {
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if is_searchable_file(&entry) {
            files.push(entry.into_path());
        }
    }
    files.sort_by_key(|path| (path.components().count(), path.clone()));
    files
}

pub fn search_workspace(
    root: &Path,
    query: &str,
    options: &SearchOptions,
    open_documents: &[SearchDocument],
    cancellation: &SearchCancellation,
) -> Result<WorkspaceSearchResult, SearchError> {
    if query.is_empty() || options.max_results == 0 {
        return Ok(WorkspaceSearchResult::default());
    }
    let matcher = build_matcher(query, options)?;
    let include = build_globs(&options.include)?;
    let exclude = build_globs(&options.exclude)?;
    let mut result = WorkspaceSearchResult::default();
    let mut overridden_paths = HashSet::new();
    let root_identity = normalized_identity(root);

    for document in open_documents {
        if cancellation.is_cancelled() {
            result.cancelled = true;
            return Ok(result);
        }
        let document_identity = normalized_identity(&document.path);
        let Some(relative) = relative_path(&root_identity, &document_identity) else {
            continue;
        };
        // The open buffer always wins over its disk copy, even when the buffer
        // itself is too large to search: the disk copy is stale and its match
        // ranges would highlight the wrong text.
        overridden_paths.insert(document_identity.clone());
        if document.content.len() as u64 > options.max_file_bytes {
            continue;
        }
        if !path_is_included(relative, include.as_ref(), exclude.as_ref()) {
            continue;
        }
        collect_matches(
            relative,
            &document.content,
            &matcher,
            options.max_results,
            &mut result,
            cancellation,
        );
        if result.truncated || result.cancelled {
            return Ok(result);
        }
    }

    for entry in workspace_walker(root) {
        if cancellation.is_cancelled() {
            result.cancelled = true;
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if !is_searchable_file(&entry)
            || overridden_paths.contains(&normalized_identity(entry.path()))
        {
            continue;
        }
        let Some(relative) = relative_path(root, entry.path()) else {
            continue;
        };
        if !path_is_included(relative, include.as_ref(), exclude.as_ref()) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > options.max_file_bytes {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path()) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let Ok(content) = std::str::from_utf8(&bytes) else {
            continue;
        };
        collect_matches(
            relative,
            content,
            &matcher,
            options.max_results,
            &mut result,
            cancellation,
        );
        if result.truncated || result.cancelled {
            break;
        }
    }
    Ok(result)
}

fn workspace_walker(root: &Path) -> ignore::Walk {
    let mut builder = WalkBuilder::new(root);
    builder
        .follow_links(false)
        .hidden(true)
        .parents(true)
        .require_git(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .sort_by_file_path(|left, right| left.cmp(right))
        .filter_entry(|entry| !is_generated_directory(entry));
    builder.build()
}

fn is_generated_directory(entry: &DirEntry) -> bool {
    entry.file_type().is_some_and(|kind| kind.is_dir())
        && entry
            .file_name()
            .to_str()
            .is_some_and(|name| GENERATED_DIRECTORIES.contains(&name))
}

fn is_searchable_file(entry: &DirEntry) -> bool {
    entry.file_type().is_some_and(|kind| kind.is_file())
}

fn build_matcher(query: &str, options: &SearchOptions) -> Result<Regex, regex::Error> {
    let pattern = if options.use_regex {
        query.to_string()
    } else {
        regex::escape(query)
    };
    let pattern = if options.whole_word {
        format!(r"\b(?:{pattern})\b")
    } else {
        pattern
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(!options.case_sensitive)
        .build()
}

fn build_globs(patterns: &[String]) -> Result<Option<GlobSet>, SearchError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|source| SearchError::InvalidGlob {
            pattern: pattern.clone(),
            source,
        })?;
        builder.add(glob);
    }
    builder
        .build()
        .map(Some)
        .map_err(|source| SearchError::InvalidGlob {
            pattern: patterns.join(", "),
            source,
        })
}

fn path_is_included(path: &Path, include: Option<&GlobSet>, exclude: Option<&GlobSet>) -> bool {
    include.is_none_or(|patterns| patterns.is_match(path))
        && !exclude.is_some_and(|patterns| patterns.is_match(path))
}

fn relative_path<'a>(root: &Path, path: &'a Path) -> Option<&'a Path> {
    path.strip_prefix(root).ok()
}

fn normalized_identity(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn collect_matches(
    path: &Path,
    content: &str,
    matcher: &Regex,
    max_results: usize,
    result: &mut WorkspaceSearchResult,
    cancellation: &SearchCancellation,
) {
    let Some(path) = path.to_str() else {
        return;
    };
    for (line_index, raw_line) in content.split('\n').enumerate() {
        if cancellation.is_cancelled() {
            result.cancelled = true;
            return;
        }
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        for found in matcher.find_iter(line) {
            if result.matches.len() >= max_results {
                result.truncated = true;
                return;
            }
            let (preview, preview_column, preview_length) = match_preview(line, &found);
            result.matches.push(WorkspaceSearchMatch {
                path: path.to_string(),
                line: u32::try_from(line_index).unwrap_or(u32::MAX),
                column: utf16_len(&line[..found.start()]),
                length: utf16_len(found.as_str()),
                preview,
                preview_column,
                preview_length,
            });
        }
    }
}

fn match_preview(line: &str, found: &regex::Match<'_>) -> (String, u32, u32) {
    const BEFORE_CHARS: usize = 80;
    const MATCH_CHARS: usize = 120;
    const AFTER_CHARS: usize = 80;

    let preview_start = line[..found.start()]
        .char_indices()
        .rev()
        .nth(BEFORE_CHARS.saturating_sub(1))
        .map(|(index, _)| index)
        .unwrap_or(0);
    let preview_match_end = line[found.start()..found.end()]
        .char_indices()
        .nth(MATCH_CHARS)
        .map(|(index, _)| found.start() + index)
        .unwrap_or(found.end());
    let preview_end = line[preview_match_end..]
        .char_indices()
        .nth(AFTER_CHARS)
        .map(|(index, _)| preview_match_end + index)
        .unwrap_or(line.len());
    let has_prefix = preview_start > 0;
    let has_suffix = preview_end < line.len();
    let mut preview = String::new();
    if has_prefix {
        preview.push('…');
    }
    preview.push_str(&line[preview_start..preview_end]);
    if has_suffix {
        preview.push('…');
    }
    let prefix_width = u32::from(has_prefix);
    (
        preview,
        prefix_width + utf16_len(&line[preview_start..found.start()]),
        utf16_len(&line[found.start()..preview_match_end]),
    )
}

fn utf16_len(value: &str) -> u32 {
    u32::try_from(value.encode_utf16().count()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn index_respects_gitignore_hidden_and_generated_directories() {
        let workspace = tempdir().unwrap();
        fs::write(workspace.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(workspace.path().join("visible.txt"), "visible").unwrap();
        fs::write(workspace.path().join("ignored.txt"), "ignored").unwrap();
        fs::write(workspace.path().join(".secret"), "hidden").unwrap();
        fs::create_dir(workspace.path().join("target")).unwrap();
        fs::write(workspace.path().join("target/generated.rs"), "generated").unwrap();

        assert_eq!(
            index_workspace_files(workspace.path(), 20, &SearchCancellation::default()),
            vec![workspace.path().join("visible.txt")]
        );
    }

    #[test]
    fn search_uses_dirty_override_and_reports_utf16_ranges() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("다국어.txt");
        fs::write(&path, "disk only\n").unwrap();
        let result = search_workspace(
            workspace.path(),
            "검색🙂",
            &SearchOptions::default(),
            &[SearchDocument {
                path: path.clone(),
                content: "앞🙂 검색🙂 뒤\n".into(),
            }],
            &SearchCancellation::default(),
        )
        .unwrap();

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].path, "다국어.txt");
        assert_eq!(result.matches[0].line, 0);
        assert_eq!(result.matches[0].column, 4);
        assert_eq!(result.matches[0].length, 4);
        assert_eq!(result.matches[0].preview, "앞🙂 검색🙂 뒤");
        assert_eq!(result.matches[0].preview_column, 4);
        assert_eq!(result.matches[0].preview_length, 4);
    }

    #[test]
    fn options_apply_regex_case_word_and_glob_filters() {
        let workspace = tempdir().unwrap();
        fs::create_dir(workspace.path().join("src")).unwrap();
        fs::write(workspace.path().join("src/main.rs"), "Cat catalog CAT\n").unwrap();
        fs::write(workspace.path().join("src/skip.rs"), "Cat\n").unwrap();
        fs::write(workspace.path().join("notes.txt"), "Cat\n").unwrap();
        let options = SearchOptions {
            case_sensitive: true,
            whole_word: true,
            use_regex: true,
            include: vec!["src/**".into()],
            exclude: vec!["**/skip.rs".into()],
            ..SearchOptions::default()
        };

        let result = search_workspace(
            workspace.path(),
            "C.t",
            &options,
            &[],
            &SearchCancellation::default(),
        )
        .unwrap();

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].path, "src/main.rs");
        assert_eq!(result.matches[0].column, 0);
    }

    #[test]
    fn result_limit_and_cancellation_are_explicit() {
        let workspace = tempdir().unwrap();
        fs::write(workspace.path().join("many.txt"), "hit hit hit\n").unwrap();
        let options = SearchOptions {
            max_results: 2,
            ..SearchOptions::default()
        };
        let result = search_workspace(
            workspace.path(),
            "hit",
            &options,
            &[],
            &SearchCancellation::default(),
        )
        .unwrap();
        assert_eq!(result.matches.len(), 2);
        assert!(result.truncated);

        let cancellation = SearchCancellation::default();
        cancellation.cancel();
        let result = search_workspace(
            workspace.path(),
            "hit",
            &SearchOptions::default(),
            &[],
            &cancellation,
        )
        .unwrap();
        assert!(result.cancelled);
        assert!(result.matches.is_empty());
    }

    #[test]
    fn invalid_regex_and_glob_return_errors() {
        let workspace = tempdir().unwrap();
        let regex_error = search_workspace(
            workspace.path(),
            "(",
            &SearchOptions {
                use_regex: true,
                ..SearchOptions::default()
            },
            &[],
            &SearchCancellation::default(),
        )
        .unwrap_err();
        assert!(matches!(regex_error, SearchError::InvalidPattern(_)));

        let glob_error = search_workspace(
            workspace.path(),
            "value",
            &SearchOptions {
                include: vec!["[".into()],
                ..SearchOptions::default()
            },
            &[],
            &SearchCancellation::default(),
        )
        .unwrap_err();
        assert!(matches!(glob_error, SearchError::InvalidGlob { .. }));
    }

    #[test]
    fn dirty_override_outside_workspace_is_not_searched() {
        let workspace = tempdir().unwrap();
        let external = tempdir().unwrap();
        fs::write(external.path().join("outside.txt"), "secret").unwrap();
        let result = search_workspace(
            workspace.path(),
            "secret",
            &SearchOptions::default(),
            &[SearchDocument {
                path: external.path().join("outside.txt"),
                content: "secret".into(),
            }],
            &SearchCancellation::default(),
        )
        .unwrap();

        assert!(result.matches.is_empty());

        let escaped = workspace.path().join("..").join(
            external
                .path()
                .file_name()
                .expect("temporary directory has a file name"),
        );
        let result = search_workspace(
            workspace.path(),
            "secret",
            &SearchOptions::default(),
            &[SearchDocument {
                path: escaped.join("outside.txt"),
                content: "secret".into(),
            }],
            &SearchCancellation::default(),
        )
        .unwrap();
        assert!(result.matches.is_empty());
    }

    #[test]
    fn very_long_lines_have_bounded_highlightable_previews() {
        let workspace = tempdir().unwrap();
        let content = format!("{}needle{}", "앞".repeat(500), "뒤".repeat(500));
        fs::write(workspace.path().join("long.txt"), content).unwrap();
        let result = search_workspace(
            workspace.path(),
            "needle",
            &SearchOptions::default(),
            &[],
            &SearchCancellation::default(),
        )
        .unwrap();

        let found = &result.matches[0];
        assert!(found.preview.chars().count() <= 168);
        assert!(found.preview.starts_with('…'));
        assert!(found.preview.ends_with('…'));
        assert_eq!(found.preview_column, 81);
        assert_eq!(found.preview_length, 6);
    }
}
