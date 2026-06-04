use crate::sandbox::config::FileAccess;

const RUNTIME_POLICY_BLOCKED_TEMPLATE: &str = include_str!("../prompts/runtime-policy-blocked.md");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedAccess {
    Read,
    Write,
    Execute,
}

impl RequestedAccess {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Execute => "execute",
        }
    }

    fn equivalent_methods(self) -> &'static str {
        match self {
            Self::Read => {
                "read_file, process_start commands that read or inspect the target such as cat, less, head, tail, sed, awk, grep, rg, find, ls, stat"
            }
            Self::Write => {
                "write_file, apply_patch, process_start commands that create, modify, delete, rename, or chmod the target such as tee, redirection, cp, mv, rm, touch, mkdir, sed -i"
            }
            Self::Execute => {
                "process_start, stdio MCP server startup, or another command that performs the same operation"
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct PolicyBlock {
    pub blocked_by: String,
    pub capability: String,
    pub requested_access: RequestedAccess,
    pub target: String,
    pub sandbox_preset: String,
    pub effective_access: String,
    pub detail: String,
    pub resolution: String,
}

impl PolicyBlock {
    pub fn tool_scope(
        capability: impl Into<String>,
        requested_access: RequestedAccess,
        target: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        let capability = capability.into();
        let resolution = tool_scope_resolution(&capability, requested_access);
        Self {
            blocked_by: "tool_path_scope".to_string(),
            capability,
            requested_access,
            target: target.into(),
            sandbox_preset: "not_evaluated".to_string(),
            effective_access: "not_evaluated".to_string(),
            detail: detail.into(),
            resolution,
        }
    }

    pub fn sandbox_filesystem(
        sandbox_preset: impl Into<String>,
        capability: impl Into<String>,
        requested_access: RequestedAccess,
        target: impl Into<String>,
        effective_access: FileAccess,
    ) -> Self {
        let requested = requested_access.as_str();
        let effective = file_access_label(effective_access);
        Self {
            blocked_by: "sandbox_filesystem".to_string(),
            capability: capability.into(),
            requested_access,
            target: target.into(),
            sandbox_preset: sandbox_preset.into(),
            effective_access: effective.to_string(),
            detail: format!(
                "The active sandbox filesystem policy does not allow {requested} access to this target. Current effective filesystem access is `{effective}`."
            ),
            resolution: sandbox_filesystem_resolution(requested_access),
        }
    }
}

pub fn format_policy_block(block: PolicyBlock) -> String {
    RUNTIME_POLICY_BLOCKED_TEMPLATE
        .replace("{{blocked_by}}", &block.blocked_by)
        .replace("{{capability}}", &block.capability)
        .replace("{{requested_access}}", block.requested_access.as_str())
        .replace("{{target}}", &block.target)
        .replace("{{sandbox_preset}}", &block.sandbox_preset)
        .replace("{{effective_access}}", &block.effective_access)
        .replace("{{detail}}", &block.detail)
        .replace("{{resolution}}", &block.resolution)
        .replace(
            "{{equivalent_methods}}",
            block.requested_access.equivalent_methods(),
        )
}

fn file_access_label(access: FileAccess) -> &'static str {
    match access {
        FileAccess::None => "none",
        FileAccess::Ro => "ro",
        FileAccess::Rw => "rw",
    }
}

fn tool_scope_resolution(
    capability: impl Into<String>,
    requested_access: RequestedAccess,
) -> String {
    let capability = capability.into();
    match capability.as_str() {
        "read_file" => {
            "- `read_file` is intentionally scoped to files accepted by the built-in file tool.\n- If the user wants this exact host path, ask them to either move/copy the file into the accepted workspace path or change the task so an allowed capability can access it.\n- Do not switch to shell/process for the same target unless the user explicitly changes sandbox/filesystem policy for that target.".to_string()
        }
        "write_file" => {
            "- `write_file` is intentionally scoped to files accepted by the built-in file tool.\n- If the user wants this exact host path, ask them to either choose a writable workspace path or change sandbox/filesystem policy and use an allowed write-capable method.\n- Do not switch to shell/process for the same target unless the user explicitly changes sandbox/filesystem policy for that target.".to_string()
        }
        _ => format!(
            "- Ask the user to change the target or configure a capability that is allowed to perform this {} access.\n- Do not bypass this capability boundary with an equivalent method.",
            requested_access.as_str()
        ),
    }
}

fn sandbox_filesystem_resolution(requested_access: RequestedAccess) -> String {
    match requested_access {
        RequestedAccess::Read => {
            "- Ask the user to add a read-only filesystem mount or a more specific allow rule for this target in the active sandbox preset.\n- Prefer read-only access (`access: \"ro\"`) for inspection tasks.\n- Example mount: `{ \"path\": \"/absolute/path/or/directory\", \"access\": \"ro\" }`.".to_string()
        }
        RequestedAccess::Write => {
            "- Ask the user to add a writable filesystem mount or a more specific writable rule for this target in the active sandbox preset.\n- Use write access (`access: \"rw\"`) only when the task really needs to modify that path.\n- Example mount: `{ \"path\": \"/absolute/path/or/directory\", \"access\": \"rw\" }`.".to_string()
        }
        RequestedAccess::Execute => {
            "- Ask the user to change the sandbox preset or choose a command/target that is executable within the active sandbox.\n- Do not request broad access when a narrower filesystem mount is enough.".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_block_mentions_only_read_retries() {
        let text = format_policy_block(PolicyBlock::sandbox_filesystem(
            "workspace",
            "read_file",
            RequestedAccess::Read,
            "/secret",
            FileAccess::None,
        ));

        assert!(text.contains("requested_access: read"));
        assert!(text.contains("retryable: false"));
        assert!(text.contains("request_filesystem_access"));
        assert!(text.contains("concrete file or directory path"));
        assert!(!text.contains("requested_access: write"));
    }

    #[test]
    fn write_block_preserves_readonly_effective_access() {
        let text = format_policy_block(PolicyBlock::sandbox_filesystem(
            "workspace",
            "write_file",
            RequestedAccess::Write,
            ".git/config",
            FileAccess::Ro,
        ));

        assert!(text.contains("requested_access: write"));
        assert!(text.contains("effective_access: ro"));
        assert!(text.contains("concrete file or directory path"));
    }
}
