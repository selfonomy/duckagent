use crate::approval::{ApprovalDecision, ApprovalProvider, ApprovalResponse, RuleHit};
use crate::sandbox::config::{
    FileAccess, append_filesystem_mount_to_current_preset, grant_sandbox_access_once,
    grant_sandbox_access_session, path_is_core_protected,
};
use crate::sandbox::matcher::normalize_path_text;
use crate::sandbox::policy_block::{PolicyBlock, RequestedAccess, format_policy_block};
use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const ACCESS_GRANTED_TEMPLATE: &str =
    include_str!("../prompts/sandbox-filesystem-access-granted.md");
const ACCESS_DENIED_TEMPLATE: &str = include_str!("../prompts/sandbox-filesystem-access-denied.md");
const ACCESS_INVALID_TEMPLATE: &str =
    include_str!("../prompts/sandbox-filesystem-access-invalid.md");
const ACCESS_ERROR_TEMPLATE: &str = include_str!("../prompts/sandbox-filesystem-access-error.md");

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RequestFilesystemAccessArgs {
    /// Concrete file or directory path that needs sandbox access.
    pub path: String,
    /// Requested filesystem access. Use `ro` for read-only inspection and `rw` for writes.
    pub access: RequestedFilesystemAccess,
    /// Why this access is necessary for the user's request.
    pub reason: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RequestedFilesystemAccess {
    Ro,
    Rw,
}

impl RequestedFilesystemAccess {
    fn as_file_access(self) -> FileAccess {
        match self {
            Self::Ro => FileAccess::Ro,
            Self::Rw => FileAccess::Rw,
        }
    }
}

pub fn execute(args: Value, approval_provider: Arc<dyn ApprovalProvider>) -> Result<String> {
    let input: RequestFilesystemAccessArgs = match serde_json::from_value(args) {
        Ok(input) => input,
        Err(error) => {
            return Ok(render_invalid_request(
                "args",
                &format!("failed to parse request_filesystem_access args: {error}"),
                "(unavailable)",
                None,
            ));
        }
    };
    let access = input.access.as_file_access();
    if path_contains_glob(&input.path) {
        return Ok(render_invalid_request(
            "path",
            "path must be a concrete file or directory path, not a glob",
            &input.path,
            None,
        ));
    }
    if input.reason.trim().is_empty() {
        return Ok(render_invalid_request(
            "reason",
            "reason must explain why the access is necessary",
            &input.path,
            None,
        ));
    }

    let target = match resolve_requested_path(&input.path) {
        Ok(target) => target,
        Err(error) => return Ok(error.render()),
    };
    if path_is_core_protected(&target) {
        return Ok(render_ungrantable_request(&target, access));
    }
    let mount = match mount_for_request(&target, access) {
        Ok(mount) => mount,
        Err(error) => return Ok(error.render_with_path(&target)),
    };
    if path_is_core_protected(&mount) {
        return Ok(render_ungrantable_request(&target, access));
    }

    let command = format!(
        "sandbox-access {} {}",
        access_label(access),
        normalize_path_text(&mount)
    );
    let rule_hits = vec![RuleHit {
        rule_id: "sandbox.filesystem.request".to_string(),
        description: input.reason.trim().to_string(),
    }];
    let decision = approval_provider
        .request_approval(&command, &rule_hits, ApprovalDecision::options())
        .unwrap_or(ApprovalResponse {
            decision: ApprovalDecision::Forbidden,
        })
        .decision;

    match decision {
        ApprovalDecision::Forbidden => Ok(render_denied(&target)),
        ApprovalDecision::Once => {
            grant_sandbox_access_once(mount.clone(), access);
            Ok(granted_message("allow once", &target, &mount, access, None))
        }
        ApprovalDecision::Session => {
            grant_sandbox_access_session(mount.clone(), access);
            Ok(granted_message(
                "allow session",
                &target,
                &mount,
                access,
                None,
            ))
        }
        ApprovalDecision::Always => {
            let preset = match append_filesystem_mount_to_current_preset(
                &normalize_path_text(&mount),
                access,
            )
            .context("failed to persist sandbox filesystem access")
            {
                Ok(preset) => preset,
                Err(error) => {
                    return Ok(render_access_error(&target, access, &format!("{error:#}")));
                }
            };
            grant_sandbox_access_session(mount.clone(), access);
            Ok(granted_message(
                "allow always",
                &target,
                &mount,
                access,
                Some(&preset),
            ))
        }
    }
}

fn resolve_requested_path(path: &str) -> std::result::Result<PathBuf, InvalidAccessRequest> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(InvalidAccessRequest::new(
            "path",
            "path must be non-empty",
            "(empty)",
        ));
    }
    let raw = expand_home(trimmed)?;
    let absolute = if raw.is_absolute() {
        raw
    } else {
        let cwd = std::env::current_dir().map_err(|error| {
            InvalidAccessRequest::new(
                "path",
                format!("failed to resolve current workspace directory: {error}"),
                trimmed,
            )
        })?;
        cwd.join(raw)
    };
    Ok(lexical_normalize(&absolute))
}

fn expand_home(path: &str) -> std::result::Result<PathBuf, InvalidAccessRequest> {
    if path == "~" {
        return dirs::home_dir().ok_or_else(|| {
            InvalidAccessRequest::new(
                "path",
                "cannot expand `~` because home directory is unavailable",
                path,
            )
        });
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        let home = dirs::home_dir().ok_or_else(|| {
            InvalidAccessRequest::new(
                "path",
                "cannot expand `~` because home directory is unavailable",
                path,
            )
        })?;
        return Ok(home.join(rest));
    }
    Ok(PathBuf::from(path))
}

fn mount_for_request(
    target: &Path,
    access: FileAccess,
) -> std::result::Result<PathBuf, InvalidAccessRequest> {
    match access {
        FileAccess::Ro => {
            if !target.exists() {
                return Err(InvalidAccessRequest::new(
                    "path",
                    "read access requires an existing file or directory",
                    normalize_path_text(target),
                ));
            }
            Ok(target.to_path_buf())
        }
        FileAccess::Rw => {
            if target.is_dir() {
                return Ok(target.to_path_buf());
            }
            let parent = target.parent().ok_or_else(|| {
                InvalidAccessRequest::new(
                    "path",
                    "write access requires a parent directory",
                    normalize_path_text(target),
                )
            })?;
            if !parent.exists() {
                return Err(InvalidAccessRequest::new(
                    "path",
                    "write access requires the parent directory to exist",
                    normalize_path_text(target),
                )
                .with_parent(parent));
            }
            if !parent.is_dir() {
                return Err(InvalidAccessRequest::new(
                    "path",
                    "write access parent must be a directory",
                    normalize_path_text(target),
                )
                .with_parent(parent));
            }
            Ok(parent.to_path_buf())
        }
        FileAccess::None => Err(InvalidAccessRequest::new(
            "access",
            "access must be `ro` or `rw`",
            normalize_path_text(target),
        )),
    }
}

fn granted_message(
    user_choice: &str,
    target: &Path,
    mount: &Path,
    access: FileAccess,
    preset: Option<&str>,
) -> String {
    let granted_scope_line = if access == FileAccess::Rw && mount != target {
        format!("Granted write scope: {}\n", normalize_path_text(mount))
    } else {
        String::new()
    };
    let persisted_preset_line = preset
        .map(|preset| format!("Persisted sandbox preset: `{preset}`\n"))
        .unwrap_or_default();
    let retry_instruction = match access {
        FileAccess::Ro => "You can retry the previously blocked read now.",
        FileAccess::Rw => "You can retry the previously blocked write now.",
        FileAccess::None => "Do not retry the blocked operation unchanged.",
    };
    render_template(
        ACCESS_GRANTED_TEMPLATE,
        &[
            ("user_choice", user_choice.to_string()),
            ("requested_path", normalize_path_text(target)),
            ("effective_access", access_label(access).to_string()),
            ("granted_scope_line", granted_scope_line),
            ("persisted_preset_line", persisted_preset_line),
            ("retry_instruction", retry_instruction.to_string()),
        ],
    )
}

fn render_denied(target: &Path) -> String {
    render_template(
        ACCESS_DENIED_TEMPLATE,
        &[("requested_path", normalize_path_text(target))],
    )
}

fn render_invalid_request(field: &str, reason: &str, path: &str, parent: Option<&Path>) -> String {
    let parent_line = parent
        .map(|parent| format!("Parent: {}\n", normalize_path_text(parent)))
        .unwrap_or_default();
    render_template(
        ACCESS_INVALID_TEMPLATE,
        &[
            ("field", field.to_string()),
            ("reason", reason.to_string()),
            ("path", path.to_string()),
            ("parent_line", parent_line),
        ],
    )
}

fn render_ungrantable_request(target: &Path, access: FileAccess) -> String {
    format_policy_block(PolicyBlock {
        blocked_by: "sandbox_filesystem".to_string(),
        capability: "request_filesystem_access".to_string(),
        requested_access: requested_access_for_file_access(access),
        target: normalize_path_text(target),
        sandbox_preset: "current".to_string(),
        effective_access: "none".to_string(),
        detail: "The requested filesystem access cannot be granted by this capability under the active sandbox policy.".to_string(),
        resolution: "Choose a different target that is already allowed, or have the user change sandbox configuration outside the Agent. Do not retry this same access request unchanged.".to_string(),
    })
}

fn requested_access_for_file_access(access: FileAccess) -> RequestedAccess {
    match access {
        FileAccess::Rw => RequestedAccess::Write,
        FileAccess::Ro | FileAccess::None => RequestedAccess::Read,
    }
}

fn render_access_error(target: &Path, access: FileAccess, detail: &str) -> String {
    render_template(
        ACCESS_ERROR_TEMPLATE,
        &[
            ("requested_path", normalize_path_text(target)),
            ("requested_access", access_label(access).to_string()),
            ("detail", detail.to_string()),
        ],
    )
}

fn access_label(access: FileAccess) -> &'static str {
    match access {
        FileAccess::None => "none",
        FileAccess::Ro => "ro",
        FileAccess::Rw => "rw",
    }
}

fn path_contains_glob(path: &str) -> bool {
    path.chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | ']' | '{' | '}'))
}

#[derive(Debug)]
struct InvalidAccessRequest {
    field: &'static str,
    reason: String,
    path: String,
    parent: Option<PathBuf>,
}

impl InvalidAccessRequest {
    fn new(field: &'static str, reason: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            field,
            reason: reason.into(),
            path: path.into(),
            parent: None,
        }
    }

    fn with_parent(mut self, parent: &Path) -> Self {
        self.parent = Some(parent.to_path_buf());
        self
    }

    fn render(&self) -> String {
        render_invalid_request(self.field, &self.reason, &self.path, self.parent.as_deref())
    }

    fn render_with_path(mut self, target: &Path) -> String {
        self.path = normalize_path_text(target);
        self.render()
    }
}

fn render_template(template: &str, values: &[(&str, String)]) -> String {
    let mut rendered = template.to_string();
    for (key, value) in values {
        rendered = rendered.replace(&format!("{{{{{key}}}}}"), value);
    }
    rendered.trim_end().to_string()
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{
        ApprovalDecision, ApprovalProvider, ApprovalResponse, DenyApprovalProvider,
    };
    use std::sync::Mutex;

    struct StaticApprovalProvider {
        decision: ApprovalDecision,
    }

    impl ApprovalProvider for StaticApprovalProvider {
        fn request_approval(
            &self,
            _command: &str,
            _rule_hits: &[RuleHit],
            _options: [ApprovalDecision; 4],
        ) -> Option<ApprovalResponse> {
            Some(ApprovalResponse {
                decision: self.decision,
            })
        }
    }

    static REQUEST_TEST_LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();

    fn request_test_lock() -> std::sync::MutexGuard<'static, ()> {
        REQUEST_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn rejects_core_duckagent_path_without_prompt() -> Result<()> {
        let home = dirs::home_dir().context("home dir required for test")?;
        let out = execute(
            serde_json::json!({
                "path": home.join(".duckagent/config.json"),
                "access": "rw",
                "reason": "test"
            }),
            Arc::new(DenyApprovalProvider),
        )?;
        assert!(out.contains("Tool error: policy_blocked"));
        assert!(out.contains("blocked_by: sandbox_filesystem"));
        assert!(out.contains("capability: request_filesystem_access"));
        Ok(())
    }

    #[test]
    fn schema_requires_path_access_and_reason() -> Result<()> {
        let schema = serde_json::to_value(schemars::schema_for!(RequestFilesystemAccessArgs))?;
        let required = schema
            .get("required")
            .and_then(Value::as_array)
            .context("schema should contain required fields")?
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        assert!(required.contains(&"path"));
        assert!(required.contains(&"access"));
        assert!(required.contains(&"reason"));
        Ok(())
    }

    #[test]
    fn ignores_unknown_args_before_prompting() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        let out = execute(
            serde_json::json!({
                "path": file.path(),
                "access": "ro",
                "reason": "Need to inspect a user-requested file",
                "duration": "always"
            }),
            Arc::new(DenyApprovalProvider),
        )?;
        assert!(out.contains("Tool error: policy_blocked"));
        assert!(out.contains("Blocked by: user_decision"));
        assert!(!out.contains("Tool error: invalid_request"));
        Ok(())
    }

    #[test]
    fn rejects_glob_path_before_prompting() {
        let out = execute(
            serde_json::json!({
                "path": "**/.env",
                "access": "ro",
                "reason": "test"
            }),
            Arc::new(DenyApprovalProvider),
        )
        .unwrap();
        assert!(out.contains("Tool error: invalid_request"));
        assert!(out.contains("not a glob"));
    }

    #[test]
    fn rejects_none_access_before_prompting() {
        let out = execute(
            serde_json::json!({
                "path": "/tmp/example",
                "access": "none",
                "reason": "test"
            }),
            Arc::new(DenyApprovalProvider),
        )
        .unwrap();
        assert!(out.contains("Tool error: invalid_request"));
        assert!(out.contains("Capability: request_filesystem_access"));
        assert!(out.contains("unknown variant"));
    }

    #[test]
    fn schema_access_allows_only_ro_and_rw() -> Result<()> {
        let schema = serde_json::to_value(schemars::schema_for!(RequestFilesystemAccessArgs))?;
        let schema_text = schema.to_string();
        assert!(schema_text.contains("\"ro\""));
        assert!(schema_text.contains("\"rw\""));
        assert!(!schema_text.contains("\"none\""));
        Ok(())
    }

    #[test]
    fn rejects_empty_path_before_prompting() {
        let out = execute(
            serde_json::json!({
                "path": " ",
                "access": "ro",
                "reason": "test"
            }),
            Arc::new(DenyApprovalProvider),
        )
        .unwrap();
        assert!(out.contains("Tool error: invalid_request"));
        assert!(out.contains("path must be non-empty"));
    }

    #[test]
    fn rejects_empty_reason_before_prompting() {
        let out = execute(
            serde_json::json!({
                "path": "/tmp/example",
                "access": "ro",
                "reason": " "
            }),
            Arc::new(DenyApprovalProvider),
        )
        .unwrap();
        assert!(out.contains("Tool error: invalid_request"));
        assert!(out.contains("reason must explain why"));
    }

    #[test]
    fn tilde_path_expands_to_home_directory() -> Result<()> {
        let home = dirs::home_dir().context("home dir required for test")?;
        let resolved = resolve_requested_path("~").unwrap();
        assert_eq!(resolved, lexical_normalize(&home));

        let resolved_child = resolve_requested_path("~/duckagent-test").unwrap();
        assert_eq!(
            resolved_child,
            lexical_normalize(&home.join("duckagent-test"))
        );
        Ok(())
    }

    #[test]
    fn normalized_paths_use_forward_slashes_for_windows_style_input() {
        let text = normalize_path_text(Path::new("C:\\Users\\tester\\file.txt"));
        assert!(text.contains("C:/Users/tester/file.txt"));
    }

    #[test]
    fn read_access_requires_existing_target_before_prompting() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let missing = dir.path().join("missing.txt");
        let out = execute(
            serde_json::json!({
                "path": missing,
                "access": "ro",
                "reason": "Need to inspect the missing file"
            }),
            Arc::new(DenyApprovalProvider),
        )?;
        assert!(out.contains("Tool error: invalid_request"));
        assert!(out.contains("read access requires an existing file or directory"));
        Ok(())
    }

    #[test]
    fn write_access_requires_existing_parent_before_prompting() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let missing = dir.path().join("missing-parent").join("out.txt");
        let out = execute(
            serde_json::json!({
                "path": missing,
                "access": "rw",
                "reason": "Need to create the file"
            }),
            Arc::new(DenyApprovalProvider),
        )?;
        assert!(out.contains("Tool error: invalid_request"));
        assert!(out.contains("write access requires the parent directory to exist"));
        assert!(out.contains("Parent:"));
        Ok(())
    }

    #[test]
    fn forbidden_decision_returns_policy_blocked_output() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        let out = execute(
            serde_json::json!({
                "path": file.path(),
                "access": "ro",
                "reason": "Need to inspect a user-requested file"
            }),
            Arc::new(DenyApprovalProvider),
        )?;
        assert!(out.contains("Tool error: policy_blocked"));
        assert!(out.contains("Blocked by: user_decision"));
        assert!(out.contains("User choice: deny"));
        assert!(out.contains("Effective access: none"));
        Ok(())
    }

    #[test]
    fn once_decision_returns_granted_output_shape() -> Result<()> {
        let _guard = request_test_lock();
        let file = tempfile::NamedTempFile::new()?;
        crate::sandbox::config::consume_once_sandbox_access_grants();
        let out = execute(
            serde_json::json!({
                "path": file.path(),
                "access": "ro",
                "reason": "Need to inspect a user-requested file"
            }),
            Arc::new(StaticApprovalProvider {
                decision: ApprovalDecision::Once,
            }),
        )?;
        assert!(out.contains("Sandbox filesystem access granted."));
        assert!(out.contains("User choice: allow once"));
        assert!(out.contains("Effective access: ro"));
        assert!(out.contains("You can retry the previously blocked read now."));
        crate::sandbox::config::consume_once_sandbox_access_grants();
        Ok(())
    }

    #[test]
    fn session_granted_output_shape_is_stable() {
        let target = PathBuf::from("/tmp/example");
        let out = granted_message("allow session", &target, &target, FileAccess::Ro, None);
        assert!(out.contains("Sandbox filesystem access granted."));
        assert!(out.contains("User choice: allow session"));
        assert!(out.contains("Requested path: /tmp/example"));
        assert!(out.contains("Effective access: ro"));
    }

    #[test]
    fn always_granted_output_shape_includes_persisted_preset() {
        let target = PathBuf::from("/tmp/example");
        let out = granted_message(
            "allow always",
            &target,
            &target,
            FileAccess::Ro,
            Some("workspace-custom"),
        );
        assert!(out.contains("User choice: allow always"));
        assert!(out.contains("Effective access: ro"));
        assert!(out.contains("Persisted sandbox preset: `workspace-custom`"));
    }

    #[test]
    fn read_file_request_uses_exact_file_mount() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        let mount = mount_for_request(file.path(), FileAccess::Ro).unwrap();
        assert_eq!(mount, file.path());
        Ok(())
    }

    #[test]
    fn read_directory_request_uses_exact_directory_mount() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mount = mount_for_request(dir.path(), FileAccess::Ro).unwrap();
        assert_eq!(mount, dir.path());
        Ok(())
    }

    #[test]
    fn write_directory_request_uses_exact_directory_mount() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mount = mount_for_request(dir.path(), FileAccess::Rw).unwrap();
        assert_eq!(mount, dir.path());
        Ok(())
    }

    #[test]
    fn granted_message_for_rw_file_reports_write_scope() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("out.txt");
        let mount = mount_for_request(&target, FileAccess::Rw).unwrap();
        let out = granted_message("allow session", &target, &mount, FileAccess::Rw, None);
        assert!(out.contains("User choice: allow session"));
        assert!(out.contains("Effective access: rw"));
        assert!(out.contains(&format!("Requested path: {}", normalize_path_text(&target))));
        assert!(out.contains(&format!(
            "Granted write scope: {}",
            normalize_path_text(dir.path())
        )));
        Ok(())
    }

    #[test]
    fn rw_file_request_mounts_parent_directory() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("out.txt");
        let mount = mount_for_request(&target, FileAccess::Rw).unwrap();
        assert_eq!(mount, dir.path());
        Ok(())
    }
}
