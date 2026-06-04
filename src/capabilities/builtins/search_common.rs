use super::{format_tool_path_scope_block, resolve_existing_sandbox_path};
use crate::sandbox::config::{ResolvedSandbox, resolve_sandbox};
use crate::sandbox::permissions::{AccessKind, ensure_path_allowed};
use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_SEARCH_PATH: &str = ".";
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 500;
pub const MAX_GLOB_RESULTS: usize = 50_000;
pub const MAX_LINE_CHARS: usize = 2_000;
pub const MAX_CONTEXT_LINES: usize = 5;
pub const BINARY_SAMPLE_BYTES: usize = 8_192;

#[derive(Debug, Clone, Copy)]
pub struct Paging {
    pub offset: usize,
    pub limit: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct SearchWarnings {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<String>,
    pub skipped_walk_errors: usize,
    pub skipped_sandbox_denied: usize,
    pub skipped_binary_files: usize,
    pub skipped_io_errors: usize,
}

pub struct SearchRoot {
    pub path: PathBuf,
    pub workspace: PathBuf,
    pub sandbox: ResolvedSandbox,
}

#[derive(Debug, Clone, Copy)]
pub struct WalkOptions {
    pub include_hidden: bool,
    pub follow_symlinks: bool,
    pub respect_ignore: bool,
}

pub fn default_path() -> String {
    DEFAULT_SEARCH_PATH.to_string()
}

pub fn default_respect_ignore() -> bool {
    true
}

pub fn parse_paging(
    capability: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<Paging> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 {
        bail!("{capability}.limit must be >= 1");
    }
    Ok(Paging {
        offset: offset.unwrap_or(0),
        limit: limit.min(MAX_LIMIT),
    })
}

pub fn next_offset(paging: Paging, returned: usize, has_more: bool) -> Option<usize> {
    if has_more {
        Some(paging.offset.saturating_add(returned))
    } else {
        None
    }
}

pub fn resolve_read_root(capability: &str, requested: &str) -> Result<SearchRoot> {
    let path = resolve_existing_sandbox_path(requested).map_err(|error| {
        anyhow::anyhow!(format_tool_path_scope_block(
            capability, "read", requested, error
        ))
    })?;
    let workspace = std::env::current_dir()?.canonicalize()?;
    let sandbox = resolve_sandbox()?;
    ensure_path_allowed(&sandbox, AccessKind::Read, &path, &workspace, capability)?;
    Ok(SearchRoot {
        path,
        workspace,
        sandbox,
    })
}

pub fn ensure_entry_read_allowed(
    root: &SearchRoot,
    capability: &str,
    path: &Path,
    warnings: &mut SearchWarnings,
) -> bool {
    match ensure_path_allowed(
        &root.sandbox,
        AccessKind::Read,
        path,
        &root.workspace,
        capability,
    ) {
        Ok(()) => true,
        Err(_) => {
            warnings.skipped_sandbox_denied += 1;
            false
        }
    }
}

pub fn build_walker(root: &Path, options: WalkOptions) -> ignore::Walk {
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(options.respect_ignore)
        .hidden(!options.include_hidden)
        .follow_links(options.follow_symlinks);
    if !options.respect_ignore {
        builder
            .parents(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false);
    }
    builder.build()
}

pub fn compile_globset(patterns: &[String]) -> Result<Option<GlobSet>> {
    let patterns = patterns
        .iter()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
        .collect::<Vec<_>>();
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        for candidate in glob_candidates(pattern) {
            builder.add(
                Glob::new(&candidate)
                    .with_context(|| format!("invalid glob pattern: {pattern}"))?,
            );
        }
    }
    Ok(Some(
        builder.build().context("failed to build glob matcher")?,
    ))
}

pub fn path_matches(globs: &GlobSet, root: &Path, path: &Path) -> bool {
    let relative = normalized_relative_path(root, path);
    if globs.is_match(&relative) {
        return true;
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| globs.is_match(normalize_path_text(name)))
        .unwrap_or(false)
}

pub fn display_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub fn normalized_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub fn normalize_path_text(path: &str) -> String {
    path.replace('\\', "/")
}

pub fn glob_candidates(pattern: &str) -> Vec<String> {
    let normalized = normalize_path_text(pattern.trim());
    if normalized.contains('/') || normalized.starts_with("**") {
        return vec![normalized];
    }
    vec![normalized.clone(), format!("**/{normalized}")]
}

pub fn truncate_text(text: &str, max_chars: usize) -> (String, bool) {
    let mut chars = text.chars();
    let truncated = text.chars().count() > max_chars;
    if !truncated {
        return (text.to_string(), false);
    }
    let mut out = chars.by_ref().take(max_chars).collect::<String>();
    out.push_str("...");
    (out, true)
}

pub fn is_file_entry(entry: &ignore::DirEntry) -> bool {
    entry
        .file_type()
        .map(|file_type| file_type.is_file())
        .unwrap_or(false)
}
