use super::search_common::{
    MAX_GLOB_RESULTS, SearchWarnings, WalkOptions, compile_globset, default_path,
    default_respect_ignore, display_path, ensure_entry_read_allowed, is_file_entry, next_offset,
    parse_paging, path_matches, resolve_read_root,
};
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum SearchFilesSortBy {
    #[default]
    Path,
    Modified,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchFilesArgs {
    /// Directory to search. Defaults to the current workspace directory.
    #[serde(default = "default_path")]
    pub path: String,
    /// Glob pattern for file paths or file names, for example "*.rs", "**/mod.rs", or "*config*".
    pub pattern: String,
    /// Include hidden files and directories. Defaults to false.
    #[serde(default)]
    pub include_hidden: bool,
    /// Follow symbolic links while walking. Defaults to false.
    #[serde(default)]
    pub follow_symlinks: bool,
    /// Respect .gitignore, .ignore, .rgignore, git exclude, and global gitignore. Defaults to true.
    #[serde(default = "default_respect_ignore")]
    pub respect_ignore: bool,
    /// Sort returned files by path or modified time. Defaults to path.
    #[serde(default)]
    pub sort_by: SearchFilesSortBy,
    /// Result offset for pagination. Defaults to 0.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum number of files to return. Defaults to 100 and is capped at 500.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug)]
struct FileHit {
    path: String,
    modified_ms: i128,
}

pub const DESCRIPTION: &str = concat!(
    "Find files by glob pattern using Rust's ignore/ripgrep-style directory walker. ",
    "Use this instead of shell find/ls/glob commands when you need paths to inspect next. ",
    "Defaults respect .gitignore, .ignore, .rgignore, git exclude, global gitignore, and skip hidden files. ",
    "Results are paginated with offset/limit."
);

pub fn execute(args: Value) -> Result<String> {
    let input: SearchFilesArgs =
        serde_json::from_value(args).context("failed to parse search_files args")?;
    if input.pattern.trim().is_empty() {
        bail!("search_files.pattern must be non-empty");
    }
    let paging = parse_paging("search_files", input.offset, input.limit)?;
    let root = resolve_read_root("search_files", &input.path)?;
    let metadata = fs::metadata(&root.path)
        .with_context(|| format!("failed to stat search path: {}", root.path.display()))?;
    if !metadata.is_dir() {
        bail!(
            "search_files.path must be a directory: {}",
            root.path.display()
        );
    }

    let globs = compile_globset(&[input.pattern.trim().to_string()])?
        .context("search_files.pattern must compile to a glob matcher")?;
    let walk_options = WalkOptions {
        include_hidden: input.include_hidden,
        follow_symlinks: input.follow_symlinks,
        respect_ignore: input.respect_ignore,
    };
    let mut warnings = SearchWarnings::default();
    let collect_cap = paging
        .offset
        .saturating_add(paging.limit)
        .saturating_add(1)
        .max(10_000)
        .min(MAX_GLOB_RESULTS);
    let mut hits = Vec::<FileHit>::new();
    let mut capped = false;

    for entry in super::search_common::build_walker(&root.path, walk_options) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.skipped_walk_errors += 1;
                if warnings.messages.len() < 5 {
                    warnings.messages.push(format!("walk error: {error}"));
                }
                continue;
            }
        };
        if !is_file_entry(&entry) {
            continue;
        }
        let path = entry.path();
        if !path_matches(&globs, &root.path, path) {
            continue;
        }
        if !ensure_entry_read_allowed(&root, "search_files", path, &mut warnings) {
            continue;
        }
        let modified_ms = match input.sort_by {
            SearchFilesSortBy::Path => 0,
            SearchFilesSortBy::Modified => entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as i128)
                .unwrap_or(0),
        };
        hits.push(FileHit {
            path: display_path(&root.workspace, path),
            modified_ms,
        });
        if hits.len() > collect_cap {
            capped = true;
            break;
        }
    }

    match input.sort_by {
        SearchFilesSortBy::Path => hits.sort_by(|left, right| left.path.cmp(&right.path)),
        SearchFilesSortBy::Modified => hits.sort_by(|left, right| {
            right
                .modified_ms
                .cmp(&left.modified_ms)
                .then_with(|| left.path.cmp(&right.path))
        }),
    }

    let start = paging.offset.min(hits.len());
    let end = start.saturating_add(paging.limit).min(hits.len());
    let mut has_more = end < hits.len() || capped;
    let mut files = hits[start..end]
        .iter()
        .map(|hit| hit.path.clone())
        .collect::<Vec<_>>();
    if files.len() > paging.limit {
        files.truncate(paging.limit);
        has_more = true;
    }
    if capped {
        warnings.messages.push(format!(
            "search_files stopped after collecting {collect_cap} matches; narrow pattern or use a deeper offset carefully"
        ));
    }

    Ok(serde_json::to_string_pretty(&json!({
        "status": "ok",
        "path": input.path,
        "pattern": input.pattern,
        "sort_by": input.sort_by,
        "offset": paging.offset,
        "limit": paging.limit,
        "returned": files.len(),
        "has_more": has_more,
        "next_offset": next_offset(paging, files.len(), has_more),
        "files": files,
        "warnings": warnings,
    }))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn finds_files_with_pagination() -> Result<()> {
        let dir = test_dir()?;
        fs::create_dir_all(dir.join("src"))?;
        fs::write(dir.join("src/a.rs"), "")?;
        fs::write(dir.join("src/b.rs"), "")?;
        fs::write(dir.join("src/c.txt"), "")?;
        let output = execute(json!({
            "path": dir.to_string_lossy(),
            "pattern": "*.rs",
            "limit": 1
        }))?;
        assert!(output.contains("\"returned\": 1"));
        assert!(output.contains("\"has_more\": true"));
        assert!(output.contains("src/a.rs"));
        Ok(())
    }

    fn test_dir() -> Result<std::path::PathBuf> {
        let dir = std::env::current_dir()?
            .join("target")
            .join("duckagent-search-files-tests")
            .join(Uuid::now_v7().to_string());
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}
