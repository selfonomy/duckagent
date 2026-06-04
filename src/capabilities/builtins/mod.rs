pub mod apply_patch;
pub mod cron;
pub mod edit;
pub mod load_mcp;
pub mod main_agent;
pub mod memory;
pub mod process;
pub mod read_file;
pub mod sandbox_access;
pub mod search_common;
pub mod search_content;
pub mod search_files;
pub mod skills;
pub mod vision_analyze;
pub mod web_extract;
pub mod web_search;
pub mod write_file;

use crate::approval::ApprovalProvider;
use crate::client::ModelClient;
use crate::session::SessionManager;
use crate::tools::RuntimeToolInput;
use anyhow::{Context, Result, bail};
use schemars::schema_for;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct BuiltinToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

pub struct BuiltinExecutionContext<'a> {
    pub session_manager: &'a SessionManager,
    pub session_id: &'a str,
    pub approval_provider: Arc<dyn ApprovalProvider>,
    pub vision_client: &'a ModelClient,
}

pub fn builtin_tool_specs() -> Vec<BuiltinToolSpec> {
    let mut specs = vec![
        BuiltinToolSpec {
            name: "read_file",
            description: "Read a sandbox-allowed text file with line numbers and pagination. Non-text files return metadata and guidance.",
            input_schema: schema_value(schema_for!(read_file::ReadFileArgs)),
        },
        BuiltinToolSpec {
            name: "search_files",
            description: search_files::DESCRIPTION,
            input_schema: schema_value(schema_for!(search_files::SearchFilesArgs)),
        },
        BuiltinToolSpec {
            name: "search_content",
            description: search_content::DESCRIPTION,
            input_schema: schema_value(schema_for!(search_content::SearchContentArgs)),
        },
        BuiltinToolSpec {
            name: "write_file",
            description: "Write full file content to a sandbox-writable path, creating parent directories when needed.",
            input_schema: schema_value(schema_for!(write_file::WriteFileArgs)),
        },
        BuiltinToolSpec {
            name: "edit",
            description: edit::DESCRIPTION,
            input_schema: schema_value(schema_for!(edit::EditArgs)),
        },
        BuiltinToolSpec {
            name: "apply_patch",
            description: apply_patch::DESCRIPTION,
            input_schema: schema_value(schema_for!(apply_patch::ApplyPatchArgs)),
        },
        BuiltinToolSpec {
            name: "process_start",
            description: "Start a managed shell command. Non-PTY commands are observed for up to 5000ms and then return process_id, status, exit_code, output, cursor, and truncated without stopping the process.",
            input_schema: process_start_schema(),
        },
        BuiltinToolSpec {
            name: "process_read",
            description: "Read output for a managed process by process_id. Supports tail, since, full, and search modes.",
            input_schema: process_read_schema(),
        },
        BuiltinToolSpec {
            name: "process_list",
            description: "List managed processes for this Agent session.",
            input_schema: empty_object_schema(),
        },
        BuiltinToolSpec {
            name: "process_stop",
            description: "Request graceful termination of a managed process by process_id.",
            input_schema: process_id_schema(),
        },
        BuiltinToolSpec {
            name: "process_write",
            description: "Write bytes to a currently live PTY process by process_id.",
            input_schema: process_write_schema(),
        },
        BuiltinToolSpec {
            name: "process_watch",
            description: "Watch a managed process for exit and send a follow-up event message containing process_id, event, exit_code, output_tail, and on_event.",
            input_schema: process_watch_schema(),
        },
        BuiltinToolSpec {
            name: "vision_analyze",
            description: "Analyze a sandbox-readable local image file using the active model provider's vision support.",
            input_schema: schema_value(schema_for!(vision_analyze::VisionAnalyzeArgs)),
        },
        BuiltinToolSpec {
            name: "web_search",
            description: web_search::DESCRIPTION,
            input_schema: schema_value(schema_for!(web_search::WebSearchArgs)),
        },
        BuiltinToolSpec {
            name: "web_extract",
            description: web_extract::DESCRIPTION,
            input_schema: schema_value(schema_for!(web_extract::WebExtractArgs)),
        },
    ];
    specs.extend(skills::specs());
    specs.extend(cron::specs());
    specs.push(sandbox_access::spec());
    specs
}

pub fn execute_builtin(
    input: RuntimeToolInput,
    context: &BuiltinExecutionContext<'_>,
) -> Result<String> {
    match input.capability.trim() {
        "process_start" => process::start(
            context.session_manager,
            context.session_id,
            input.args,
            context.approval_provider.clone(),
        ),
        "process_read" => process::read(context.session_manager, context.session_id, input.args),
        "process_list" => process::list(context.session_manager, context.session_id, input.args),
        "process_stop" => process::stop(context.session_manager, context.session_id, input.args),
        "process_write" => process::write(context.session_manager, context.session_id, input.args),
        "process_watch" => process::watch(context.session_manager, context.session_id, input.args),
        "read_file" => read_file::execute(input.args),
        "search_files" => search_files::execute(input.args),
        "search_content" => search_content::execute(input.args),
        "write_file" => write_file::execute_with_session(
            input.args,
            context.session_manager,
            context.session_id,
        ),
        "edit" => {
            edit::execute_with_session(input.args, context.session_manager, context.session_id)
        }
        "apply_patch" => apply_patch::execute_with_session(
            input.args,
            context.session_manager,
            context.session_id,
        ),
        "vision_analyze" => execute_vision(input.args, context.vision_client),
        "web_search" => web_search::execute(input.args),
        "web_extract" => web_extract::execute(input.args, context.vision_client),
        "load_skill" => skills::execute_load_skill(input.args),
        "read_skill_file" => skills::execute_read_skill_file(input.args),
        "cron_create" | "cron_list" | "cron_get" | "cron_update" | "cron_delete" | "cron_pause"
        | "cron_resume" => cron::execute(input.capability.trim(), input.args, context.session_id),
        "request_filesystem_access" => {
            sandbox_access::execute(input.args, context.approval_provider.clone())
        }
        capability if capability.is_empty() => {
            Ok("Tool error: call_capability.capability must be non-empty.".to_string())
        }
        capability => Ok(format!(
            "Tool error: unknown builtin call_capability capability `{capability}`."
        )),
    }
}

pub fn unavailable_capability_result(
    agent_mode: &str,
    capability: &str,
    allowed: &[&str],
) -> String {
    json!({
        "status": "unavailable",
        "agent_mode": agent_mode,
        "capability": capability,
        "allowed_capabilities": allowed,
        "message": format!("Capability `{capability}` is not available in {agent_mode} mode.")
    })
    .to_string()
}

fn execute_vision(args: Value, client: &ModelClient) -> Result<String> {
    match vision_analyze::prepare_request(args) {
        Ok(request) => Ok(client
            .analyze_image_file(&request.path, &request.mime, &request.question)
            .unwrap_or_else(|error| {
                format!(
                    "Tool error: vision_analyze failed or is unsupported by the current AI provider.\npath: {}\nmime: {}\nsize_bytes: {}\nerror: {error:#}",
                    request.path.display(),
                    request.mime,
                    request.size_bytes
                )
            })),
        Err(error) => Ok(format!("Tool error: {error:#}")),
    }
}

#[allow(dead_code)]
pub(crate) fn resolve_existing_workspace_path(path: &str) -> Result<PathBuf> {
    let root = workspace_root()?;
    let candidate = workspace_candidate(&root, path)?;
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve path: {}", candidate.display()))?;
    ensure_inside_workspace(&root, &canonical)?;
    Ok(canonical)
}

pub(crate) fn resolve_existing_sandbox_path(path: &str) -> Result<PathBuf> {
    let root = workspace_root()?;
    let candidate = host_candidate(&root, path)?;
    candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve path: {}", candidate.display()))
}

pub(crate) fn resolve_writable_sandbox_path(path: &str) -> Result<PathBuf> {
    let root = workspace_root()?;
    let candidate = host_candidate(&root, path)?;
    if candidate.exists() {
        return candidate
            .canonicalize()
            .with_context(|| format!("failed to resolve path: {}", candidate.display()));
    }
    if let Some(parent) = candidate.parent() {
        let nearest = nearest_existing_ancestor(parent)?;
        nearest
            .canonicalize()
            .with_context(|| format!("failed to resolve path: {}", nearest.display()))?;
    }
    Ok(candidate)
}

pub(crate) fn format_tool_path_scope_block(
    capability: &str,
    requested_access: &str,
    target: &str,
    error: anyhow::Error,
) -> String {
    use crate::sandbox::policy_block::{PolicyBlock, RequestedAccess, format_policy_block};

    let requested_access = match requested_access {
        "read" => RequestedAccess::Read,
        "write" => RequestedAccess::Write,
        _ => RequestedAccess::Execute,
    };
    format_policy_block(PolicyBlock::tool_scope(
        capability,
        requested_access,
        target,
        format!(
            "`{capability}` did not reach sandbox filesystem evaluation because the target is outside this capability's path scope or uses an unsupported path form. Original error: {error:#}"
        ),
    ))
}

fn workspace_root() -> Result<PathBuf> {
    std::env::current_dir()
        .context("failed to get current workspace directory")?
        .canonicalize()
        .context("failed to resolve current workspace directory")
}

#[allow(dead_code)]
fn workspace_candidate(root: &Path, path: &str) -> Result<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        bail!("path must be non-empty");
    }
    let raw = PathBuf::from(trimmed);
    if raw
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        bail!("path must be a workspace path without . or .. components");
    }
    Ok(if raw.is_absolute() {
        raw
    } else {
        root.join(raw)
    })
}

fn host_candidate(root: &Path, path: &str) -> Result<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        bail!("path must be non-empty");
    }
    let raw = expand_home_path(trimmed)?;
    let candidate = if raw.is_absolute() {
        raw
    } else {
        root.join(raw)
    };
    Ok(lexical_normalize_path(&candidate))
}

fn expand_home_path(path: &str) -> Result<PathBuf> {
    if path == "~" {
        return dirs::home_dir().context("failed to expand `~`: home directory is unavailable");
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        let home =
            dirs::home_dir().context("failed to expand `~`: home directory is unavailable")?;
        return Ok(home.join(rest));
    }
    Ok(PathBuf::from(path))
}

fn lexical_normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut current = path;
    loop {
        if current.exists() {
            return Ok(current.to_path_buf());
        }
        current = current
            .parent()
            .with_context(|| format!("failed to find existing parent for {}", path.display()))?;
    }
}

#[allow(dead_code)]
fn ensure_inside_workspace(root: &Path, path: &Path) -> Result<()> {
    if !path.starts_with(root) {
        bail!("path is outside the workspace: {}", path.display());
    }
    Ok(())
}

pub(super) fn schema_value(schema: schemars::Schema) -> Value {
    serde_json::to_value(schema).unwrap_or_else(|_| json!({"type": "object"}))
}

fn empty_object_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

fn process_start_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": { "type": "string" },
            "cwd": { "type": "string" },
            "pty": { "type": "boolean", "default": false }
        },
        "required": ["command"],
        "additionalProperties": false
    })
}

fn process_read_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "process_id": { "type": "string" },
            "mode": {
                "type": "string",
                "enum": ["tail", "since", "full", "search"],
                "default": "tail"
            },
            "cursor": { "type": "integer" },
            "query": { "type": "string" },
            "limit": { "type": "integer" },
        },
        "required": ["process_id"],
        "additionalProperties": false
    })
}

fn process_id_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "process_id": { "type": "string" }
        },
        "required": ["process_id"],
        "additionalProperties": false
    })
}

fn process_write_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "process_id": { "type": "string" },
            "data": { "type": "string" }
        },
        "required": ["process_id", "data"],
        "additionalProperties": false
    })
}

fn process_watch_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "process_id": { "type": "string" },
            "event": {
                "type": "string",
                "enum": ["exit"]
            },
            "on_event": { "type": "string" }
        },
        "required": ["process_id", "event", "on_event"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_path_scope_block_uses_access_specific_policy_language() {
        let read = format_tool_path_scope_block(
            "read_file",
            "read",
            "/Users/example/.duckagent/config.json",
            anyhow::anyhow!("path is outside the workspace"),
        );
        assert!(read.contains("blocked_by: tool_path_scope"));
        assert!(read.contains("requested_access: read"));
        assert!(read.contains("retryable: false"));
        assert!(read.contains("request_filesystem_access"));

        let write = format_tool_path_scope_block(
            "write_file",
            "write",
            "../secret",
            anyhow::anyhow!("path must be a workspace path without . or .. components"),
        );
        assert!(write.contains("requested_access: write"));
        assert!(write.contains("concrete file or directory path"));
    }

    #[test]
    fn host_candidate_expands_tilde_before_joining_workspace() -> Result<()> {
        let home = dirs::home_dir().context("home dir required for test")?;
        let root = Path::new("/tmp/workspace");
        let candidate = host_candidate(root, "~/duckagent-test")?;
        assert_eq!(
            candidate,
            lexical_normalize_path(&home.join("duckagent-test"))
        );
        Ok(())
    }

    #[test]
    fn tool_path_scope_block_does_not_special_case_duckagent_text() {
        let text = format_tool_path_scope_block(
            "read_file",
            "read",
            "~/.duckagent/config.json",
            anyhow::anyhow!("failed to resolve path"),
        );

        assert!(text.contains("blocked_by: tool_path_scope"));
        assert!(text.contains("requested_access: read"));
        assert!(text.contains("Original error"));
    }
}
