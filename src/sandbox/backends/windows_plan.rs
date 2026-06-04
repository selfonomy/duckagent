use crate::sandbox::config::{FileAccess, NetworkMode, PermissionAction, ResolvedSandbox};
use crate::sandbox::matcher::normalize_path_text;
use crate::sandbox::path_vars::resolve_config_path;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WindowsSandboxPlan {
    pub full_access: bool,
    pub filesystem: WindowsFileSystemPlan,
    pub network: WindowsNetworkPlan,
    pub secrets: WindowsSecretsPlan,
    pub environment: WindowsEnvironmentPlan,
    pub execution: WindowsExecutionPlan,
    pub required_enforcement: Vec<WindowsEnforcementRequirement>,
    pub unsupported_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WindowsFileSystemPlan {
    pub mounts: Vec<WindowsPathAccess>,
    pub rules: Vec<WindowsPathAccess>,
    pub read_roots: Vec<WindowsPathAccess>,
    pub write_roots: Vec<WindowsPathAccess>,
    pub protected_paths: Vec<WindowsProtectedPath>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WindowsPathAccess {
    pub path: String,
    pub access: FileAccess,
    pub glob: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WindowsProtectedPath {
    pub path: String,
    pub requested_access: FileAccess,
    pub glob: bool,
    pub deny_read: bool,
    pub deny_write: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WindowsNetworkPlan {
    pub mode: NetworkMode,
    pub hosts: Vec<WindowsHostRule>,
    pub addresses: Vec<WindowsAddressRule>,
    pub requires_windows_firewall_outbound_block: bool,
    pub requires_windows_firewall_loopback_proxy_allowlist: bool,
    pub requires_managed_proxy: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WindowsHostRule {
    pub pattern: String,
    pub action: PermissionAction,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WindowsAddressRule {
    pub pattern: String,
    pub action: PermissionAction,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WindowsSecretsPlan {
    pub secret_count: usize,
    pub exposed_env_placeholder_count: usize,
    pub injected_secret_count: usize,
    pub requires_secret_proxy: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WindowsEnvironmentPlan {
    pub sanitized_environment: bool,
    pub proxy_environment_managed_by_duckagent: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WindowsExecutionPlan {
    pub runner: WindowsRunnerKind,
    pub requires_conpty_or_pipe_runner: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WindowsRunnerKind {
    Direct,
    ElevatedSetupRunner,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WindowsEnforcementRequirement {
    SetupHelper,
    FilesystemAcl,
    FilesystemReadIsolation,
    FilesystemGlobExpansion,
    WindowsFirewallOutboundBlock,
    WindowsFirewallLoopbackProxyAllowlist,
    ManagedProxy,
    SecretProxy,
    SanitizedEnvironment,
    ConptyOrPipeRunner,
}

impl WindowsSandboxPlan {
    pub fn from_sandbox(sandbox: &ResolvedSandbox, cwd: &Path) -> Self {
        let full_access = is_full_access(sandbox);
        let mounts = sandbox
            .preset
            .filesystem
            .mounts
            .iter()
            .map(|mount| WindowsPathAccess {
                path: normalize_config_path(&mount.path, cwd),
                access: mount.access,
                glob: contains_glob(&mount.path),
            })
            .collect::<Vec<_>>();
        let rules = sandbox
            .preset
            .filesystem
            .rules
            .iter()
            .map(|rule| WindowsPathAccess {
                path: normalize_config_path(&rule.path, cwd),
                access: rule.access,
                glob: contains_glob(&rule.path),
            })
            .collect::<Vec<_>>();
        let filesystem = WindowsFileSystemPlan::from_paths(mounts, rules);
        let network = WindowsNetworkPlan::from_sandbox(sandbox);
        let secrets = WindowsSecretsPlan::from_sandbox(sandbox);
        let environment = WindowsEnvironmentPlan {
            sanitized_environment: true,
            proxy_environment_managed_by_duckagent: matches!(
                sandbox.preset.network.mode,
                NetworkMode::Proxy
            ),
        };
        let execution = WindowsExecutionPlan {
            runner: if full_access {
                WindowsRunnerKind::Direct
            } else {
                WindowsRunnerKind::ElevatedSetupRunner
            },
            requires_conpty_or_pipe_runner: false,
        };
        let required_enforcement = required_enforcement(
            full_access,
            &filesystem,
            &network,
            &secrets,
            &environment,
            &execution,
        );
        let unsupported_reasons = unsupported_reasons(full_access, &required_enforcement);
        Self {
            full_access,
            filesystem,
            network,
            secrets,
            environment,
            execution,
            required_enforcement,
            unsupported_reasons,
        }
    }

    pub fn unsupported_summary(&self) -> String {
        if self.unsupported_reasons.is_empty() {
            return "none".to_string();
        }
        self.unsupported_reasons.join("; ")
    }

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn required_enforcement_summary(&self) -> String {
        if self.required_enforcement.is_empty() {
            return "none".to_string();
        }
        self.required_enforcement
            .iter()
            .map(|requirement| requirement.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn supported_runtime_summary(&self) -> String {
        if self.full_access {
            return "danger preset runs without an OS sandbox by design".to_string();
        }

        match self.network.mode {
            NetworkMode::Allow => format!(
                "Windows sandbox backend can enforce this preset as a filesystem sandbox with direct network access using {}",
                self.required_enforcement_summary()
            ),
            NetworkMode::Deny => format!(
                "Windows sandbox backend can enforce this preset with {}",
                self.required_enforcement_summary()
            ),
            NetworkMode::Proxy => format!(
                "Windows sandbox backend can enforce this preset with {}. {}",
                self.required_enforcement_summary(),
                self.network_enforcement_guidance()
            ),
        }
    }

    pub fn unsupported_runtime_summary(&self) -> String {
        format!(
            "Windows sandbox backend cannot safely enforce this preset yet; unsupported presets fail closed. Missing enforcement: {}. {}",
            self.unsupported_summary(),
            self.network_enforcement_guidance()
        )
    }

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn unsupported_command_error(&self, program: &str) -> String {
        format!(
            "{} Refusing to run `{}` unsandboxed. Resolved sandbox plan: {}",
            self.unsupported_runtime_summary(),
            program,
            serde_json::to_string(self).unwrap_or_else(|_| "<unserializable>".to_string())
        )
    }

    pub fn network_enforcement_guidance(&self) -> &'static str {
        match self.network.mode {
            NetworkMode::Allow => {
                "network.mode=allow does not require network isolation; any missing enforcement is in filesystem/process setup."
            }
            NetworkMode::Deny => {
                "network.mode=deny is enforced with Windows Firewall outbound blocking scoped to the sandbox identity."
            }
            NetworkMode::Proxy => {
                "network.mode=proxy is enforced with Windows Firewall outbound blocking plus a loopback-only allowlist for duckagent's managed proxy. Injecting HTTP_PROXY/HTTPS_PROXY alone is intentionally not treated as sandboxing because child processes can bypass proxy environment variables."
            }
        }
    }
}

impl WindowsFileSystemPlan {
    fn from_paths(mounts: Vec<WindowsPathAccess>, rules: Vec<WindowsPathAccess>) -> Self {
        let read_roots = mounts
            .iter()
            .filter(|path| path.access.can_read() && path.path != "*")
            .cloned()
            .collect();
        let write_roots = mounts
            .iter()
            .filter(|path| path.access.can_write() && path.path != "*")
            .cloned()
            .collect();
        let protected_paths = rules
            .iter()
            .filter_map(|path| {
                let deny_read = !path.access.can_read();
                let deny_write = !path.access.can_write();
                (deny_read || deny_write).then(|| WindowsProtectedPath {
                    path: path.path.clone(),
                    requested_access: path.access,
                    glob: path.glob,
                    deny_read,
                    deny_write,
                })
            })
            .collect();
        Self {
            mounts,
            rules,
            read_roots,
            write_roots,
            protected_paths,
        }
    }

    fn requires_current_user_read_isolation(&self) -> bool {
        let full_read_mount = self
            .mounts
            .iter()
            .any(|mount| mount.path == "*" && mount.access.can_read());
        let has_read_deny_rule = self.protected_paths.iter().any(|path| path.deny_read);
        !full_read_mount || has_read_deny_rule
    }
}

impl WindowsNetworkPlan {
    fn from_sandbox(sandbox: &ResolvedSandbox) -> Self {
        let mode = sandbox.preset.network.mode.clone();
        Self {
            requires_windows_firewall_outbound_block: matches!(
                &mode,
                NetworkMode::Deny | NetworkMode::Proxy
            ),
            requires_windows_firewall_loopback_proxy_allowlist: matches!(&mode, NetworkMode::Proxy),
            requires_managed_proxy: matches!(&mode, NetworkMode::Proxy),
            mode,
            hosts: sandbox
                .preset
                .network
                .hosts
                .iter()
                .map(|(pattern, action)| WindowsHostRule {
                    pattern: pattern.clone(),
                    action: action.clone(),
                })
                .collect(),
            addresses: sandbox
                .preset
                .network
                .addresses
                .iter()
                .map(|(pattern, action)| WindowsAddressRule {
                    pattern: pattern.clone(),
                    action: action.clone(),
                })
                .collect(),
        }
    }
}

impl WindowsSecretsPlan {
    fn from_sandbox(sandbox: &ResolvedSandbox) -> Self {
        let secret_count = sandbox.preset.secrets.0.len();
        let exposed_env_placeholder_count = secret_count;
        let injected_secret_count = secret_count;
        Self {
            secret_count,
            exposed_env_placeholder_count,
            injected_secret_count,
            requires_secret_proxy: injected_secret_count > 0,
        }
    }
}

pub fn is_full_access(sandbox: &ResolvedSandbox) -> bool {
    sandbox.is_full_access()
}

fn normalize_config_path(path: &str, cwd: &Path) -> String {
    if path == "*" {
        return path.to_string();
    }
    normalize_path_text(&resolve_config_path(path, cwd))
}

fn contains_glob(pattern: &str) -> bool {
    pattern
        .chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | ']'))
}

fn required_enforcement(
    full_access: bool,
    filesystem: &WindowsFileSystemPlan,
    network: &WindowsNetworkPlan,
    secrets: &WindowsSecretsPlan,
    environment: &WindowsEnvironmentPlan,
    execution: &WindowsExecutionPlan,
) -> Vec<WindowsEnforcementRequirement> {
    if full_access {
        return Vec::new();
    }
    let mut requirements = vec![
        WindowsEnforcementRequirement::SetupHelper,
        WindowsEnforcementRequirement::FilesystemAcl,
    ];
    if filesystem.requires_current_user_read_isolation() {
        requirements.push(WindowsEnforcementRequirement::FilesystemReadIsolation);
    }
    if filesystem
        .mounts
        .iter()
        .chain(filesystem.rules.iter())
        .any(|path| path.glob)
    {
        requirements.push(WindowsEnforcementRequirement::FilesystemGlobExpansion);
    }
    if network.requires_windows_firewall_outbound_block {
        requirements.push(WindowsEnforcementRequirement::WindowsFirewallOutboundBlock);
    }
    if network.requires_windows_firewall_loopback_proxy_allowlist {
        requirements.push(WindowsEnforcementRequirement::WindowsFirewallLoopbackProxyAllowlist);
    }
    if network.requires_managed_proxy {
        requirements.push(WindowsEnforcementRequirement::ManagedProxy);
    }
    if secrets.requires_secret_proxy {
        requirements.push(WindowsEnforcementRequirement::SecretProxy);
    }
    if environment.sanitized_environment {
        requirements.push(WindowsEnforcementRequirement::SanitizedEnvironment);
    }
    if execution.requires_conpty_or_pipe_runner {
        requirements.push(WindowsEnforcementRequirement::ConptyOrPipeRunner);
    }
    requirements
}

fn unsupported_reasons(
    full_access: bool,
    required: &[WindowsEnforcementRequirement],
) -> Vec<String> {
    if full_access {
        return Vec::new();
    }
    required
        .iter()
        .filter(|requirement| requirement.is_unimplemented_in_current_backend())
        .map(|requirement| {
            format!(
                "{} is required but the duckagent Windows backend has not wired it yet",
                requirement.as_str()
            )
        })
        .collect()
}

impl WindowsEnforcementRequirement {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SetupHelper => "elevated/non-elevated setup helper",
            Self::FilesystemAcl => "filesystem ACL enforcement",
            Self::FilesystemReadIsolation => {
                "filesystem read isolation from the current user profile"
            }
            Self::FilesystemGlobExpansion => "filesystem glob expansion before ACL application",
            Self::WindowsFirewallOutboundBlock => "Windows Firewall outbound block",
            Self::WindowsFirewallLoopbackProxyAllowlist => {
                "Windows Firewall loopback proxy allowlist"
            }
            Self::ManagedProxy => "managed proxy",
            Self::SecretProxy => "secret proxy",
            Self::SanitizedEnvironment => "sanitized environment block",
            Self::ConptyOrPipeRunner => "ConPTY or pipe runner",
        }
    }

    fn is_unimplemented_in_current_backend(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::config::SandboxConfig;
    use std::sync::{Mutex, OnceLock};

    static ENV_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[test]
    fn workspace_plan_preserves_mounts_rules_and_network() {
        let sandbox = SandboxConfig::default()
            .resolve(Some("workspace"))
            .expect("workspace preset should resolve");
        let plan = WindowsSandboxPlan::from_sandbox(&sandbox, Path::new("C:/repo"));

        assert!(!plan.full_access);
        assert!(
            plan.filesystem
                .mounts
                .iter()
                .any(|mount| { mount.path == "*" && mount.access == FileAccess::Ro })
        );
        assert!(
            plan.filesystem
                .read_roots
                .iter()
                .all(|mount| mount.path != "*"),
            "wildcard read mount must not become a root ACL grant"
        );
        assert!(
            plan.filesystem
                .mounts
                .iter()
                .any(|mount| mount.path == "C:/repo" && mount.access == FileAccess::Rw)
        );
        let temp_dir = normalize_path_text(&std::env::temp_dir());
        assert!(
            plan.filesystem
                .mounts
                .iter()
                .any(|mount| mount.path == temp_dir && mount.access == FileAccess::Rw)
        );
        assert!(plan.filesystem.rules.iter().any(|rule| {
            rule.path.ends_with(".env") && rule.access == FileAccess::None && !rule.glob
        }));
        assert_eq!(plan.network.mode, NetworkMode::Proxy);
        assert!(plan.network.requires_windows_firewall_outbound_block);
        assert!(
            plan.network
                .requires_windows_firewall_loopback_proxy_allowlist
        );
        assert!(plan.network.requires_managed_proxy);
        assert_eq!(
            plan.execution.runner,
            WindowsRunnerKind::ElevatedSetupRunner
        );
        assert!(
            plan.required_enforcement
                .contains(&WindowsEnforcementRequirement::SetupHelper)
        );
        assert!(
            plan.required_enforcement
                .contains(&WindowsEnforcementRequirement::FilesystemAcl)
        );
        assert!(
            plan.required_enforcement
                .contains(&WindowsEnforcementRequirement::FilesystemReadIsolation)
        );
        assert!(
            plan.required_enforcement
                .contains(&WindowsEnforcementRequirement::WindowsFirewallOutboundBlock)
        );
        assert!(
            plan.required_enforcement
                .contains(&WindowsEnforcementRequirement::ManagedProxy)
        );
        assert!(plan.unsupported_reasons.is_empty());
        assert_eq!(plan.unsupported_summary(), "none");
        assert!(
            plan.network.hosts.iter().any(|rule| {
                rule.pattern == "localhost" && rule.action == PermissionAction::Allow
            })
        );
        let message = plan.supported_runtime_summary();
        assert!(message.contains("Windows Firewall outbound block"));
        assert!(message.contains("HTTP_PROXY/HTTPS_PROXY alone"));
        assert!(!plan.unsupported_summary().contains("managed proxy"));
    }

    #[test]
    fn danger_plan_marks_full_access() {
        let sandbox = SandboxConfig::default()
            .resolve(Some("danger"))
            .expect("danger preset should resolve");
        let plan = WindowsSandboxPlan::from_sandbox(&sandbox, Path::new("C:/repo"));

        assert!(plan.full_access);
        assert_eq!(plan.network.mode, NetworkMode::Allow);
        assert!(plan.required_enforcement.is_empty());
        assert!(plan.unsupported_reasons.is_empty());
        assert!(
            plan.filesystem
                .mounts
                .iter()
                .any(|mount| mount.path == "*" && mount.access == FileAccess::Rw)
        );
    }

    #[test]
    fn readonly_plan_requires_acl_and_no_network_but_not_proxy() {
        let sandbox = SandboxConfig::default()
            .resolve(Some("readonly"))
            .expect("readonly preset should resolve");
        let plan = WindowsSandboxPlan::from_sandbox(&sandbox, Path::new("C:/repo"));

        assert!(!plan.full_access);
        assert_eq!(plan.network.mode, NetworkMode::Deny);
        assert!(plan.network.requires_windows_firewall_outbound_block);
        assert!(!plan.network.requires_managed_proxy);
        assert_eq!(
            plan.execution.runner,
            WindowsRunnerKind::ElevatedSetupRunner
        );
        assert!(
            plan.required_enforcement
                .contains(&WindowsEnforcementRequirement::SetupHelper)
        );
        assert!(
            plan.required_enforcement
                .contains(&WindowsEnforcementRequirement::FilesystemAcl)
        );
        assert!(
            plan.required_enforcement
                .contains(&WindowsEnforcementRequirement::FilesystemReadIsolation)
        );
        assert!(
            plan.required_enforcement
                .contains(&WindowsEnforcementRequirement::WindowsFirewallOutboundBlock)
        );
        assert!(
            !plan
                .required_enforcement
                .contains(&WindowsEnforcementRequirement::ManagedProxy)
        );
        assert!(plan.unsupported_reasons.is_empty());
        let message = plan.supported_runtime_summary();
        assert!(message.contains("Windows Firewall outbound block"));
    }

    #[test]
    fn network_allow_still_requires_filesystem_sandbox_when_not_full_access() {
        let mut config = SandboxConfig::default();
        config.preset = "custom".to_string();
        config.presets.insert(
            "custom".to_string(),
            crate::sandbox::config::SandboxPresetConfig {
                extends: Some("workspace".to_string()),
                filesystem: None,
                network: Some(crate::sandbox::config::NetworkRulesConfig {
                    mode: Some(NetworkMode::Allow),
                    hosts: Default::default(),
                    addresses: Default::default(),
                }),
                env: None,
                permissions: None,
                ..Default::default()
            },
        );
        let sandbox = config
            .resolve(Some("custom"))
            .expect("custom preset should resolve");
        let plan = WindowsSandboxPlan::from_sandbox(&sandbox, Path::new("C:/repo"));

        assert!(!plan.full_access);
        assert_eq!(plan.network.mode, NetworkMode::Allow);
        assert!(!plan.network.requires_windows_firewall_outbound_block);
        assert_eq!(
            plan.execution.runner,
            WindowsRunnerKind::ElevatedSetupRunner
        );
        assert!(
            plan.required_enforcement
                .contains(&WindowsEnforcementRequirement::SetupHelper)
        );
        assert!(
            plan.required_enforcement
                .contains(&WindowsEnforcementRequirement::FilesystemAcl)
        );
        assert!(
            plan.required_enforcement
                .contains(&WindowsEnforcementRequirement::FilesystemReadIsolation)
        );
        assert!(
            !plan
                .required_enforcement
                .contains(&WindowsEnforcementRequirement::WindowsFirewallOutboundBlock)
        );
        assert!(plan.unsupported_reasons.is_empty());
        let message = plan.supported_runtime_summary();
        assert!(message.contains("filesystem sandbox"));
    }

    #[test]
    fn windows_plan_preserves_addresses_and_secret_requirements() {
        let _guard = ENV_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env test lock poisoned");
        unsafe {
            std::env::set_var("DUCKAGENT_WINDOWS_TEST_OPENAI_API_KEY", "secret");
            std::env::set_var(
                "DUCKAGENT_WINDOWS_TEST_OPENAI_BASE_URL",
                "https://api.example.com",
            );
        }
        let value = serde_json::json!({
            "preset": "custom",
            "presets": {
                "custom": {
                    "filesystem": {
                        "mounts": [{"path": "$CWD", "access": "rw"}],
                        "rules": [{"path": "$CWD/private/**", "access": "none"}]
                    },
                    "network": {
                        "mode": "proxy",
                        "hosts": {"*": "ask", "api.example.com": "allow"},
                        "addresses": {"169.254.0.0/16": "deny"}
                    },
                    "env": {
                        "DUCKAGENT_WINDOWS_TEST_OPENAI_API_KEY": {
                            "type": "secret",
                            "inject": {
                                "url": "DUCKAGENT_WINDOWS_TEST_OPENAI_BASE_URL",
                                "header": "Authorization",
                                "format": "Bearer {}"
                            }
                        }
                    }
                }
            }
        });
        let sandbox = serde_json::from_value::<SandboxConfig>(value)
            .expect("sandbox config should parse")
            .resolve(None)
            .expect("sandbox config should resolve");
        let plan = WindowsSandboxPlan::from_sandbox(&sandbox, Path::new("C:/repo"));

        assert!(
            plan.filesystem
                .mounts
                .iter()
                .any(|mount| mount.path == "C:/repo" && mount.access == FileAccess::Rw)
        );
        assert!(
            plan.filesystem
                .rules
                .iter()
                .any(|rule| rule.path == "C:/repo/private/**" && rule.glob)
        );
        assert!(plan.network.addresses.iter().any(|rule| {
            rule.pattern == "169.254.0.0/16" && rule.action == PermissionAction::Deny
        }));
        assert_eq!(plan.secrets.secret_count, 1);
        assert_eq!(plan.secrets.exposed_env_placeholder_count, 1);
        assert_eq!(plan.secrets.injected_secret_count, 1);
        assert!(plan.secrets.requires_secret_proxy);
        assert!(
            plan.required_enforcement
                .contains(&WindowsEnforcementRequirement::SecretProxy)
        );
        assert!(
            plan.required_enforcement
                .contains(&WindowsEnforcementRequirement::FilesystemReadIsolation)
        );
        assert!(!plan.unsupported_summary().contains("secret proxy"));
        assert_eq!(plan.unsupported_summary(), "none");
        unsafe {
            std::env::remove_var("DUCKAGENT_WINDOWS_TEST_OPENAI_API_KEY");
            std::env::remove_var("DUCKAGENT_WINDOWS_TEST_OPENAI_BASE_URL");
        }
    }

    #[test]
    fn no_env_secret_entries_do_not_require_windows_secret_proxy() {
        let value = serde_json::json!({
            "preset": "custom",
            "presets": {
                "custom": {
                    "filesystem": {
                        "mounts": [{"path": "$CWD", "access": "rw"}],
                        "rules": []
                    },
                    "network": {
                        "mode": "allow",
                        "hosts": {"*": "allow"}
                    }
                }
            }
        });
        let sandbox = serde_json::from_value::<SandboxConfig>(value)
            .expect("sandbox config should parse")
            .resolve(None)
            .expect("sandbox config should resolve");
        let plan = WindowsSandboxPlan::from_sandbox(&sandbox, Path::new("C:/repo"));

        assert_eq!(plan.secrets.secret_count, 0);
        assert_eq!(plan.secrets.exposed_env_placeholder_count, 0);
        assert_eq!(plan.secrets.injected_secret_count, 0);
        assert!(!plan.secrets.requires_secret_proxy);
        assert!(
            !plan
                .required_enforcement
                .contains(&WindowsEnforcementRequirement::SecretProxy)
        );
        assert!(plan.unsupported_reasons.is_empty());
        assert_eq!(plan.unsupported_summary(), "none");
    }
}
