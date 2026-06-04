use crate::profiles;
use crate::session::{ContentItem, SessionMessage};
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const AGENTS_FILENAME: &str = "AGENTS.md";
const MAX_UPWARD_LEVELS: usize = 5;
const DYNAMIC_CONTEXT_BLOCK_TEMPLATE: &str = include_str!("prompts/dynamic-context-block.md");
const MAIN_USER_MESSAGE_WITH_CONTEXT_TEMPLATE: &str =
    include_str!("prompts/main-user-message-with-context.md");
const TOOL_RESULT_WITH_CONTEXT_TEMPLATE: &str = include_str!("prompts/tool-result-with-context.md");

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DynamicContextBlock {
    pub id: String,
    pub label: String,
    pub content: String,
}

pub fn compose_main_user_message(
    user_text: &str,
    visible_messages: &[SessionMessage],
) -> Result<String> {
    compose_main_user_message_with_capabilities(user_text, visible_messages, None)
}

pub fn compose_main_user_message_with_capabilities(
    user_text: &str,
    visible_messages: &[SessionMessage],
    capabilities: Option<String>,
) -> Result<String> {
    compose_main_user_message_with_context(user_text, visible_messages, capabilities, Vec::new())
}

pub fn compose_main_user_message_with_context(
    user_text: &str,
    visible_messages: &[SessionMessage],
    capabilities: Option<String>,
    mut extra_blocks: Vec<DynamicContextBlock>,
) -> Result<String> {
    let mut blocks = Vec::new();
    if let Some(content) = capabilities.map(|value| value.trim().to_string()) {
        if !content.is_empty() {
            blocks.push(DynamicContextBlock {
                id: "duckagent://capabilities".to_string(),
                label: "AVAILABLE CAPABILITIES".to_string(),
                content,
            });
        }
    }
    blocks.append(&mut extra_blocks);
    if let Some(block) = read_global_agents()? {
        blocks.push(block);
    }
    if let Some(block) = read_project_agents()? {
        blocks.push(block);
    }
    compose_user_message_with_blocks(user_text, &blocks, visible_messages)
}

pub fn compose_tool_result(
    tool_output: &str,
    candidate_dirs: &[PathBuf],
    visible_messages: &[SessionMessage],
    current_turn_seen: &mut HashSet<(String, String)>,
) -> Result<String> {
    let mut blocks = Vec::new();
    for dir in candidate_dirs {
        for block in find_project_agents_upward(dir)? {
            let key = (block.id.clone(), block.content.clone());
            if !current_turn_seen.insert(key) {
                continue;
            }
            blocks.push(block);
        }
    }
    let new_blocks = filter_changed_blocks(&blocks, visible_messages);
    if new_blocks.is_empty() {
        return Ok(tool_output.to_string());
    }
    Ok(render_prompt_template(
        TOOL_RESULT_WITH_CONTEXT_TEMPLATE,
        &[
            (
                "dynamic_context",
                format_dynamic_context_blocks(&new_blocks),
            ),
            ("tool_output", tool_output.to_string()),
        ],
    ))
}

pub fn path_candidate_dirs_for_runtime_tool(
    capability: &str,
    args: &serde_json::Value,
) -> Vec<PathBuf> {
    let path = match capability.trim() {
        "read_file" | "search_files" | "search_content" | "write_file" | "edit" => {
            args.get("path").and_then(serde_json::Value::as_str)
        }
        "apply_patch" => return apply_patch_candidate_dirs(args),
        "process_start" => args.get("cwd").and_then(serde_json::Value::as_str),
        _ => None,
    };
    let Some(path) = path.map(str::trim).filter(|value| !value.is_empty()) else {
        return Vec::new();
    };
    let raw = PathBuf::from(path);
    let candidate = if raw.is_absolute() {
        raw
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(raw),
            Err(_) => return Vec::new(),
        }
    };
    if candidate.is_dir() {
        vec![candidate]
    } else {
        candidate
            .parent()
            .map(Path::to_path_buf)
            .into_iter()
            .collect()
    }
}

fn apply_patch_candidate_dirs(args: &serde_json::Value) -> Vec<PathBuf> {
    let Some(patch) = args.get("patch").and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    let mut dirs = BTreeSet::<PathBuf>::new();
    for line in patch.lines() {
        let trimmed = line.trim();
        for prefix in [
            "*** Add File: ",
            "*** Delete File: ",
            "*** Update File: ",
            "*** Move to: ",
        ] {
            if let Some(path) = trimmed.strip_prefix(prefix) {
                insert_candidate_parent(&cwd, path.trim(), &mut dirs);
            }
        }
    }
    dirs.into_iter().collect()
}

fn insert_candidate_parent(cwd: &Path, path: &str, dirs: &mut BTreeSet<PathBuf>) {
    if path.is_empty() {
        return;
    }
    let raw = PathBuf::from(path);
    let candidate = if raw.is_absolute() {
        raw
    } else {
        cwd.join(raw)
    };
    if candidate.is_dir() {
        dirs.insert(candidate);
    } else if let Some(parent) = candidate.parent() {
        dirs.insert(parent.to_path_buf());
    }
}

fn compose_user_message_with_blocks(
    user_text: &str,
    blocks: &[DynamicContextBlock],
    visible_messages: &[SessionMessage],
) -> Result<String> {
    let new_blocks = filter_changed_blocks(blocks, visible_messages);
    if new_blocks.is_empty() {
        return Ok(user_text.to_string());
    }
    Ok(render_prompt_template(
        MAIN_USER_MESSAGE_WITH_CONTEXT_TEMPLATE,
        &[
            (
                "dynamic_context",
                format_dynamic_context_blocks(&new_blocks),
            ),
            ("user_message", user_text.to_string()),
        ],
    ))
}

fn read_global_agents() -> Result<Option<DynamicContextBlock>> {
    let path = profiles::active_profile_path(AGENTS_FILENAME)?;
    read_instruction_file(
        &path,
        "PROFILE INSTRUCTIONS".to_string(),
        path.display().to_string(),
    )
}

fn read_project_agents() -> Result<Option<DynamicContextBlock>> {
    let path = std::env::current_dir()
        .context("failed to resolve current directory")?
        .join(AGENTS_FILENAME);
    let id = path.display().to_string();
    read_instruction_file(&path, "PROJECT INSTRUCTIONS".to_string(), id)
}

fn find_project_agents_upward(start_dir: &Path) -> Result<Vec<DynamicContextBlock>> {
    let mut blocks = Vec::new();
    let mut current = start_dir
        .canonicalize()
        .unwrap_or_else(|_| start_dir.to_path_buf());
    for _ in 0..MAX_UPWARD_LEVELS {
        let path = current.join(AGENTS_FILENAME);
        if let Some(block) = read_instruction_file(
            &path,
            "PROJECT INSTRUCTIONS".to_string(),
            path.display().to_string(),
        )? {
            blocks.push(block);
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }
    blocks.reverse();
    Ok(blocks)
}

fn read_instruction_file(
    path: &Path,
    label: String,
    id: String,
) -> Result<Option<DynamicContextBlock>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read instruction file: {}", path.display()))?;
    let content = content.trim().to_string();
    if content.is_empty() {
        return Ok(None);
    }
    Ok(Some(DynamicContextBlock { id, label, content }))
}

fn filter_changed_blocks(
    blocks: &[DynamicContextBlock],
    visible_messages: &[SessionMessage],
) -> Vec<DynamicContextBlock> {
    blocks
        .iter()
        .filter(
            |block| match latest_content_for_id(visible_messages, &block.id) {
                Some(content) => content.trim() != block.content.trim(),
                None => true,
            },
        )
        .cloned()
        .collect()
}

fn latest_content_for_id(messages: &[SessionMessage], id: &str) -> Option<String> {
    messages
        .iter()
        .rev()
        .flat_map(message_texts)
        .find_map(|text| {
            extract_blocks(&text).into_iter().rev().find_map(|block| {
                if block.id == id {
                    Some(block.content)
                } else {
                    None
                }
            })
        })
}

fn message_texts(message: &SessionMessage) -> Vec<String> {
    match message {
        SessionMessage::Message { content, .. } => content
            .iter()
            .map(|item| match item {
                ContentItem::InputText { text } | ContentItem::OutputText { text } => text.clone(),
            })
            .collect(),
        SessionMessage::ToolResult { output, .. } => vec![output.clone()],
        _ => Vec::new(),
    }
}

fn format_dynamic_context_blocks(blocks: &[DynamicContextBlock]) -> String {
    blocks
        .iter()
        .map(|block| {
            render_prompt_template(
                DYNAMIC_CONTEXT_BLOCK_TEMPLATE,
                &[
                    ("label", block.label.trim().to_string()),
                    ("id", block.id.trim().to_string()),
                    ("content", block.content.clone()),
                ],
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_prompt_template(template: &str, values: &[(&str, String)]) -> String {
    let mut rendered = template.to_string();
    for (key, value) in values {
        rendered = rendered.replace(&format!("{{{{{key}}}}}"), value);
    }
    rendered.trim_end().to_string()
}

fn extract_blocks(text: &str) -> Vec<DynamicContextBlock> {
    let mut blocks = Vec::new();

    let mut rest = text;
    while let Some(start) = rest.find('[') {
        let after_open = &rest[start + 1..];
        let Some(close) = after_open.find(']') else {
            break;
        };
        let label = after_open[..close].trim();
        let after_start = &after_open[close + 1..];
        if !is_context_label(label) {
            rest = after_start;
            continue;
        }

        let end_marker = format!("[/{label}]");
        let Some(end) = after_start.find(&end_marker) else {
            rest = after_start;
            continue;
        };
        let body = &after_start[..end];
        if let Some((id, content)) = parse_block_body(body) {
            blocks.push(DynamicContextBlock {
                id,
                label: label.to_string(),
                content,
            });
        }
        rest = &after_start[end + end_marker.len()..];
    }
    blocks
}

fn parse_block_body(body: &str) -> Option<(String, String)> {
    let body = body.trim_matches(['\n', '\r']);
    let mut lines = body.lines();
    let path_line = lines.next()?.trim();
    let id = path_line
        .strip_prefix("id: ")
        .or_else(|| path_line.strip_prefix("path: "))?
        .trim()
        .to_string();
    let mut saw_separator = false;
    let mut content_lines = Vec::new();
    for line in lines {
        if saw_separator {
            content_lines.push(line);
        } else if line.trim().is_empty() {
            saw_separator = true;
        }
    }
    if !saw_separator {
        return None;
    }
    let content = content_lines.join("\n").trim().to_string();
    if id.is_empty() || content.is_empty() {
        return None;
    }
    Some((id, content))
}

fn is_context_label(label: &str) -> bool {
    !label.is_empty()
        && !label.starts_with('/')
        && label.chars().all(|ch| {
            ch.is_ascii_uppercase() || ch.is_ascii_digit() || matches!(ch, ' ' | '_' | '-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionManager, SessionRole};
    use tempfile::tempdir;

    #[test]
    fn unchanged_dynamic_context_block_is_not_repeated() -> Result<()> {
        let manager = SessionManager::new(tempdir()?.keep())?;
        let session_id = manager.create_session(Some("demo"), "system")?;
        let block = DynamicContextBlock {
            id: "/tmp/project/AGENTS.md".to_string(),
            label: "PROJECT INSTRUCTIONS".to_string(),
            content: "be concise".to_string(),
        };
        let first = compose_user_message_with_blocks(
            "hello",
            &[block.clone()],
            &manager.get_all_messages(&session_id)?,
        )?;
        manager.append_text_message(&session_id, SessionRole::User, first)?;
        let second = compose_user_message_with_blocks(
            "again",
            &[block],
            &manager.get_all_messages(&session_id)?,
        )?;
        assert_eq!(second, "again");
        Ok(())
    }

    #[test]
    fn changed_dynamic_context_block_is_repeated() -> Result<()> {
        let manager = SessionManager::new(tempdir()?.keep())?;
        let session_id = manager.create_session(Some("demo"), "system")?;
        let id = "/tmp/project/AGENTS.md".to_string();
        let first_block = DynamicContextBlock {
            id: id.clone(),
            label: "PROJECT INSTRUCTIONS".to_string(),
            content: "old".to_string(),
        };
        let first = compose_user_message_with_blocks(
            "hello",
            &[first_block],
            &manager.get_all_messages(&session_id)?,
        )?;
        manager.append_text_message(&session_id, SessionRole::User, first)?;
        let changed = DynamicContextBlock {
            id,
            label: "PROJECT INSTRUCTIONS".to_string(),
            content: "new".to_string(),
        };
        let second = compose_user_message_with_blocks(
            "again",
            &[changed],
            &manager.get_all_messages(&session_id)?,
        )?;
        assert!(second.contains("[PROJECT INSTRUCTIONS]"));
        assert!(second.contains("new"));
        Ok(())
    }

    #[test]
    fn legacy_path_header_is_read_as_dynamic_context_id() -> Result<()> {
        let manager = SessionManager::new(tempdir()?.keep())?;
        let session_id = manager.create_session(Some("demo"), "system")?;
        manager.append_text_message(
            &session_id,
            SessionRole::User,
            "[PROJECT INSTRUCTIONS]\npath: /tmp/project/AGENTS.md\n\nbe concise\n[/PROJECT INSTRUCTIONS]",
        )?;
        let block = DynamicContextBlock {
            id: "/tmp/project/AGENTS.md".to_string(),
            label: "PROJECT INSTRUCTIONS".to_string(),
            content: "be concise".to_string(),
        };
        let second = compose_user_message_with_blocks(
            "again",
            &[block],
            &manager.get_all_messages(&session_id)?,
        )?;
        assert_eq!(second, "again");
        Ok(())
    }

    #[test]
    fn crlf_dynamic_context_block_is_not_repeated() -> Result<()> {
        let manager = SessionManager::new(tempdir()?.keep())?;
        let session_id = manager.create_session(Some("demo"), "system")?;
        manager.append_text_message(
            &session_id,
            SessionRole::User,
            "[PROJECT INSTRUCTIONS]\r\nid: /tmp/project/AGENTS.md\r\n\r\nbe concise\r\n[/PROJECT INSTRUCTIONS]",
        )?;
        let block = DynamicContextBlock {
            id: "/tmp/project/AGENTS.md".to_string(),
            label: "PROJECT INSTRUCTIONS".to_string(),
            content: "be concise".to_string(),
        };
        let second = compose_user_message_with_blocks(
            "again",
            &[block],
            &manager.get_all_messages(&session_id)?,
        )?;
        assert_eq!(second, "again");
        Ok(())
    }

    #[test]
    fn tool_result_injects_nearest_agents_once_per_turn() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path().join("project");
        let nested = root.join("a").join("b").join("c");
        fs::create_dir_all(&nested)?;
        fs::write(root.join(AGENTS_FILENAME), "root rule")?;
        fs::write(nested.join(AGENTS_FILENAME), "nested rule")?;
        let manager = SessionManager::new(tempdir()?.keep())?;
        let session_id = manager.create_session(Some("demo"), "system")?;
        let mut seen = HashSet::new();

        let first = compose_tool_result(
            "tool output",
            std::slice::from_ref(&nested),
            &manager.get_all_messages(&session_id)?,
            &mut seen,
        )?;
        let second = compose_tool_result(
            "tool output",
            std::slice::from_ref(&nested),
            &manager.get_all_messages(&session_id)?,
            &mut seen,
        )?;

        assert!(first.contains("root rule"));
        assert!(first.contains("nested rule"));
        assert_eq!(second, "tool output");
        Ok(())
    }

    #[test]
    fn apply_patch_runtime_tool_paths_extract_candidate_dirs() -> Result<()> {
        let dirs = path_candidate_dirs_for_runtime_tool(
            "apply_patch",
            &serde_json::json!({
                "patch": r#"*** Begin Patch
*** Add File: src/new.rs
+content
*** Update File: src/app.rs
@@
-old
+new
*** Update File: old/name.txt
*** Move to: renamed/name.txt
@@
-old
+new
*** Delete File: obsolete.txt
*** End Patch"#
            }),
        );

        assert!(dirs.iter().any(|dir| dir.ends_with("src")));
        assert!(dirs.iter().any(|dir| dir.ends_with("old")));
        assert!(dirs.iter().any(|dir| dir.ends_with("renamed")));
        Ok(())
    }

    #[test]
    fn upward_search_stops_after_five_levels() -> Result<()> {
        let dir = tempdir()?;
        let mut current = dir.path().join("root");
        fs::create_dir_all(&current)?;
        fs::write(current.join(AGENTS_FILENAME), "too far")?;
        for part in ["a", "b", "c", "d", "e", "f"] {
            current = current.join(part);
        }
        fs::create_dir_all(&current)?;
        let blocks = find_project_agents_upward(&current)?;
        assert!(blocks.is_empty());
        Ok(())
    }
}
