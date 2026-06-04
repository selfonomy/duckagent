use super::{format_tool_path_scope_block, resolve_writable_sandbox_path};
use crate::sandbox::config::resolve_sandbox;
use crate::sandbox::permissions::{AccessKind, ensure_path_allowed};
use crate::session::SessionManager;
use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    pub path: String,
    pub content: String,
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
    let input: WriteFileArgs =
        serde_json::from_value(args).context("failed to parse write_file args")?;
    let path = resolve_writable_sandbox_path(&input.path).map_err(|error| {
        anyhow::anyhow!(format_tool_path_scope_block(
            "write_file",
            "write",
            &input.path,
            error
        ))
    })?;
    let workspace = std::env::current_dir()?;
    let sandbox = resolve_sandbox()?;
    ensure_path_allowed(&sandbox, AccessKind::Write, &path, &workspace, "write_file")?;
    let snapshot = if let Some((session_manager, session_id)) = session {
        Some(session_manager.capture_file_snapshot_before(session_id, "write_file", &path)?)
    } else {
        None
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent directory: {}", parent.display()))?;
    }
    fs::write(&path, input.content.as_bytes())
        .with_context(|| format!("failed to write file: {}", path.display()))?;
    if let (Some((session_manager, session_id)), Some(snapshot)) = (session, snapshot) {
        session_manager.append_file_snapshot_after(session_id, snapshot)?;
    }
    Ok(format!(
        "wrote {} bytes to {}",
        input.content.len(),
        path.display()
    ))
}
