use super::{format_tool_path_scope_block, resolve_existing_sandbox_path};
use crate::sandbox::config::resolve_sandbox;
use crate::sandbox::permissions::{AccessKind, ensure_path_allowed};
use crate::session::SessionManager;
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditArgs {
    /// Existing UTF-8 text file to edit.
    pub path: String,
    /// Exact text to replace. Must be non-empty and must match exactly after line-ending normalization.
    pub old_string: String,
    /// Replacement text. Must differ from old_string.
    pub new_string: String,
    /// Replace all occurrences. Defaults to false, which requires old_string to match exactly once.
    #[serde(default)]
    pub replace_all: bool,
    /// Optional safety check for how many replacements should be applied.
    #[serde(default)]
    pub expected_replacements: Option<usize>,
}

pub const DESCRIPTION: &str = concat!(
    "Edit one existing UTF-8 text file by exact string replacement. ",
    "Use this for small targeted changes after search_content/read_file. ",
    "By default old_string must match exactly once; use replace_all=true only for intentional repeated replacements. ",
    "Use write_file for full file overwrite or apply_patch for multi-file/structural edits."
);

pub fn execute(args: Value) -> Result<String> {
    execute_inner(args, None)
}

pub fn execute_with_session(
    args: Value,
    session_manager: &SessionManager,
    session_id: &str,
) -> Result<String> {
    execute_inner(args, Some((session_manager, session_id)))
}

fn execute_inner(args: Value, session: Option<(&SessionManager, &str)>) -> Result<String> {
    let input: EditArgs = serde_json::from_value(args).context("failed to parse edit args")?;
    if input.old_string.is_empty() {
        bail!("edit.old_string must be non-empty; use write_file to create or overwrite a file");
    }
    if input.old_string == input.new_string {
        bail!("edit.old_string and edit.new_string must differ");
    }
    if matches!(input.expected_replacements, Some(0)) {
        bail!("edit.expected_replacements must be >= 1 when provided");
    }

    let path = resolve_existing_sandbox_path(&input.path).map_err(|error| {
        anyhow::anyhow!(format_tool_path_scope_block(
            "edit",
            "write",
            &input.path,
            error
        ))
    })?;
    let workspace = std::env::current_dir()?;
    let sandbox = resolve_sandbox()?;
    ensure_path_allowed(&sandbox, AccessKind::Read, &path, &workspace, "edit")?;
    ensure_path_allowed(&sandbox, AccessKind::Write, &path, &workspace, "edit")?;

    let metadata =
        fs::metadata(&path).with_context(|| format!("failed to stat file: {}", path.display()))?;
    if metadata.is_dir() {
        bail!(
            "edit.path must be a file, not a directory: {}",
            path.display()
        );
    }

    let bytes =
        fs::read(&path).with_context(|| format!("failed to read file: {}", path.display()))?;
    if is_probably_binary(&bytes) {
        bail!(
            "edit only supports UTF-8 text files; binary data detected: {}",
            path.display()
        );
    }
    let sha256_before = sha256_hex(&bytes);
    let content = String::from_utf8(bytes)
        .with_context(|| format!("file is not valid UTF-8 text: {}", path.display()))?;
    let line_ending = detect_line_ending(&content);
    let old_string =
        convert_to_line_ending(&normalize_line_endings(&input.old_string), line_ending);
    let new_string =
        convert_to_line_ending(&normalize_line_endings(&input.new_string), line_ending);

    let occurrences = count_occurrences(&content, &old_string);
    if occurrences == 0 {
        bail!("edit.old_string was not found in {}", path.display());
    }
    let replacements = if input.replace_all {
        occurrences
    } else {
        if occurrences > 1 {
            bail!(
                "edit.old_string matched {occurrences} times in {}; provide more surrounding context or set replace_all=true",
                path.display()
            );
        }
        1
    };
    if let Some(expected) = input.expected_replacements {
        if expected != replacements {
            bail!(
                "edit expected {expected} replacement(s), but would apply {replacements} replacement(s)"
            );
        }
    }

    let new_content = if input.replace_all {
        content.replace(&old_string, &new_string)
    } else {
        content.replacen(&old_string, &new_string, 1)
    };
    let new_bytes = new_content.as_bytes();
    let snapshot = if let Some((session_manager, session_id)) = session {
        Some(session_manager.capture_file_snapshot_before(session_id, "edit", &path)?)
    } else {
        None
    };
    fs::write(&path, new_bytes)
        .with_context(|| format!("failed to write edited file: {}", path.display()))?;
    if let (Some((session_manager, session_id)), Some(snapshot)) = (session, snapshot) {
        session_manager.append_file_snapshot_after(session_id, snapshot)?;
    }
    let sha256_after = sha256_hex(new_bytes);

    Ok(serde_json::to_string_pretty(&json!({
        "status": "ok",
        "path": path.display().to_string(),
        "replacements": replacements,
        "replace_all": input.replace_all,
        "sha256_before": sha256_before,
        "sha256_after": sha256_after,
        "bytes_written": new_bytes.len(),
        "line_ending": match line_ending {
            LineEnding::Lf => "lf",
            LineEnding::Crlf => "crlf",
        },
    }))?)
}

#[derive(Debug, Clone, Copy)]
enum LineEnding {
    Lf,
    Crlf,
}

fn detect_line_ending(text: &str) -> LineEnding {
    if text.contains("\r\n") {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    }
}

fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn convert_to_line_ending(text: &str, line_ending: LineEnding) -> String {
    match line_ending {
        LineEnding::Lf => text.to_string(),
        LineEnding::Crlf => text.replace('\n', "\r\n"),
    }
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut start = 0usize;
    while let Some(index) = haystack[start..].find(needle) {
        count += 1;
        start += index + needle.len();
    }
    count
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_probably_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|byte| *byte == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn edits_unique_match() -> Result<()> {
        let path = test_workspace_file("sample.txt")?;
        fs::write(&path, "alpha\nbeta\n")?;
        let output = execute(json!({
            "path": path.to_string_lossy(),
            "old_string": "beta",
            "new_string": "gamma",
            "expected_replacements": 1
        }))?;
        assert!(output.contains("\"replacements\": 1"));
        assert_eq!(fs::read_to_string(&path)?, "alpha\ngamma\n");
        Ok(())
    }

    #[test]
    fn rejects_ambiguous_match_without_replace_all() -> Result<()> {
        let path = test_workspace_file("ambiguous.txt")?;
        fs::write(&path, "same\nsame\n")?;
        let error = execute(json!({
            "path": path.to_string_lossy(),
            "old_string": "same",
            "new_string": "other"
        }))
        .expect_err("multiple matches should fail");
        assert!(format!("{error:#}").contains("matched 2 times"));
        Ok(())
    }

    fn test_workspace_file(name: &str) -> Result<std::path::PathBuf> {
        let dir = std::env::current_dir()?
            .join("target")
            .join("duckagent-edit-tests")
            .join(Uuid::now_v7().to_string());
        fs::create_dir_all(&dir)?;
        Ok(dir.join(name))
    }
}
