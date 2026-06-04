use crate::sandbox::config::path_is_core_protected;
use crate::sandbox::config::{FileAccess, PermissionAction, ResolvedSandbox};
use crate::sandbox::matcher::{normalize_path_text, path_pattern_matches};
use crate::sandbox::policy_block::{PolicyBlock, RequestedAccess, format_policy_block};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessKind {
    Read,
    Write,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionMatch {
    Allow,
    Ask,
    Deny,
    Unspecified,
}

impl From<PermissionAction> for PermissionMatch {
    fn from(value: PermissionAction) -> Self {
        match value {
            PermissionAction::Allow => Self::Allow,
            PermissionAction::Ask => Self::Ask,
            PermissionAction::Deny => Self::Deny,
        }
    }
}

pub fn ensure_path_allowed(
    sandbox: &ResolvedSandbox,
    kind: AccessKind,
    path: &Path,
    workspace: &Path,
    capability: &str,
) -> Result<()> {
    let access = effective_file_access(sandbox, path, workspace);
    let allowed = match kind {
        AccessKind::Read => access.can_read(),
        AccessKind::Write => access.can_write(),
    };
    audit_filesystem_access(sandbox, kind, path, access, allowed, capability);
    if !allowed {
        bail!(
            "{}",
            format_policy_block(PolicyBlock::sandbox_filesystem(
                sandbox.name.clone(),
                capability.trim(),
                kind.into(),
                normalize_path_text(path),
                access,
            ))
        );
    }
    Ok(())
}

fn audit_filesystem_access(
    sandbox: &ResolvedSandbox,
    kind: AccessKind,
    path: &Path,
    access: FileAccess,
    allowed: bool,
    capability: &str,
) {
    let mut event = crate::audit::AuditEvent::new("filesystem", "access_check");
    event.sandbox = Some(sandbox.name.clone());
    event.target = Some(normalize_path_text(path));
    event.outcome = if allowed { "allow" } else { "blocked" }.to_string();
    event.fields = serde_json::json!({
        "capability": capability.trim(),
        "requested_access": match kind {
            AccessKind::Read => "read",
            AccessKind::Write => "write",
        },
        "effective_access": access,
    });
    crate::audit::record(event);
}

impl From<AccessKind> for RequestedAccess {
    fn from(value: AccessKind) -> Self {
        match value {
            AccessKind::Read => Self::Read,
            AccessKind::Write => Self::Write,
        }
    }
}

pub fn effective_file_access(
    sandbox: &ResolvedSandbox,
    path: &Path,
    workspace: &Path,
) -> FileAccess {
    if path_is_core_protected(path) {
        return FileAccess::None;
    }
    let mut best: Option<(usize, usize, FileAccess)> = None;
    let mut order = 0usize;
    for mount in &sandbox.preset.filesystem.mounts {
        if path_pattern_matches(&mount.path, path, workspace) {
            update_access_match(
                &mut best,
                pattern_specificity(&mount.path),
                order,
                mount.access,
            );
        }
        order += 1;
    }

    for rule in &sandbox.preset.filesystem.rules {
        if path_pattern_matches(&rule.path, path, workspace) {
            update_access_match(
                &mut best,
                pattern_specificity(&rule.path),
                order,
                rule.access,
            );
        }
        order += 1;
    }
    best.map(|(_, _, access)| access)
        .unwrap_or(FileAccess::None)
}

fn update_access_match(
    best: &mut Option<(usize, usize, FileAccess)>,
    specificity: usize,
    order: usize,
    access: FileAccess,
) {
    match best {
        Some((best_specificity, best_order, _))
            if (*best_specificity, *best_order) > (specificity, order) => {}
        _ => *best = Some((specificity, order, access)),
    }
}

fn pattern_specificity(pattern: &str) -> usize {
    pattern
        .chars()
        .filter(|ch| !matches!(ch, '*' | '?' | '[' | ']' | '{' | '}' | ','))
        .count()
}

pub fn tool_permission(sandbox: &ResolvedSandbox, tool_name: &str) -> PermissionMatch {
    let tool_name = tool_name.trim();
    crate::sandbox::shell_permissions::permission_action_for_pattern(
        &sandbox.preset.permissions.tools,
        tool_name,
        None,
    )
    .map(Into::into)
    .unwrap_or(PermissionMatch::Unspecified)
}

pub fn shell_explicit_permission(sandbox: &ResolvedSandbox, command: &str) -> PermissionMatch {
    crate::sandbox::shell_permissions::evaluate_shell_permission(sandbox, command).action
}

pub fn workspace_path() -> Result<PathBuf> {
    std::env::current_dir().map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::config::{FileSystemMount, FileSystemRule, SandboxConfig};
    use anyhow::Context;

    fn resolved_workspace() -> ResolvedSandbox {
        SandboxConfig::default()
            .resolve(Some("workspace"))
            .expect("workspace sandbox preset should resolve")
    }

    #[test]
    fn workspace_preset_reads_outside_workspace_but_only_writes_workspace() -> Result<()> {
        let sandbox = resolved_workspace();
        let workspace = Path::new("/tmp/workspace-project");
        let outside = Path::new("/var/log/some-tool.log");
        let inside = workspace.join("src/main.rs");

        assert_eq!(
            effective_file_access(&sandbox, outside, workspace),
            FileAccess::Ro
        );
        assert!(
            ensure_path_allowed(&sandbox, AccessKind::Read, outside, workspace, "read_file")
                .is_ok()
        );
        assert!(
            ensure_path_allowed(
                &sandbox,
                AccessKind::Write,
                outside,
                workspace,
                "write_file"
            )
            .is_err()
        );
        assert_eq!(
            effective_file_access(&sandbox, &inside, workspace),
            FileAccess::Rw
        );
        Ok(())
    }

    #[test]
    fn readonly_preset_reads_outside_workspace_but_writes_nowhere() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("readonly"))?;
        let workspace = Path::new("/tmp/workspace-project");
        let outside = Path::new("/var/log/some-tool.log");
        let inside = workspace.join("src/main.rs");

        assert_eq!(
            effective_file_access(&sandbox, outside, workspace),
            FileAccess::Ro
        );
        assert!(
            ensure_path_allowed(&sandbox, AccessKind::Read, outside, workspace, "read_file")
                .is_ok()
        );
        assert!(
            ensure_path_allowed(
                &sandbox,
                AccessKind::Write,
                outside,
                workspace,
                "write_file"
            )
            .is_err()
        );
        assert_eq!(
            effective_file_access(&sandbox, &inside, workspace),
            FileAccess::Ro
        );
        Ok(())
    }

    #[test]
    fn sandbox_denies_root_env_file() -> Result<()> {
        let config = SandboxConfig::default();
        let sandbox = config.resolve(Some("workspace"))?;
        let workspace = Path::new("/workspace-test/work");
        let path = Path::new("/workspace-test/work/.env");
        assert!(
            ensure_path_allowed(&sandbox, AccessKind::Read, path, workspace, "read_file").is_err()
        );
        Ok(())
    }

    #[test]
    fn sandbox_rules_can_downgrade_writable_mount_to_readonly() -> Result<()> {
        let config = SandboxConfig::default();
        let sandbox = config.resolve(Some("workspace"))?;
        let workspace = Path::new("/workspace-test/work");
        let path = Path::new("/workspace-test/work/.git/config");

        assert_eq!(
            effective_file_access(&sandbox, path, workspace),
            FileAccess::Ro
        );
        assert!(
            ensure_path_allowed(&sandbox, AccessKind::Read, path, workspace, "read_file").is_ok()
        );
        assert!(
            ensure_path_allowed(&sandbox, AccessKind::Write, path, workspace, "write_file")
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn more_specific_filesystem_rule_can_reopen_wider_rule() -> Result<()> {
        let mut sandbox = resolved_workspace();
        sandbox
            .preset
            .filesystem
            .rules
            .push(crate::sandbox::config::FileSystemRule {
                path: "**/generated/**".to_string(),
                access: FileAccess::None,
            });
        sandbox
            .preset
            .filesystem
            .rules
            .push(crate::sandbox::config::FileSystemRule {
                path: "**/generated/public/**".to_string(),
                access: FileAccess::Rw,
            });
        let workspace = Path::new("/tmp/work");

        assert_eq!(
            effective_file_access(
                &sandbox,
                Path::new("/tmp/work/generated/private/out.txt"),
                workspace
            ),
            FileAccess::None
        );
        assert_eq!(
            effective_file_access(
                &sandbox,
                Path::new("/tmp/work/generated/public/out.txt"),
                workspace
            ),
            FileAccess::Rw
        );
        Ok(())
    }

    #[test]
    fn parent_none_rule_and_child_ro_rule_allow_only_child_subtree() -> Result<()> {
        let mut sandbox = resolved_workspace();
        sandbox.preset.filesystem.mounts = vec![FileSystemMount {
            path: "/foo".to_string(),
            access: FileAccess::Rw,
        }];
        sandbox.preset.filesystem.rules = vec![
            FileSystemRule {
                path: "/foo".to_string(),
                access: FileAccess::None,
            },
            FileSystemRule {
                path: "/foo/test".to_string(),
                access: FileAccess::Ro,
            },
        ];
        let workspace = Path::new("/");

        assert_eq!(
            effective_file_access(&sandbox, Path::new("/foo/test/1.md"), workspace),
            FileAccess::Ro
        );
        assert_eq!(
            effective_file_access(&sandbox, Path::new("/foo/other.md"), workspace),
            FileAccess::None
        );
        assert!(
            ensure_path_allowed(
                &sandbox,
                AccessKind::Read,
                Path::new("/foo/test/1.md"),
                workspace,
                "read_file"
            )
            .is_ok()
        );
        assert!(
            ensure_path_allowed(
                &sandbox,
                AccessKind::Write,
                Path::new("/foo/test/1.md"),
                workspace,
                "write_file"
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn parent_ro_rule_and_child_rw_rule_allow_writes_only_child_subtree() -> Result<()> {
        let mut sandbox = resolved_workspace();
        sandbox.preset.filesystem.mounts = vec![FileSystemMount {
            path: "/foo".to_string(),
            access: FileAccess::Rw,
        }];
        sandbox.preset.filesystem.rules = vec![
            FileSystemRule {
                path: "/foo".to_string(),
                access: FileAccess::Ro,
            },
            FileSystemRule {
                path: "/foo/test".to_string(),
                access: FileAccess::Rw,
            },
        ];
        let workspace = Path::new("/");

        assert_eq!(
            effective_file_access(&sandbox, Path::new("/foo/test/a.md"), workspace),
            FileAccess::Rw
        );
        assert_eq!(
            effective_file_access(&sandbox, Path::new("/foo/a.md"), workspace),
            FileAccess::Ro
        );
        Ok(())
    }

    #[test]
    fn same_path_later_rule_wins_for_none_then_ro() -> Result<()> {
        let mut sandbox = resolved_workspace();
        sandbox.preset.filesystem.mounts = vec![FileSystemMount {
            path: "/foo".to_string(),
            access: FileAccess::Rw,
        }];
        sandbox.preset.filesystem.rules = vec![
            FileSystemRule {
                path: "/foo".to_string(),
                access: FileAccess::None,
            },
            FileSystemRule {
                path: "/foo".to_string(),
                access: FileAccess::Ro,
            },
        ];
        let workspace = Path::new("/");

        assert_eq!(
            effective_file_access(&sandbox, Path::new("/foo/a.md"), workspace),
            FileAccess::Ro
        );
        assert!(
            ensure_path_allowed(
                &sandbox,
                AccessKind::Read,
                Path::new("/foo/a.md"),
                workspace,
                "read_file"
            )
            .is_ok()
        );
        assert!(
            ensure_path_allowed(
                &sandbox,
                AccessKind::Write,
                Path::new("/foo/a.md"),
                workspace,
                "write_file"
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn existing_rw_rule_is_not_downgraded_by_later_ro_request_when_config_helper_skips_ro_rule() {
        let mut sandbox = resolved_workspace();
        sandbox.preset.filesystem.mounts = vec![FileSystemMount {
            path: "/foo".to_string(),
            access: FileAccess::Rw,
        }];
        sandbox.preset.filesystem.rules = vec![FileSystemRule {
            path: "/foo".to_string(),
            access: FileAccess::Rw,
        }];
        let workspace = Path::new("/");

        assert_eq!(
            effective_file_access(&sandbox, Path::new("/foo/a.md"), workspace),
            FileAccess::Rw
        );
    }

    #[test]
    fn core_policy_keeps_duckagent_paths_none_even_with_explicit_rw_grant() -> Result<()> {
        let home = dirs::home_dir().context("home dir required for test")?;
        let target = home.join(".duckagent/config.json");
        let mut sandbox = resolved_workspace();
        sandbox.preset.filesystem.mounts.push(FileSystemMount {
            path: home.join(".duckagent").to_string_lossy().to_string(),
            access: FileAccess::Rw,
        });
        sandbox.preset.filesystem.rules.push(FileSystemRule {
            path: target.to_string_lossy().to_string(),
            access: FileAccess::Rw,
        });

        assert_eq!(
            effective_file_access(&sandbox, &target, Path::new("/")),
            FileAccess::None
        );
        Ok(())
    }

    #[test]
    fn core_policy_returns_generic_filesystem_block_prompt() -> Result<()> {
        let home = dirs::home_dir().context("home dir required for test")?;
        let target = home.join(".duckagent/config.json");
        let sandbox = resolved_workspace();

        let error = ensure_path_allowed(
            &sandbox,
            AccessKind::Read,
            &target,
            Path::new("/"),
            "read_file",
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("blocked_by: sandbox_filesystem"));
        assert!(error.contains("capability: read_file"));
        assert!(error.contains("requested_access: read"));
        assert!(error.contains("effective_access: none"));
        Ok(())
    }

    #[test]
    fn tool_glob_permissions_match_exposed_mcp_names() {
        let mut sandbox = resolved_workspace();
        sandbox
            .preset
            .permissions
            .tools
            .insert("context7_*".to_string(), PermissionAction::Ask);
        assert_eq!(
            tool_permission(&sandbox, "context7_search"),
            PermissionMatch::Ask
        );
        assert_eq!(
            tool_permission(&sandbox, "other_search"),
            PermissionMatch::Unspecified
        );
    }

    #[test]
    fn shell_permission_order_prefers_deny() {
        let mut sandbox = resolved_workspace();
        sandbox
            .preset
            .permissions
            .shell
            .rules
            .insert("git".to_string(), PermissionAction::Allow);
        sandbox
            .preset
            .permissions
            .shell
            .rules
            .insert("git push".to_string(), PermissionAction::Deny);
        assert_eq!(
            shell_explicit_permission(&sandbox, "git push origin main"),
            PermissionMatch::Deny
        );
        assert_eq!(
            shell_explicit_permission(&sandbox, "git status"),
            PermissionMatch::Allow
        );
    }

    #[test]
    fn default_unspecified_tool_is_not_denied() {
        let mut sandbox = resolved_workspace();
        sandbox.preset.permissions = Default::default();
        assert_eq!(
            tool_permission(&sandbox, "read_file"),
            PermissionMatch::Unspecified
        );
    }
}
