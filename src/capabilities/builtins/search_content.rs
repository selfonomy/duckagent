use super::search_common::{
    BINARY_SAMPLE_BYTES, MAX_CONTEXT_LINES, MAX_LINE_CHARS, SearchWarnings, WalkOptions,
    compile_globset, default_path, default_respect_ignore, display_path, ensure_entry_read_allowed,
    is_file_entry, next_offset, parse_paging, path_matches, resolve_read_root, truncate_text,
};
use anyhow::{Context, Result, bail};
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchContentOutputMode {
    #[default]
    Matches,
    FilesOnly,
    Count,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchContentArgs {
    /// File or directory to search. Defaults to the current workspace directory.
    #[serde(default = "default_path")]
    pub path: String,
    /// Literal text or regex pattern to search for.
    pub query: String,
    /// Treat query as a regex. Defaults to false, so queries are literal and safer for AI use.
    #[serde(default)]
    pub regex: bool,
    /// Use case-sensitive matching. Defaults to false.
    #[serde(default)]
    pub case_sensitive: bool,
    /// File glob filters to include, for example ["*.rs", "*.{ts,tsx}"]. Empty means all text files.
    #[serde(default)]
    pub include_globs: Vec<String>,
    /// File glob filters to exclude.
    #[serde(default)]
    pub exclude_globs: Vec<String>,
    /// Number of context lines before and after each match. Defaults to 0 and is capped at 5.
    #[serde(default)]
    pub context_lines: Option<usize>,
    /// Output shape: matches, files_only, or count. Defaults to matches.
    #[serde(default)]
    pub output_mode: SearchContentOutputMode,
    /// Include hidden files and directories. Defaults to false.
    #[serde(default)]
    pub include_hidden: bool,
    /// Follow symbolic links while walking. Defaults to false.
    #[serde(default)]
    pub follow_symlinks: bool,
    /// Respect .gitignore, .ignore, .rgignore, git exclude, and global gitignore. Defaults to true.
    #[serde(default = "default_respect_ignore")]
    pub respect_ignore: bool,
    /// Result offset for pagination. In matches mode this is match-line offset; otherwise it is file offset.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum number of results to return. Defaults to 100 and is capped at 500.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
struct LineContext {
    line: usize,
    text: String,
    line_truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
struct Submatch {
    start_byte: usize,
    end_byte: usize,
    text: String,
}

#[derive(Debug, Clone, Serialize)]
struct ContentMatch {
    path: String,
    line: usize,
    text: String,
    submatches: Vec<Submatch>,
    line_truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    before: Vec<LineContext>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    after: Vec<LineContext>,
}

#[derive(Debug, Clone, Serialize)]
struct FileCount {
    path: String,
    count: usize,
}

pub const DESCRIPTION: &str = concat!(
    "Search text inside files using Rust regex plus the ignore/ripgrep-style walker. ",
    "Use this instead of shell grep/rg when you need structured file, line, and match data. ",
    "By default query is literal, case-insensitive, paginated, ignores hidden files, and respects .gitignore/.ignore/.rgignore. ",
    "Use output_mode=files_only to discover files, or output_mode=count for per-file counts."
);

pub fn execute(args: Value) -> Result<String> {
    let input: SearchContentArgs =
        serde_json::from_value(args).context("failed to parse search_content args")?;
    if input.query.trim().is_empty() {
        bail!("search_content.query must be non-empty");
    }
    let paging = parse_paging("search_content", input.offset, input.limit)?;
    let context_lines = input.context_lines.unwrap_or(0).min(MAX_CONTEXT_LINES);
    let root = resolve_read_root("search_content", &input.path)?;
    let metadata = fs::metadata(&root.path)
        .with_context(|| format!("failed to stat search path: {}", root.path.display()))?;

    let pattern = if input.regex {
        input.query.clone()
    } else {
        regex::escape(&input.query)
    };
    let matcher = RegexBuilder::new(&pattern)
        .case_insensitive(!input.case_sensitive)
        .build()
        .with_context(|| "failed to compile search_content query")?;
    let include_globs = compile_globset(&input.include_globs)?;
    let exclude_globs = compile_globset(&input.exclude_globs)?;
    let walk_options = WalkOptions {
        include_hidden: input.include_hidden,
        follow_symlinks: input.follow_symlinks,
        respect_ignore: input.respect_ignore,
    };
    let mut warnings = SearchWarnings::default();

    let search_base = if metadata.is_file() {
        root.path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.path.clone())
    } else {
        root.path.clone()
    };

    let response = match input.output_mode {
        SearchContentOutputMode::Matches => {
            let (mut matches, has_more) = search_matches(
                &root,
                &search_base,
                metadata.is_file(),
                walk_options,
                &matcher,
                include_globs.as_ref(),
                exclude_globs.as_ref(),
                context_lines,
                paging,
                &mut warnings,
            )?;
            if matches.len() > paging.limit {
                matches.truncate(paging.limit);
            }
            json!({
                "status": "ok",
                "path": input.path,
                "query": input.query,
                "regex": input.regex,
                "case_sensitive": input.case_sensitive,
                "output_mode": input.output_mode,
                "offset": paging.offset,
                "limit": paging.limit,
                "returned": matches.len(),
                "has_more": has_more,
                "next_offset": next_offset(paging, matches.len(), has_more),
                "matches": matches,
                "warnings": warnings,
            })
        }
        SearchContentOutputMode::FilesOnly => {
            let (mut files, has_more) = search_files_only(
                &root,
                &search_base,
                metadata.is_file(),
                walk_options,
                &matcher,
                include_globs.as_ref(),
                exclude_globs.as_ref(),
                paging,
                &mut warnings,
            )?;
            if files.len() > paging.limit {
                files.truncate(paging.limit);
            }
            json!({
                "status": "ok",
                "path": input.path,
                "query": input.query,
                "regex": input.regex,
                "case_sensitive": input.case_sensitive,
                "output_mode": input.output_mode,
                "offset": paging.offset,
                "limit": paging.limit,
                "returned": files.len(),
                "has_more": has_more,
                "next_offset": next_offset(paging, files.len(), has_more),
                "files": files,
                "warnings": warnings,
            })
        }
        SearchContentOutputMode::Count => {
            let (mut counts, has_more) = search_counts(
                &root,
                &search_base,
                metadata.is_file(),
                walk_options,
                &matcher,
                include_globs.as_ref(),
                exclude_globs.as_ref(),
                paging,
                &mut warnings,
            )?;
            if counts.len() > paging.limit {
                counts.truncate(paging.limit);
            }
            json!({
                "status": "ok",
                "path": input.path,
                "query": input.query,
                "regex": input.regex,
                "case_sensitive": input.case_sensitive,
                "output_mode": input.output_mode,
                "offset": paging.offset,
                "limit": paging.limit,
                "returned": counts.len(),
                "has_more": has_more,
                "next_offset": next_offset(paging, counts.len(), has_more),
                "counts": counts,
                "warnings": warnings,
            })
        }
    };

    Ok(serde_json::to_string_pretty(&response)?)
}

fn candidate_files<'a>(
    root: &'a super::search_common::SearchRoot,
    search_base: &'a Path,
    single_file: bool,
    walk_options: WalkOptions,
    include_globs: Option<&'a globset::GlobSet>,
    exclude_globs: Option<&'a globset::GlobSet>,
    warnings: &'a mut SearchWarnings,
) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    if single_file {
        if file_passes_globs(search_base, &root.path, include_globs, exclude_globs)
            && ensure_entry_read_allowed(root, "search_content", &root.path, warnings)
        {
            files.push(root.path.clone());
        }
        return Ok(files);
    }
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
        if !file_passes_globs(&root.path, path, include_globs, exclude_globs) {
            continue;
        }
        if ensure_entry_read_allowed(root, "search_content", path, warnings) {
            files.push(path.to_path_buf());
        }
    }
    Ok(files)
}

fn file_passes_globs(
    base: &Path,
    path: &Path,
    include_globs: Option<&globset::GlobSet>,
    exclude_globs: Option<&globset::GlobSet>,
) -> bool {
    if let Some(exclude_globs) = exclude_globs {
        if path_matches(exclude_globs, base, path) {
            return false;
        }
    }
    if let Some(include_globs) = include_globs {
        return path_matches(include_globs, base, path);
    }
    true
}

fn search_matches(
    root: &super::search_common::SearchRoot,
    search_base: &Path,
    single_file: bool,
    walk_options: WalkOptions,
    matcher: &regex::Regex,
    include_globs: Option<&globset::GlobSet>,
    exclude_globs: Option<&globset::GlobSet>,
    context_lines: usize,
    paging: super::search_common::Paging,
    warnings: &mut SearchWarnings,
) -> Result<(Vec<ContentMatch>, bool)> {
    let files = candidate_files(
        root,
        search_base,
        single_file,
        walk_options,
        include_globs,
        exclude_globs,
        warnings,
    )?;
    let mut seen = 0usize;
    let mut out = Vec::<ContentMatch>::new();
    for file in files {
        scan_file_matches(
            root,
            &file,
            matcher,
            context_lines,
            paging,
            &mut seen,
            &mut out,
            warnings,
        )?;
        if out.len() > paging.limit {
            return Ok((out, true));
        }
    }
    Ok((out, false))
}

fn search_files_only(
    root: &super::search_common::SearchRoot,
    search_base: &Path,
    single_file: bool,
    walk_options: WalkOptions,
    matcher: &regex::Regex,
    include_globs: Option<&globset::GlobSet>,
    exclude_globs: Option<&globset::GlobSet>,
    paging: super::search_common::Paging,
    warnings: &mut SearchWarnings,
) -> Result<(Vec<String>, bool)> {
    let files = candidate_files(
        root,
        search_base,
        single_file,
        walk_options,
        include_globs,
        exclude_globs,
        warnings,
    )?;
    let mut seen = 0usize;
    let mut out = Vec::<String>::new();
    for file in files {
        if file_has_match(&file, matcher, warnings)? {
            let index = seen;
            seen += 1;
            if index < paging.offset {
                continue;
            }
            out.push(display_path(&root.workspace, &file));
            if out.len() > paging.limit {
                return Ok((out, true));
            }
        }
    }
    Ok((out, false))
}

fn search_counts(
    root: &super::search_common::SearchRoot,
    search_base: &Path,
    single_file: bool,
    walk_options: WalkOptions,
    matcher: &regex::Regex,
    include_globs: Option<&globset::GlobSet>,
    exclude_globs: Option<&globset::GlobSet>,
    paging: super::search_common::Paging,
    warnings: &mut SearchWarnings,
) -> Result<(Vec<FileCount>, bool)> {
    let files = candidate_files(
        root,
        search_base,
        single_file,
        walk_options,
        include_globs,
        exclude_globs,
        warnings,
    )?;
    let mut seen = 0usize;
    let mut out = Vec::<FileCount>::new();
    for file in files {
        let count = file_match_count(&file, matcher, warnings)?;
        if count == 0 {
            continue;
        }
        let index = seen;
        seen += 1;
        if index < paging.offset {
            continue;
        }
        out.push(FileCount {
            path: display_path(&root.workspace, &file),
            count,
        });
        if out.len() > paging.limit {
            return Ok((out, true));
        }
    }
    Ok((out, false))
}

fn scan_file_matches(
    root: &super::search_common::SearchRoot,
    path: &Path,
    matcher: &regex::Regex,
    context_lines: usize,
    paging: super::search_common::Paging,
    seen: &mut usize,
    out: &mut Vec<ContentMatch>,
    warnings: &mut SearchWarnings,
) -> Result<()> {
    if is_probably_binary(path, warnings)? {
        return Ok(());
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => {
            warnings.skipped_io_errors += 1;
            return Ok(());
        }
    };
    let mut reader = BufReader::new(file);
    let mut raw = Vec::<u8>::new();
    let mut line_no = 0usize;
    let mut before = VecDeque::<LineContext>::new();
    let mut pending_after = Vec::<(usize, usize)>::new();

    loop {
        raw.clear();
        let bytes = reader.read_until(b'\n', &mut raw)?;
        if bytes == 0 {
            break;
        }
        line_no += 1;
        let text = bytes_to_line_text(&raw);
        let line_context = make_line_context(line_no, &text);
        for (match_index, remaining) in &mut pending_after {
            if *remaining > 0 {
                if let Some(existing) = out.get_mut(*match_index) {
                    existing.after.push(line_context.clone());
                }
                *remaining -= 1;
            }
        }
        pending_after.retain(|(_, remaining)| *remaining > 0);

        if matcher.is_match(&text) {
            let index = *seen;
            *seen += 1;
            if index >= paging.offset {
                let mut record = make_match(root, path, line_no, &text, matcher);
                if context_lines > 0 {
                    record.before = before.iter().cloned().collect();
                }
                out.push(record);
                if context_lines > 0 {
                    pending_after.push((out.len() - 1, context_lines));
                }
                if out.len() > paging.limit {
                    break;
                }
            }
        }

        if context_lines > 0 {
            before.push_back(line_context);
            while before.len() > context_lines {
                before.pop_front();
            }
        }
    }
    Ok(())
}

fn file_has_match(
    path: &Path,
    matcher: &regex::Regex,
    warnings: &mut SearchWarnings,
) -> Result<bool> {
    if is_probably_binary(path, warnings)? {
        return Ok(false);
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => {
            warnings.skipped_io_errors += 1;
            return Ok(false);
        }
    };
    let mut reader = BufReader::new(file);
    let mut raw = Vec::<u8>::new();
    loop {
        raw.clear();
        let bytes = reader.read_until(b'\n', &mut raw)?;
        if bytes == 0 {
            return Ok(false);
        }
        if matcher.is_match(&bytes_to_line_text(&raw)) {
            return Ok(true);
        }
    }
}

fn file_match_count(
    path: &Path,
    matcher: &regex::Regex,
    warnings: &mut SearchWarnings,
) -> Result<usize> {
    if is_probably_binary(path, warnings)? {
        return Ok(0);
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => {
            warnings.skipped_io_errors += 1;
            return Ok(0);
        }
    };
    let mut reader = BufReader::new(file);
    let mut raw = Vec::<u8>::new();
    let mut count = 0usize;
    loop {
        raw.clear();
        let bytes = reader.read_until(b'\n', &mut raw)?;
        if bytes == 0 {
            return Ok(count);
        }
        if matcher.is_match(&bytes_to_line_text(&raw)) {
            count += 1;
        }
    }
}

fn is_probably_binary(path: &Path, warnings: &mut SearchWarnings) -> Result<bool> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => {
            warnings.skipped_io_errors += 1;
            return Ok(true);
        }
    };
    let mut sample = [0u8; BINARY_SAMPLE_BYTES];
    let read = file.read(&mut sample)?;
    if sample[..read].contains(&0) {
        warnings.skipped_binary_files += 1;
        return Ok(true);
    }
    Ok(false)
}

fn bytes_to_line_text(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    text.trim_end_matches(['\r', '\n']).to_string()
}

fn make_line_context(line: usize, text: &str) -> LineContext {
    let (text, line_truncated) = truncate_text(text, MAX_LINE_CHARS);
    LineContext {
        line,
        text,
        line_truncated,
    }
}

fn make_match(
    root: &super::search_common::SearchRoot,
    path: &Path,
    line: usize,
    text: &str,
    matcher: &regex::Regex,
) -> ContentMatch {
    let (display_text, line_truncated) = truncate_text(text, MAX_LINE_CHARS);
    let visible_len = display_text.len();
    let submatches = matcher
        .find_iter(text)
        .filter(|matched| matched.start() < visible_len)
        .take(8)
        .map(|matched| Submatch {
            start_byte: matched.start(),
            end_byte: matched.end().min(visible_len),
            text: text[matched.start()..matched.end().min(text.len())].to_string(),
        })
        .collect::<Vec<_>>();
    ContentMatch {
        path: display_path(&root.workspace, path),
        line,
        text: display_text,
        submatches,
        line_truncated,
        before: Vec::new(),
        after: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn searches_literal_content_with_files_only() -> Result<()> {
        let dir = test_dir()?;
        fs::create_dir_all(dir.join("src"))?;
        fs::write(dir.join("src/a.rs"), "fn execute_builtin() {}\n")?;
        fs::write(dir.join("src/b.txt"), "nothing\n")?;
        let output = execute(json!({
            "path": dir.to_string_lossy(),
            "query": "execute_builtin",
            "include_globs": ["*.rs"]
        }))?;
        assert!(output.contains("\"line\": 1"));
        assert!(output.contains("execute_builtin"));
        assert!(!output.contains("b.txt"));
        Ok(())
    }

    fn test_dir() -> Result<std::path::PathBuf> {
        let dir = std::env::current_dir()?
            .join("target")
            .join("duckagent-search-content-tests")
            .join(Uuid::now_v7().to_string());
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}
