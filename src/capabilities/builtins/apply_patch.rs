use super::{format_tool_path_scope_block, resolve_writable_sandbox_path};
use crate::sandbox::config::resolve_sandbox;
use crate::sandbox::permissions::{AccessKind, ensure_path_allowed};
use crate::session::SessionManager;
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const ADD_FILE: &str = "*** Add File: ";
const DELETE_FILE: &str = "*** Delete File: ";
const UPDATE_FILE: &str = "*** Update File: ";
const MOVE_TO: &str = "*** Move to: ";
const EOF_MARKER: &str = "*** End of File";

pub const DESCRIPTION: &str = concat!(
    "Apply a Codex-style V4A patch to sandbox-writable text files. ",
    "Only accepts args {\"patch\":\"...\"}; there is no old_string/new_string replace mode. ",
    "Patch paths must be relative. Grammar: ",
    "start: begin_patch hunk+ end_patch; ",
    "begin_patch: \"*** Begin Patch\" LF; ",
    "end_patch: \"*** End Patch\" LF?; ",
    "hunk: add_hunk | delete_hunk | update_hunk; ",
    "add_hunk: \"*** Add File: \" filename LF add_line+; ",
    "delete_hunk: \"*** Delete File: \" filename LF; ",
    "update_hunk: \"*** Update File: \" filename LF change_move? change?; ",
    "change_move: \"*** Move to: \" filename LF; ",
    "change: (change_context | change_line)+ eof_line?; ",
    "change_context: (\"@@\" | \"@@ \" text) LF; ",
    "change_line: (\"+\" | \"-\" | \" \") text LF; ",
    "eof_line: \"*** End of File\" LF."
);

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchArgs {
    /// Codex-style apply_patch payload. The whole string must start with `*** Begin Patch`
    /// and end with `*** End Patch`. Only this V4A/Codex-style patch format is accepted.
    pub patch: String,
}

#[derive(Debug, Clone)]
enum PatchOperation {
    AddFile {
        path: String,
        contents: String,
    },
    DeleteFile {
        path: String,
    },
    UpdateFile {
        path: String,
        move_to: Option<String>,
        chunks: Vec<UpdateChunk>,
    },
}

#[derive(Debug, Clone)]
struct UpdateChunk {
    change_context: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    is_end_of_file: bool,
}

#[derive(Debug)]
struct ParsedPatch {
    operations: Vec<PatchOperation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ChangeKind {
    Add,
    Modify,
    Delete,
}

#[derive(Debug, Clone)]
struct PathChange {
    display_path: String,
    kind: ChangeKind,
}

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
    let input: ApplyPatchArgs =
        serde_json::from_value(args).context("failed to parse apply_patch args")?;
    let parsed = parse_patch(&input.patch)?;
    apply_parsed_patch(parsed, session)
}

fn apply_parsed_patch(
    parsed: ParsedPatch,
    session: Option<(&SessionManager, &str)>,
) -> Result<String> {
    let workspace = std::env::current_dir()?;
    let sandbox = resolve_sandbox()?;
    let mut states: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut changes = Vec::<PathChange>::new();

    for operation in parsed.operations {
        match operation {
            PatchOperation::AddFile { path, contents } => {
                let resolved = resolve_patch_path(&path)?;
                ensure_path_allowed(
                    &sandbox,
                    AccessKind::Write,
                    &resolved,
                    &workspace,
                    "apply_patch",
                )?;
                states.insert(resolved, Some(contents));
                changes.push(PathChange {
                    display_path: path,
                    kind: ChangeKind::Add,
                });
            }
            PatchOperation::DeleteFile { path } => {
                let resolved = resolve_patch_path(&path)?;
                ensure_path_allowed(
                    &sandbox,
                    AccessKind::Write,
                    &resolved,
                    &workspace,
                    "apply_patch",
                )?;
                let _ = current_text(&mut states, &resolved)?;
                states.insert(resolved, None);
                changes.push(PathChange {
                    display_path: path,
                    kind: ChangeKind::Delete,
                });
            }
            PatchOperation::UpdateFile {
                path,
                move_to,
                chunks,
            } => {
                let resolved = resolve_patch_path(&path)?;
                ensure_path_allowed(
                    &sandbox,
                    AccessKind::Write,
                    &resolved,
                    &workspace,
                    "apply_patch",
                )?;
                let original = current_text(&mut states, &resolved)?;
                let new_contents = derive_new_contents(&original, &path, &chunks)?;
                if let Some(move_to) = move_to {
                    let destination = resolve_patch_path(&move_to)?;
                    if destination == resolved {
                        bail!("apply_patch Move to path must differ from Update File path: {path}");
                    }
                    ensure_path_allowed(
                        &sandbox,
                        AccessKind::Write,
                        &destination,
                        &workspace,
                        "apply_patch",
                    )?;
                    states.insert(destination, Some(new_contents));
                    states.insert(resolved, None);
                    changes.push(PathChange {
                        display_path: format!("{path} -> {move_to}"),
                        kind: ChangeKind::Modify,
                    });
                } else {
                    states.insert(resolved, Some(new_contents));
                    changes.push(PathChange {
                        display_path: path,
                        kind: ChangeKind::Modify,
                    });
                }
            }
        }
    }

    write_final_states(&states, session)?;
    Ok(format_summary(&changes))
}

fn resolve_patch_path(path: &str) -> Result<PathBuf> {
    validate_relative_patch_path(path)?;
    resolve_writable_sandbox_path(path).map_err(|error| {
        anyhow::anyhow!(format_tool_path_scope_block(
            "apply_patch",
            "write",
            path,
            error
        ))
    })
}

fn validate_relative_patch_path(path: &str) -> Result<()> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        bail!("apply_patch file path must be non-empty");
    }
    let raw = Path::new(trimmed);
    if raw.is_absolute() {
        bail!("apply_patch file paths must be relative, never absolute: {trimmed}");
    }
    for component in raw.components() {
        match component {
            Component::CurDir | Component::ParentDir => {
                bail!("apply_patch file paths must not contain . or .. components: {trimmed}");
            }
            Component::Prefix(_) | Component::RootDir => {
                bail!("apply_patch file paths must be relative, never absolute: {trimmed}");
            }
            Component::Normal(_) => {}
        }
    }
    Ok(())
}

fn current_text(states: &mut HashMap<PathBuf, Option<String>>, path: &Path) -> Result<String> {
    if let Some(state) = states.get(path) {
        return state
            .clone()
            .with_context(|| format!("apply_patch expected existing file: {}", path.display()));
    }
    let metadata = fs::metadata(path)
        .with_context(|| format!("apply_patch expected existing file: {}", path.display()))?;
    if metadata.is_dir() {
        bail!(
            "apply_patch cannot operate on directory: {}",
            path.display()
        );
    }
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read file for patch: {}", path.display()))?;
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        bail!("apply_patch only supports text files: {}", path.display());
    }
    let text = String::from_utf8(bytes)
        .with_context(|| format!("apply_patch target is not valid UTF-8: {}", path.display()))?;
    states.insert(path.to_path_buf(), Some(text.clone()));
    Ok(text)
}

fn write_final_states(
    states: &HashMap<PathBuf, Option<String>>,
    session: Option<(&SessionManager, &str)>,
) -> Result<()> {
    let mut paths = states.keys().cloned().collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        let snapshot = if let Some((session_manager, session_id)) = session {
            Some(session_manager.capture_file_snapshot_before(session_id, "apply_patch", &path)?)
        } else {
            None
        };
        match states.get(&path).and_then(|state| state.as_ref()) {
            Some(contents) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("failed to create parent directory: {}", parent.display())
                    })?;
                }
                fs::write(&path, contents.as_bytes())
                    .with_context(|| format!("failed to write patched file: {}", path.display()))?;
            }
            None => {
                if path.exists() {
                    let metadata = fs::metadata(&path).with_context(|| {
                        format!("failed to stat delete target: {}", path.display())
                    })?;
                    if metadata.is_dir() {
                        bail!("apply_patch cannot delete directory: {}", path.display());
                    }
                    fs::remove_file(&path)
                        .with_context(|| format!("failed to delete file: {}", path.display()))?;
                }
            }
        }
        if let (Some((session_manager, session_id)), Some(snapshot)) = (session, snapshot) {
            session_manager.append_file_snapshot_after(session_id, snapshot)?;
        }
    }
    Ok(())
}

fn format_summary(changes: &[PathChange]) -> String {
    let mut seen = BTreeSet::<(ChangeKind, String)>::new();
    for change in changes {
        seen.insert((change.kind, change.display_path.clone()));
    }
    let mut lines = vec!["Success. Updated the following files:".to_string()];
    for (kind, path) in seen {
        let marker = match kind {
            ChangeKind::Add => "A",
            ChangeKind::Modify => "M",
            ChangeKind::Delete => "D",
        };
        lines.push(format!("{marker} {path}"));
    }
    lines.join("\n")
}

fn parse_patch(patch: &str) -> Result<ParsedPatch> {
    let normalized = patch.trim();
    if normalized.is_empty() {
        bail!("apply_patch.patch must be non-empty");
    }
    let lines = normalized
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect::<Vec<_>>();
    if lines.first().map(|line| line.trim()) != Some(BEGIN_PATCH) {
        bail!("The first line of apply_patch.patch must be `{BEGIN_PATCH}`");
    }
    if lines.last().map(|line| line.trim()) != Some(END_PATCH) {
        bail!("The last line of apply_patch.patch must be `{END_PATCH}`");
    }
    let mut operations = Vec::new();
    let mut index = 1;
    while index + 1 < lines.len() {
        if lines[index].trim().is_empty() {
            index += 1;
            continue;
        }
        let (operation, next_index) = parse_operation(&lines, index)?;
        operations.push(operation);
        index = next_index;
    }
    if operations.is_empty() {
        bail!("apply_patch.patch must contain at least one file operation");
    }
    Ok(ParsedPatch { operations })
}

fn parse_operation(lines: &[String], index: usize) -> Result<(PatchOperation, usize)> {
    let line_no = index + 1;
    let line = lines[index].trim();
    if line == END_PATCH {
        bail!("apply_patch.patch must contain a file operation before `{END_PATCH}`");
    }
    if let Some(path) = line.strip_prefix(ADD_FILE) {
        let path = path.trim().to_string();
        validate_relative_patch_path(&path)?;
        let mut contents = String::new();
        let mut index = index + 1;
        let mut added_lines = 0;
        while index + 1 < lines.len() {
            let raw = &lines[index];
            if is_operation_or_end_marker(raw.trim()) {
                break;
            }
            let Some(line_to_add) = raw.strip_prefix('+') else {
                bail!(
                    "Invalid add-file line {}: every line after `{ADD_FILE}{path}` must start with `+`",
                    index + 1
                );
            };
            contents.push_str(line_to_add);
            contents.push('\n');
            added_lines += 1;
            index += 1;
        }
        if added_lines == 0 {
            bail!("Add File operation for `{path}` must contain at least one `+` line");
        }
        return Ok((PatchOperation::AddFile { path, contents }, index));
    }
    if let Some(path) = line.strip_prefix(DELETE_FILE) {
        let path = path.trim().to_string();
        validate_relative_patch_path(&path)?;
        return Ok((PatchOperation::DeleteFile { path }, index + 1));
    }
    if let Some(path) = line.strip_prefix(UPDATE_FILE) {
        let path = path.trim().to_string();
        validate_relative_patch_path(&path)?;
        let mut index = index + 1;
        let move_to = if index + 1 < lines.len() {
            lines[index]
                .trim()
                .strip_prefix(MOVE_TO)
                .map(|value| value.trim().to_string())
        } else {
            None
        };
        if let Some(move_to) = &move_to {
            validate_relative_patch_path(move_to)?;
            index += 1;
        }
        let mut chunks = Vec::new();
        while index + 1 < lines.len() {
            if lines[index].trim().is_empty() {
                index += 1;
                continue;
            }
            if is_operation_or_end_marker(lines[index].trim()) {
                break;
            }
            let (chunk, next_index) = parse_update_chunk(lines, index, chunks.is_empty())?;
            if next_index == index {
                bail!(
                    "Invalid update chunk at line {}: parser made no progress",
                    index + 1
                );
            }
            chunks.push(chunk);
            index = next_index;
        }
        if chunks.is_empty() {
            bail!("Update File operation for `{path}` must contain at least one hunk");
        }
        return Ok((
            PatchOperation::UpdateFile {
                path,
                move_to,
                chunks,
            },
            index,
        ));
    }
    bail!(
        "Invalid patch hunk on line {line_no}: expected `{ADD_FILE}<path>`, `{DELETE_FILE}<path>`, or `{UPDATE_FILE}<path>`"
    )
}

fn parse_update_chunk(
    lines: &[String],
    index: usize,
    allow_missing_context: bool,
) -> Result<(UpdateChunk, usize)> {
    let (change_context, mut index) = match lines[index].as_str() {
        "@@" => (None, index + 1),
        line if line.starts_with("@@ ") => (Some(line[3..].to_string()), index + 1),
        line if allow_missing_context => {
            let _ = line;
            (None, index)
        }
        line => bail!(
            "Expected update hunk to start with `@@` context marker at line {}, got: `{line}`",
            index + 1
        ),
    };

    let mut chunk = UpdateChunk {
        change_context,
        old_lines: Vec::new(),
        new_lines: Vec::new(),
        is_end_of_file: false,
    };
    let mut parsed_lines = 0;
    while index + 1 < lines.len() {
        let line = &lines[index];
        if line == EOF_MARKER {
            if parsed_lines == 0 {
                bail!(
                    "Update hunk at line {} does not contain any change lines",
                    index + 1
                );
            }
            chunk.is_end_of_file = true;
            index += 1;
            break;
        }
        match line.chars().next() {
            None => {
                chunk.old_lines.push(String::new());
                chunk.new_lines.push(String::new());
            }
            Some(' ') => {
                chunk.old_lines.push(line[1..].to_string());
                chunk.new_lines.push(line[1..].to_string());
            }
            Some('+') => chunk.new_lines.push(line[1..].to_string()),
            Some('-') => chunk.old_lines.push(line[1..].to_string()),
            _ if parsed_lines > 0 => break,
            _ => bail!(
                "Unexpected line in update hunk at line {}: `{}`. Lines must start with space, `+`, or `-`.",
                index + 1,
                line
            ),
        }
        parsed_lines += 1;
        index += 1;
    }
    if parsed_lines == 0 {
        bail!("Update hunk does not contain any change lines");
    }
    Ok((chunk, index))
}

fn is_operation_or_end_marker(line: &str) -> bool {
    line == END_PATCH
        || line.starts_with(ADD_FILE)
        || line.starts_with(DELETE_FILE)
        || line.starts_with(UPDATE_FILE)
}

fn derive_new_contents(
    original: &str,
    display_path: &str,
    chunks: &[UpdateChunk],
) -> Result<String> {
    let mut original_lines = original
        .split('\n')
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if original_lines.last().is_some_and(String::is_empty) {
        original_lines.pop();
    }
    let replacements = compute_replacements(&original_lines, display_path, chunks)?;
    let mut new_lines = apply_replacements(original_lines, &replacements);
    if !new_lines.last().is_some_and(String::is_empty) {
        new_lines.push(String::new());
    }
    Ok(new_lines.join("\n"))
}

fn compute_replacements(
    original_lines: &[String],
    display_path: &str,
    chunks: &[UpdateChunk],
) -> Result<Vec<(usize, usize, Vec<String>)>> {
    let mut replacements = Vec::new();
    let mut line_index = 0;
    for chunk in chunks {
        if let Some(context_line) = &chunk.change_context {
            let Some(found) = seek_sequence(
                original_lines,
                std::slice::from_ref(context_line),
                line_index,
                false,
            ) else {
                bail!("Failed to find context `{context_line}` in {display_path}");
            };
            line_index = found + 1;
        }
        if chunk.old_lines.is_empty() {
            replacements.push((original_lines.len(), 0, chunk.new_lines.clone()));
            continue;
        }
        let mut pattern = chunk.old_lines.as_slice();
        let mut replacement = chunk.new_lines.as_slice();
        let mut found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern = &pattern[..pattern.len() - 1];
            if replacement.last().is_some_and(String::is_empty) {
                replacement = &replacement[..replacement.len() - 1];
            }
            found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        }
        let Some(start) = found else {
            bail!(
                "Failed to find expected lines in {display_path}:\n{}",
                chunk.old_lines.join("\n")
            );
        };
        replacements.push((start, pattern.len(), replacement.to_vec()));
        line_index = start + pattern.len();
    }
    replacements.sort_by_key(|(start, _, _)| *start);
    for pair in replacements.windows(2) {
        let (prev_start, prev_len, _) = &pair[0];
        let (next_start, _, _) = &pair[1];
        if prev_start.saturating_add(*prev_len) > *next_start {
            bail!("apply_patch update hunks overlap in {display_path}");
        }
    }
    Ok(replacements)
}

fn apply_replacements(
    mut lines: Vec<String>,
    replacements: &[(usize, usize, Vec<String>)],
) -> Vec<String> {
    for (start, old_len, new_segment) in replacements.iter().rev() {
        for _ in 0..*old_len {
            if *start < lines.len() {
                lines.remove(*start);
            }
        }
        for (offset, line) in new_segment.iter().enumerate() {
            lines.insert(*start + offset, line.clone());
        }
    }
    lines
}

fn seek_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let search_start = if eof && lines.len() >= pattern.len() {
        lines.len() - pattern.len()
    } else {
        start
    };
    for mode in [MatchMode::Exact, MatchMode::TrimEnd, MatchMode::Trim] {
        for index in search_start..=lines.len().saturating_sub(pattern.len()) {
            if sequence_matches(lines, pattern, index, mode) {
                return Some(index);
            }
        }
    }
    None
}

#[derive(Debug, Clone, Copy)]
enum MatchMode {
    Exact,
    TrimEnd,
    Trim,
}

fn sequence_matches(lines: &[String], pattern: &[String], index: usize, mode: MatchMode) -> bool {
    pattern.iter().enumerate().all(|(offset, expected)| {
        let actual = &lines[index + offset];
        match mode {
            MatchMode::Exact => actual == expected,
            MatchMode::TrimEnd => actual.trim_end() == expected.trim_end(),
            MatchMode::Trim => actual.trim() == expected.trim(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{Builder, TempDir};

    struct Fixture {
        _dir: TempDir,
        root: PathBuf,
        prefix: String,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let workspace = std::env::current_dir()?;
            let dir = Builder::new()
                .prefix(".apply-patch-test-")
                .tempdir_in(&workspace)?;
            let root = dir.path().to_path_buf();
            let prefix = root
                .strip_prefix(&workspace)
                .context("test tempdir should live under workspace")?
                .to_string_lossy()
                .to_string();
            Ok(Self {
                _dir: dir,
                root,
                prefix,
            })
        }

        fn path(&self, path: &str) -> String {
            format!("{}/{}", self.prefix, path)
        }

        fn write(&self, path: &str, content: &str) -> Result<()> {
            let path = self.root.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, content)?;
            Ok(())
        }

        fn read(&self, path: &str) -> Result<String> {
            Ok(fs::read_to_string(self.root.join(path))?)
        }

        fn exists(&self, path: &str) -> bool {
            self.root.join(path).exists()
        }
    }

    #[test]
    fn applies_add_update_delete_and_move() -> Result<()> {
        let fixture = Fixture::new()?;
        fixture.write("src/app.rs", "fn main() {\n    println!(\"hi\");\n}\n")?;
        fixture.write("old/name.txt", "old content\n")?;
        fixture.write("delete.txt", "remove me\n")?;
        let src_new = fixture.path("src/new.rs");
        let src_app = fixture.path("src/app.rs");
        let old_name = fixture.path("old/name.txt");
        let renamed_name = fixture.path("renamed/name.txt");
        let delete_path = fixture.path("delete.txt");

        let patch = r#"*** Begin Patch
*** Add File: __SRC_NEW__
+pub fn answer() -> i32 {
+    42
+}
*** Update File: __SRC_APP__
@@ fn main() {
-    println!("hi");
+    println!("hello");
*** Update File: __OLD_NAME__
*** Move to: __RENAMED_NAME__
@@
-old content
+new content
*** Delete File: __DELETE_PATH__
*** End Patch"#
            .replace("__SRC_NEW__", &src_new)
            .replace("__SRC_APP__", &src_app)
            .replace("__OLD_NAME__", &old_name)
            .replace("__RENAMED_NAME__", &renamed_name)
            .replace("__DELETE_PATH__", &delete_path);
        let output = execute(serde_json::json!({
            "patch": patch
        }))?;

        assert!(output.contains(&format!("A {src_new}")));
        assert!(output.contains(&format!("M {old_name} -> {renamed_name}")));
        assert!(output.contains(&format!("D {delete_path}")));
        assert_eq!(
            fixture.read("src/new.rs")?,
            "pub fn answer() -> i32 {\n    42\n}\n"
        );
        assert_eq!(
            fixture.read("src/app.rs")?,
            "fn main() {\n    println!(\"hello\");\n}\n"
        );
        assert_eq!(fixture.read("renamed/name.txt")?, "new content\n");
        assert!(!fixture.exists("old/name.txt"));
        assert!(!fixture.exists("delete.txt"));
        Ok(())
    }

    #[test]
    fn rejects_non_v4a_replace_shape() -> Result<()> {
        let err = execute(serde_json::json!({
            "path": "src/lib.rs",
            "old_string": "a",
            "new_string": "b"
        }))
        .expect_err("apply_patch must reject replace-style args");
        assert!(format!("{err:#}").contains("failed to parse apply_patch args"));
        Ok(())
    }

    #[test]
    fn rejects_absolute_and_parent_paths() -> Result<()> {
        let absolute = execute(serde_json::json!({
            "patch": "*** Begin Patch\n*** Add File: /tmp/x\n+bad\n*** End Patch"
        }))
        .expect_err("absolute path must be rejected");
        assert!(absolute.to_string().contains("relative"));

        let parent = execute(serde_json::json!({
            "patch": "*** Begin Patch\n*** Add File: ../x\n+bad\n*** End Patch"
        }))
        .expect_err("parent path must be rejected");
        assert!(parent.to_string().contains(".."));
        Ok(())
    }

    #[test]
    fn pure_addition_update_appends_to_file() -> Result<()> {
        let fixture = Fixture::new()?;
        fixture.write("input.txt", "line1\nline2\n")?;
        let input_path = fixture.path("input.txt");
        execute(serde_json::json!({
            "patch": format!("*** Begin Patch\n*** Update File: {input_path}\n@@\n+added line 1\n+added line 2\n*** End Patch")
        }))?;
        assert_eq!(
            fixture.read("input.txt")?,
            "line1\nline2\nadded line 1\nadded line 2\n"
        );
        Ok(())
    }

    #[test]
    fn update_fails_when_expected_lines_do_not_match() -> Result<()> {
        let fixture = Fixture::new()?;
        fixture.write("input.txt", "actual\n")?;
        let input_path = fixture.path("input.txt");
        let err = execute(serde_json::json!({
            "patch": format!("*** Begin Patch\n*** Update File: {input_path}\n@@\n-missing\n+new\n*** End Patch")
        }))
        .expect_err("unmatched hunk must fail");
        assert!(err.to_string().contains("Failed to find expected lines"));
        assert_eq!(fixture.read("input.txt")?, "actual\n");
        Ok(())
    }
}
