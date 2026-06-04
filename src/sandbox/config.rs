use crate::mcp::config::DuckAgentConfig;
use crate::sandbox::matcher::path_pattern_matches;
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const DEFAULT_PRESET_NAME: &str = "workspace";
const DANGER_PRESET_NAME: &str = "danger";
const READONLY_PRESET_NAME: &str = "readonly";
const BUILTIN_CUSTOM_PRESET_SUFFIX: &str = "-custom";
const WORKSPACE_PRESET_JSON: &str = include_str!("presets/workspace.json");
const READONLY_PRESET_JSON: &str = include_str!("presets/readonly.json");
const DANGER_PRESET_JSON: &str = include_str!("presets/danger.json");

static CLI_SANDBOX_OVERRIDE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static SANDBOX_ACCESS_GRANTS: OnceLock<Mutex<Vec<SandboxAccessGrant>>> = OnceLock::new();
#[cfg(test)]
static TEST_SANDBOX_OVERRIDE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) const SECRET_REVERSE_ROUTE_PREFIX: &str = "/__duckagent_secret";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    Deny,
    Allow,
    Proxy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    #[serde(default = "default_preset_name")]
    pub preset: String,
    #[serde(default)]
    pub presets: BTreeMap<String, SandboxPresetConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct SandboxPresetConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesystem: Option<FileSystemConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkRulesConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<EnvConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<PresetPermissions>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxPreset {
    pub filesystem: FileSystemPolicy,
    pub network: NetworkRules,
    pub env: EnvPolicy,
    pub permissions: PresetPermissions,
    pub secrets: SecretsPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileSystemConfig {
    #[serde(default)]
    pub mounts: Vec<FileSystemMount>,
    #[serde(default)]
    pub rules: Vec<FileSystemRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileSystemPolicy {
    pub mounts: Vec<FileSystemMount>,
    pub rules: Vec<FileSystemRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileSystemMount {
    pub path: String,
    pub access: FileAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileSystemRule {
    pub path: String,
    pub access: FileAccess,
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "lowercase")]
pub enum FileAccess {
    None,
    Ro,
    Rw,
}

impl FileAccess {
    pub fn can_read(self) -> bool {
        matches!(self, Self::Ro | Self::Rw)
    }

    pub fn can_write(self) -> bool {
        matches!(self, Self::Rw)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NetworkRules {
    pub mode: NetworkMode,
    pub hosts: BTreeMap<String, PermissionAction>,
    pub addresses: BTreeMap<String, PermissionAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct NetworkRulesConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<NetworkMode>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hosts: BTreeMap<String, PermissionAction>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub addresses: BTreeMap<String, PermissionAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct EnvConfig(pub BTreeMap<String, EnvEntry>);

pub type EnvPolicy = EnvConfig;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum EnvEntry {
    Action(PermissionAction),
    Secret(EnvSecretConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvSecretConfig {
    #[serde(rename = "type")]
    pub entry_type: String,
    pub inject: EnvSecretInjectConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvSecretInjectConfig {
    pub url: String,
    pub header: String,
    pub format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct PresetPermissions {
    #[serde(default)]
    pub tools: BTreeMap<String, PermissionAction>,
    #[serde(default)]
    pub shell: ShellPermissionRules,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(transparent)]
pub struct ShellPermissionRules {
    pub rules: BTreeMap<String, PermissionAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(transparent)]
pub struct SecretsConfig(pub BTreeMap<String, SecretConfig>);

pub type SecretsPolicy = SecretsConfig;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretConfig {
    pub source_env: String,
    pub url_env: String,
    pub inject: SecretInjectConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretInjectConfig {
    pub header: String,
    pub format: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSandbox {
    pub name: String,
    pub preset: SandboxPreset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxAccessGrant {
    pub path: PathBuf,
    pub access: FileAccess,
    pub once: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuiltinPresetFile {
    sandbox: SandboxConfig,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        let mut presets = BTreeMap::new();
        insert_builtin_preset(&mut presets, DEFAULT_PRESET_NAME);
        insert_builtin_preset(&mut presets, READONLY_PRESET_NAME);
        insert_builtin_preset(&mut presets, DANGER_PRESET_NAME);
        Self {
            preset: DEFAULT_PRESET_NAME.to_string(),
            presets,
        }
    }
}

impl SandboxConfig {
    pub fn ensure_builtin_defaults(&mut self) {
        ensure_builtin_preset(&mut self.presets, DEFAULT_PRESET_NAME);
        ensure_builtin_preset(&mut self.presets, READONLY_PRESET_NAME);
        ensure_builtin_preset(&mut self.presets, DANGER_PRESET_NAME);
        if self.preset.trim().is_empty() {
            self.preset = DEFAULT_PRESET_NAME.to_string();
        }
    }

    pub fn resolve(&self, override_name: Option<&str>) -> Result<ResolvedSandbox> {
        let selected = override_name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                self.preset
                    .trim()
                    .is_empty()
                    .then_some(DEFAULT_PRESET_NAME)
                    .or_else(|| Some(self.preset.trim()))
            })
            .unwrap_or(DEFAULT_PRESET_NAME);
        let preset = self.resolve_preset(selected, &mut BTreeSet::new())?;
        crate::sandbox::shell_permissions::validate_shell_permission_rules(
            &preset.permissions.shell,
        )
        .with_context(|| format!("sandbox preset `{selected}` has invalid shell permissions"))?;
        validate_resolved_preset(selected, &preset)?;
        Ok(ResolvedSandbox {
            name: selected.to_string(),
            preset,
        })
    }

    fn resolve_preset(&self, name: &str, visiting: &mut BTreeSet<String>) -> Result<SandboxPreset> {
        if !visiting.insert(name.to_string()) {
            bail!("sandbox preset `{name}` has a circular extends chain");
        }
        let config = self
            .presets
            .get(name)
            .with_context(|| format!("sandbox preset `{name}` not found"))?;
        let resolved = if let Some(base_name) = config.extends.as_deref() {
            let mut base = self.resolve_preset(base_name.trim(), visiting)?;
            base.merge_patch(name, config)?;
            base
        } else {
            SandboxPreset::from_full_config(name, config)?
        };
        visiting.remove(name);
        Ok(resolved)
    }
}

impl ResolvedSandbox {
    pub fn is_full_access(&self) -> bool {
        if self.name == DANGER_PRESET_NAME {
            return true;
        }
        self.preset
            .filesystem
            .mounts
            .iter()
            .any(|mount| mount.path == "*" && mount.access == FileAccess::Rw)
            && self.preset.filesystem.rules.is_empty()
            && matches!(self.preset.network.mode, NetworkMode::Allow)
    }
}

impl SandboxPreset {
    fn from_full_config(name: &str, config: &SandboxPresetConfig) -> Result<Self> {
        let filesystem = config
            .filesystem
            .clone()
            .with_context(|| {
                format!("sandbox preset `{name}` is missing `filesystem`; add it or use `extends`")
            })?
            .into_policy(name)?;
        let env = config.env.clone().unwrap_or_default().into_policy(name)?;
        let secrets = SecretsConfig::from_env_policy(name, &env)?;
        Ok(Self {
            filesystem,
            network: config
                .network
                .clone()
                .with_context(|| {
                    format!("sandbox preset `{name}` is missing `network`; add it or use `extends`")
                })?
                .into_rules(name)?,
            env,
            permissions: config.permissions.clone().unwrap_or_default(),
            secrets,
        })
    }

    fn merge_patch(&mut self, name: &str, patch: &SandboxPresetConfig) -> Result<()> {
        if let Some(filesystem) = &patch.filesystem {
            self.filesystem.merge_additive(filesystem);
        }
        if let Some(network) = &patch.network {
            self.network.merge_patch(network);
        }
        if let Some(env) = &patch.env {
            self.env.merge_patch(env);
        }
        if let Some(permissions) = &patch.permissions {
            self.permissions.merge_patch(permissions);
        }
        self.secrets = SecretsConfig::from_env_policy(name, &self.env)?;
        Ok(())
    }
}

impl FileSystemConfig {
    fn into_policy(self, preset_name: &str) -> Result<FileSystemPolicy> {
        for mount in &self.mounts {
            if mount.path.trim().is_empty() {
                bail!("sandbox preset `{preset_name}` has an empty filesystem mount path");
            }
            if matches!(mount.access, FileAccess::None) {
                bail!("sandbox preset `{preset_name}` filesystem mounts only support `ro` or `rw`");
            }
        }
        for rule in &self.rules {
            if rule.path.trim().is_empty() {
                bail!("sandbox preset `{preset_name}` has an empty filesystem rule path");
            }
        }
        Ok(FileSystemPolicy {
            mounts: self.mounts,
            rules: self.rules,
        })
    }
}

impl FileSystemPolicy {
    fn merge_additive(&mut self, patch: &FileSystemConfig) {
        append_unique_mounts(&mut self.mounts, &patch.mounts);
        append_unique_rules(&mut self.rules, &patch.rules);
    }
}

impl NetworkRules {
    pub fn default_action(&self) -> PermissionAction {
        self.hosts
            .get("*")
            .cloned()
            .unwrap_or(PermissionAction::Ask)
    }

    pub fn action_for_host(&self, host: &str) -> PermissionAction {
        let host = host
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        let mut best: Option<(usize, PermissionAction)> = None;
        for (pattern, action) in &self.hosts {
            let pattern = pattern.to_ascii_lowercase();
            if pattern == host || crate::sandbox::matcher::glob_matches(&pattern, &host) {
                update_permission_match(&mut best, host_rule_specificity(&pattern), *action);
            }
        }
        best.map(|(_, action)| action)
            .unwrap_or_else(|| self.default_action())
    }

    pub fn action_for_address(&self, address: IpAddr) -> PermissionAction {
        let mut best: Option<(usize, PermissionAction)> = None;
        for (pattern, action) in &self.addresses {
            if let Some(specificity) = address_rule_specificity(pattern, address) {
                update_permission_match(&mut best, specificity, *action);
            }
        }
        best.map(|(_, action)| action)
            .unwrap_or(PermissionAction::Allow)
    }

    fn merge_patch(&mut self, patch: &NetworkRulesConfig) {
        if let Some(mode) = patch.mode.clone() {
            self.mode = mode;
        }
        for (host, action) in &patch.hosts {
            self.hosts.insert(host.clone(), action.clone());
        }
        for (address, action) in &patch.addresses {
            self.addresses.insert(address.clone(), action.clone());
        }
    }
}

impl NetworkRulesConfig {
    fn into_rules(self, preset_name: &str) -> Result<NetworkRules> {
        validate_network_rule_patterns(preset_name, &self)?;
        let mode = self.mode.unwrap_or(NetworkMode::Proxy);
        let mut hosts = self.hosts;
        hosts.entry("*".to_string()).or_insert_with(|| match mode {
            NetworkMode::Deny => PermissionAction::Deny,
            NetworkMode::Allow => PermissionAction::Allow,
            NetworkMode::Proxy => PermissionAction::Ask,
        });
        Ok(NetworkRules {
            mode,
            hosts,
            addresses: self.addresses,
        })
    }
}

impl Default for EnvConfig {
    fn default() -> Self {
        Self(default_env_rules())
    }
}

impl EnvConfig {
    fn into_policy(mut self, preset_name: &str) -> Result<EnvPolicy> {
        self.0.retain(|key, _| !key.trim().is_empty());
        validate_env_secret_entries(preset_name, &self)?;
        Ok(self)
    }

    fn merge_patch(&mut self, patch: &EnvConfig) {
        for (key, entry) in &patch.0 {
            self.0.insert(key.clone(), entry.clone());
        }
    }

    pub fn permission_rules(&self) -> BTreeMap<String, PermissionAction> {
        self.0
            .iter()
            .filter_map(|(key, entry)| match entry {
                EnvEntry::Action(action) => Some((key.clone(), *action)),
                EnvEntry::Secret(_) => None,
            })
            .collect()
    }

    pub fn secret_entries(&self) -> impl Iterator<Item = (&String, &EnvSecretConfig)> {
        self.0.iter().filter_map(|(key, entry)| match entry {
            EnvEntry::Secret(secret) => Some((key, secret)),
            EnvEntry::Action(_) => None,
        })
    }
}

impl PresetPermissions {
    fn merge_patch(&mut self, patch: &PresetPermissions) {
        for (pattern, action) in &patch.tools {
            self.tools.insert(pattern.clone(), action.clone());
        }
        self.shell.merge_patch(&patch.shell);
    }
}

impl ShellPermissionRules {
    fn merge_patch(&mut self, patch: &ShellPermissionRules) {
        merge_permission_map(&mut self.rules, &patch.rules);
    }
}

impl SecretsConfig {
    fn from_env_policy(preset_name: &str, env: &EnvPolicy) -> Result<SecretsPolicy> {
        let mut secrets = BTreeMap::new();
        for (name, secret) in env.secret_entries() {
            validate_env_name(preset_name, "env secret name", name)?;
            let inject = &secret.inject;
            validate_env_name(preset_name, "env secret inject.url", &inject.url)?;
            validate_secret_endpoint(preset_name, name, "inject.header", &inject.header)?;
            validate_secret_endpoint(preset_name, name, "inject.format", &inject.format)?;
            if !inject.format.contains("{}") {
                bail!(
                    "sandbox preset `{preset_name}` env secret `{name}` inject.format must contain `{{}}`"
                );
            }
            validate_required_secret_env(preset_name, name, name)?;
            validate_required_secret_url_env(preset_name, name, &inject.url)?;
            secrets.insert(
                name.clone(),
                SecretConfig {
                    source_env: name.clone(),
                    url_env: inject.url.clone(),
                    inject: SecretInjectConfig {
                        header: inject.header.clone(),
                        format: inject.format.clone(),
                    },
                },
            );
        }
        Ok(Self(secrets))
    }

    pub fn exposed_env_placeholders(&self) -> BTreeMap<String, String> {
        self.0
            .iter()
            .map(|(name, secret)| (secret.source_env.clone(), secret.placeholder(name)))
            .collect()
    }

    pub fn proxy_env_overrides(&self, addr: SocketAddr) -> BTreeMap<String, String> {
        self.0
            .iter()
            .flat_map(|(name, secret)| {
                [
                    (secret.source_env.clone(), secret.placeholder(name)),
                    (secret.url_env.clone(), secret.reverse_base_url(addr, name)),
                ]
            })
            .collect()
    }
}

impl SecretConfig {
    pub fn source_value(&self) -> Option<String> {
        std::env::var(&self.source_env).ok()
    }

    pub fn placeholder(&self, name: &str) -> String {
        format!("duckagent-secret:{name}")
    }

    pub fn upstream_url(&self) -> Result<url::Url> {
        let value = std::env::var(&self.url_env)
            .with_context(|| format!("missing secret upstream env `{}`", self.url_env))?;
        parse_secret_upstream_url(&value)
            .with_context(|| format!("invalid secret upstream URL in env `{}`", self.url_env))
    }

    pub fn reverse_base_url(&self, addr: SocketAddr, name: &str) -> String {
        format!("http://{addr}{}/{}", SECRET_REVERSE_ROUTE_PREFIX, name)
    }
}

pub fn set_cli_sandbox_override(value: Option<String>) {
    let slot = CLI_SANDBOX_OVERRIDE.get_or_init(|| Mutex::new(None));
    *slot.lock().expect("sandbox override mutex poisoned") = value;
}

#[cfg(test)]
pub struct TestSandboxOverrideGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl TestSandboxOverrideGuard {
    pub fn new(value: impl Into<String>) -> Self {
        let lock = TEST_SANDBOX_OVERRIDE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        set_cli_sandbox_override(Some(value.into()));
        Self { _lock: lock }
    }
}

#[cfg(test)]
impl Drop for TestSandboxOverrideGuard {
    fn drop(&mut self) {
        set_cli_sandbox_override(None);
    }
}

pub fn cli_sandbox_override() -> Option<String> {
    CLI_SANDBOX_OVERRIDE
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|guard| guard.clone()))
}

pub fn resolve_sandbox() -> Result<ResolvedSandbox> {
    let override_name = cli_sandbox_override();
    let mut config = DuckAgentConfig::load_global()?.sandbox_config()?;
    config.ensure_builtin_defaults();
    let mut resolved = config.resolve(override_name.as_deref())?;
    apply_runtime_access_grants(&mut resolved);
    apply_core_filesystem_policy(&mut resolved);
    Ok(resolved)
}

pub fn load_sandbox_config() -> Result<SandboxConfig> {
    let mut config = DuckAgentConfig::load_global()?.sandbox_config()?;
    config.ensure_builtin_defaults();
    Ok(config)
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn set_active_sandbox_preset(name: &str) -> Result<()> {
    let mut config = DuckAgentConfig::load_global()?;
    let mut sandbox = config.sandbox_config()?;
    sandbox.ensure_builtin_defaults();
    set_active_sandbox_preset_in_config(&mut sandbox, name)?;
    config.set_sandbox_config(sandbox)?;
    config.save_global()
}

fn set_active_sandbox_preset_in_config(sandbox: &mut SandboxConfig, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        bail!("sandbox preset name cannot be empty");
    }
    sandbox
        .resolve(Some(name))
        .with_context(|| format!("cannot activate sandbox preset `{name}`"))?;
    sandbox.preset = name.to_string();
    Ok(())
}

pub fn append_shell_action_to_current_preset(
    command: &str,
    action: PermissionAction,
) -> Result<()> {
    let command = command.trim();
    if command.is_empty() {
        bail!("cannot persist an empty shell permission rule");
    }
    let override_name = cli_sandbox_override();
    let mut config = DuckAgentConfig::load_global()?;
    let mut sandbox = config.sandbox_config()?;
    sandbox.ensure_builtin_defaults();
    let preset_name = override_name.unwrap_or_else(|| sandbox.preset.clone());
    let preset = sandbox
        .presets
        .entry(preset_name.clone())
        .or_insert_with(|| builtin_preset_config(DEFAULT_PRESET_NAME));
    let permissions = preset
        .permissions
        .get_or_insert_with(PresetPermissions::default);
    permissions.shell.rules.insert(command.to_string(), action);
    sandbox.preset = preset_name;
    config.set_sandbox_config(sandbox)?;
    config.save_global()
}

pub fn append_shell_allow_to_current_preset(command: &str) -> Result<()> {
    append_shell_action_to_current_preset(command, PermissionAction::Allow)
}

pub fn append_tool_action_to_current_preset(tool: &str, action: PermissionAction) -> Result<()> {
    let tool = tool.trim();
    if tool.is_empty() {
        bail!("cannot persist an empty tool permission rule");
    }
    let override_name = cli_sandbox_override();
    let mut config = DuckAgentConfig::load_global()?;
    let mut sandbox = config.sandbox_config()?;
    sandbox.ensure_builtin_defaults();
    let preset_name = override_name.unwrap_or_else(|| sandbox.preset.clone());
    let preset = sandbox
        .presets
        .entry(preset_name.clone())
        .or_insert_with(|| builtin_preset_config(DEFAULT_PRESET_NAME));
    let permissions = preset
        .permissions
        .get_or_insert_with(PresetPermissions::default);
    permissions.tools.insert(tool.to_string(), action);
    sandbox.preset = preset_name;
    config.set_sandbox_config(sandbox)?;
    config.save_global()
}

pub fn append_env_action_to_current_preset(name: &str, action: PermissionAction) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        bail!("cannot persist an empty env permission rule");
    }
    let override_name = cli_sandbox_override();
    let mut config = DuckAgentConfig::load_global()?;
    let mut sandbox = config.sandbox_config()?;
    sandbox.ensure_builtin_defaults();
    let preset_name = override_name.unwrap_or_else(|| sandbox.preset.clone());
    let preset = sandbox
        .presets
        .entry(preset_name.clone())
        .or_insert_with(|| builtin_preset_config(DEFAULT_PRESET_NAME));
    let env = preset.env.get_or_insert_with(EnvConfig::default);
    env.0.insert(name.to_string(), EnvEntry::Action(action));
    sandbox.preset = preset_name;
    config.set_sandbox_config(sandbox)?;
    config.save_global()
}

pub fn append_network_host_action_to_current_preset(
    host: &str,
    action: PermissionAction,
) -> Result<()> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        bail!("cannot persist an empty network host rule");
    }
    let override_name = cli_sandbox_override();
    let mut config = DuckAgentConfig::load_global()?;
    let mut sandbox = config.sandbox_config()?;
    sandbox.ensure_builtin_defaults();
    let preset_name = override_name.unwrap_or_else(|| sandbox.preset.clone());
    let preset = sandbox
        .presets
        .entry(preset_name.clone())
        .or_insert_with(|| builtin_preset_config(DEFAULT_PRESET_NAME));
    let network = preset
        .network
        .get_or_insert_with(NetworkRulesConfig::default);
    network.hosts.insert(host, action);
    sandbox.preset = preset_name;
    config.set_sandbox_config(sandbox)?;
    config.save_global()
}

pub fn append_network_address_action_to_current_preset(
    address: &str,
    action: PermissionAction,
) -> Result<()> {
    let address = address.trim().trim_start_matches('[').trim_end_matches(']');
    if address.is_empty() {
        bail!("cannot persist an empty network address rule");
    }
    let parsed = address.parse::<IpAddr>().with_context(|| {
        format!("network address allow rule must be an IP literal: `{address}`")
    })?;
    let override_name = cli_sandbox_override();
    let mut config = DuckAgentConfig::load_global()?;
    let mut sandbox = config.sandbox_config()?;
    sandbox.ensure_builtin_defaults();
    let preset_name = override_name.unwrap_or_else(|| sandbox.preset.clone());
    let preset = sandbox
        .presets
        .entry(preset_name.clone())
        .or_insert_with(|| builtin_preset_config(DEFAULT_PRESET_NAME));
    let network = preset
        .network
        .get_or_insert_with(NetworkRulesConfig::default);
    network.addresses.insert(parsed.to_string(), action);
    sandbox.preset = preset_name;
    config.set_sandbox_config(sandbox)?;
    config.save_global()
}

pub fn append_filesystem_mount_to_current_preset(path: &str, access: FileAccess) -> Result<String> {
    let path = path.trim();
    if path.is_empty() {
        bail!("cannot persist an empty sandbox filesystem mount");
    }
    if matches!(access, FileAccess::None) {
        bail!("sandbox filesystem mounts only support `ro` or `rw`");
    }
    if path_is_core_protected(Path::new(path)) {
        bail!("sandbox policy does not allow granting `{path}` by request_filesystem_access");
    }

    let override_name = cli_sandbox_override();
    let mut config = DuckAgentConfig::load_global()?;
    let user_defined = user_defined_sandbox_presets(&config);
    let mut sandbox = config.sandbox_config()?;
    sandbox.ensure_builtin_defaults();

    let selected = override_name.unwrap_or_else(|| sandbox.preset.clone());
    let preset_name = append_filesystem_mount_to_sandbox_config(
        &mut sandbox,
        &user_defined,
        &selected,
        path,
        access,
    )?;

    config.set_sandbox_config(sandbox)?;
    config.save_global()?;
    Ok(preset_name)
}

fn append_filesystem_mount_to_sandbox_config(
    sandbox: &mut SandboxConfig,
    user_defined: &BTreeSet<String>,
    selected: &str,
    path: &str,
    access: FileAccess,
) -> Result<String> {
    let path = path.trim();
    if path.is_empty() {
        bail!("cannot persist an empty sandbox filesystem mount");
    }
    if matches!(access, FileAccess::None) {
        bail!("sandbox filesystem mounts only support `ro` or `rw`");
    }
    if path_is_core_protected(Path::new(path)) {
        bail!("sandbox policy does not allow granting `{path}` by request_filesystem_access");
    }

    let preset_name = writable_preset_name(selected, user_defined, &sandbox.presets);
    if !sandbox.presets.contains_key(&preset_name) {
        if preset_name == selected && !is_builtin_preset_name(selected) {
            bail!("sandbox preset `{selected}` not found");
        }
        sandbox.presets.insert(
            preset_name.clone(),
            SandboxPresetConfig {
                extends: Some(selected.to_string()),
                filesystem: Some(FileSystemConfig {
                    mounts: Vec::new(),
                    rules: Vec::new(),
                }),
                network: None,
                env: None,
                permissions: None,
                ..Default::default()
            },
        );
    }

    let preset = sandbox
        .presets
        .get_mut(&preset_name)
        .expect("sandbox preset was inserted above");
    let filesystem = preset.filesystem.get_or_insert_with(|| FileSystemConfig {
        mounts: Vec::new(),
        rules: Vec::new(),
    });
    upsert_mount(&mut filesystem.mounts, path, access);
    append_grant_rule_if_needed(filesystem, path, access);
    sandbox.preset = preset_name.clone();
    prune_generated_builtin_presets(sandbox, user_defined, &preset_name);
    Ok(preset_name)
}

pub fn grant_sandbox_access_once(path: PathBuf, access: FileAccess) {
    push_sandbox_access_grant(SandboxAccessGrant {
        path,
        access,
        once: true,
    });
}

pub fn grant_sandbox_access_session(path: PathBuf, access: FileAccess) {
    push_sandbox_access_grant(SandboxAccessGrant {
        path,
        access,
        once: false,
    });
}

pub fn consume_once_sandbox_access_grants() {
    let Some(slot) = SANDBOX_ACCESS_GRANTS.get() else {
        return;
    };
    let mut grants = slot.lock().expect("sandbox access grant mutex poisoned");
    grants.retain(|grant| !grant.once);
}

pub fn path_is_core_protected(path: &Path) -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let root = lexical_normalize(&home.join(".duckagent"));
    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
    let path_lexical = lexical_normalize(path);
    if path_lexical == root || path_lexical.starts_with(&root) {
        return true;
    }
    path.canonicalize()
        .map(|canonical| canonical == root_canonical || canonical.starts_with(root_canonical))
        .unwrap_or(false)
}

pub fn current_sandbox_summary() -> String {
    match resolve_sandbox() {
        Ok(sandbox) => {
            let mut lines = vec![
                format!("- active preset: `{}`", sandbox.name),
                "- protected user path: `~/.duckagent` cannot be read, copied, modified, or granted to Agents; the user must inspect or edit it manually outside duckagent".to_string(),
                "- filesystem denials use a unified `policy_blocked` result; do not infer special path classes from command text or process stderr".to_string(),
                "- if filesystem access is still needed, use `request_filesystem_access` only after the user confirms a concrete file or directory path".to_string(),
            ];
            if !sandbox.preset.filesystem.mounts.is_empty() {
                let mounts = sandbox
                    .preset
                    .filesystem
                    .mounts
                    .iter()
                    .map(|mount| format!("{}:{}", mount.path, file_access_label(mount.access)))
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(format!("- filesystem mounts: {mounts}"));
            }
            lines.join("\n")
        }
        Err(error) => format!("- failed to resolve sandbox: {error:#}"),
    }
}

fn push_sandbox_access_grant(grant: SandboxAccessGrant) {
    if path_is_core_protected(&grant.path) {
        return;
    }
    let mut grants = SANDBOX_ACCESS_GRANTS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("sandbox access grant mutex poisoned");
    if let Some(existing) = grants
        .iter_mut()
        .find(|existing| existing.path == grant.path && existing.once == grant.once)
    {
        if existing.access < grant.access {
            existing.access = grant.access;
        }
        return;
    }
    grants.push(grant);
}

fn apply_runtime_access_grants(resolved: &mut ResolvedSandbox) {
    let Some(slot) = SANDBOX_ACCESS_GRANTS.get() else {
        return;
    };
    let grants = slot.lock().expect("sandbox access grant mutex poisoned");
    for grant in grants.iter() {
        if path_is_core_protected(&grant.path) || matches!(grant.access, FileAccess::None) {
            continue;
        }
        upsert_mount(
            &mut resolved.preset.filesystem.mounts,
            &crate::sandbox::matcher::normalize_path_text(&grant.path),
            grant.access,
        );
    }
}

fn apply_core_filesystem_policy(resolved: &mut ResolvedSandbox) {
    for path in core_filesystem_paths() {
        if !resolved
            .preset
            .filesystem
            .rules
            .iter()
            .any(|rule| rule.path == path && rule.access == FileAccess::None)
        {
            resolved.preset.filesystem.rules.push(FileSystemRule {
                path,
                access: FileAccess::None,
            });
        }
    }
}

fn default_preset_name() -> String {
    DEFAULT_PRESET_NAME.to_string()
}

fn default_env_rules() -> BTreeMap<String, EnvEntry> {
    BTreeMap::from([("*".to_string(), EnvEntry::Action(PermissionAction::Allow))])
}

fn merge_permission_map(
    target: &mut BTreeMap<String, PermissionAction>,
    values: &BTreeMap<String, PermissionAction>,
) {
    for (key, action) in values {
        target.insert(key.clone(), action.clone());
    }
}

fn update_permission_match(
    best: &mut Option<(usize, PermissionAction)>,
    specificity: usize,
    action: PermissionAction,
) {
    match best {
        Some((best_specificity, best_action))
            if (*best_specificity, permission_action_rank(*best_action))
                >= (specificity, permission_action_rank(action)) => {}
        _ => *best = Some((specificity, action)),
    }
}

fn permission_action_rank(action: PermissionAction) -> usize {
    match action {
        PermissionAction::Deny => 3,
        PermissionAction::Ask => 2,
        PermissionAction::Allow => 1,
    }
}

fn host_rule_specificity(pattern: &str) -> usize {
    pattern
        .chars()
        .filter(|ch| !matches!(ch, '*' | '?' | '[' | ']' | '{' | '}' | ','))
        .count()
}

fn address_rule_specificity(pattern: &str, address: IpAddr) -> Option<usize> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return None;
    }
    if pattern == "*" {
        return Some(0);
    }
    if let Ok(net) = pattern.parse::<ipnet::IpNet>() {
        return net
            .contains(&address)
            .then_some(usize::from(net.prefix_len()));
    }
    pattern.parse::<IpAddr>().ok().and_then(|rule_address| {
        (rule_address == address).then_some(match rule_address {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        })
    })
}

fn validate_network_rule_patterns(preset_name: &str, rules: &NetworkRulesConfig) -> Result<()> {
    for host in rules.hosts.keys() {
        if host.trim().is_empty() {
            bail!("sandbox preset `{preset_name}` has an empty network.hosts rule");
        }
    }
    for address in rules.addresses.keys() {
        let address = address.trim();
        if address.is_empty() {
            bail!("sandbox preset `{preset_name}` has an empty network.addresses rule");
        }
        if address == "*" {
            continue;
        }
        if address.parse::<ipnet::IpNet>().is_ok() || address.parse::<IpAddr>().is_ok() {
            continue;
        }
        bail!(
            "sandbox preset `{preset_name}` has invalid network.addresses rule `{address}`; use `*`, an IP literal, or CIDR"
        );
    }
    Ok(())
}

fn validate_resolved_preset(preset_name: &str, preset: &SandboxPreset) -> Result<()> {
    if !preset.secrets.0.is_empty() && !matches!(preset.network.mode, NetworkMode::Proxy) {
        bail!(
            "sandbox preset `{preset_name}` uses env secret proxy entries, which require `network.mode = \"proxy\"`"
        );
    }
    Ok(())
}

fn validate_env_secret_entries(preset_name: &str, env: &EnvConfig) -> Result<()> {
    for (name, secret) in env.secret_entries() {
        if secret.entry_type != "secret" {
            bail!(
                "sandbox preset `{preset_name}` env `{name}` has unsupported type `{}`; only `secret` is supported",
                secret.entry_type
            );
        }
    }
    Ok(())
}

fn validate_required_secret_env(
    preset_name: &str,
    secret_name: &str,
    env_name: &str,
) -> Result<()> {
    let value = std::env::var(env_name).with_context(|| {
        format!(
            "sandbox preset `{preset_name}` env secret `{secret_name}` requires parent env `{env_name}`"
        )
    })?;
    if value.trim().is_empty() {
        bail!(
            "sandbox preset `{preset_name}` env secret `{secret_name}` requires non-empty parent env `{env_name}`"
        );
    }
    Ok(())
}

fn validate_required_secret_url_env(
    preset_name: &str,
    secret_name: &str,
    env_name: &str,
) -> Result<()> {
    let value = std::env::var(env_name).with_context(|| {
        format!(
            "sandbox preset `{preset_name}` env secret `{secret_name}` requires parent URL env `{env_name}`"
        )
    })?;
    parse_secret_upstream_url(&value).with_context(|| {
        format!(
            "sandbox preset `{preset_name}` env secret `{secret_name}` requires `{env_name}` to be a valid http(s) URL"
        )
    })?;
    Ok(())
}

fn parse_secret_upstream_url(value: &str) -> Result<url::Url> {
    let parsed = url::Url::parse(value.trim())?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        bail!("secret upstream URL must be an absolute http(s) URL");
    }
    Ok(parsed)
}

fn validate_env_name(preset_name: &str, field: &str, value: &str) -> Result<()> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        bail!("sandbox preset `{preset_name}` has an empty {field}");
    };
    if !(first == '_' || first.is_ascii_alphabetic())
        || !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
    {
        bail!(
            "sandbox preset `{preset_name}` has invalid {field} `{value}`; use an environment variable name"
        );
    }
    Ok(())
}

fn validate_secret_endpoint(
    preset_name: &str,
    secret_name: &str,
    field: &str,
    value: &str,
) -> Result<()> {
    if value.trim().is_empty() {
        bail!("sandbox preset `{preset_name}` secret `{secret_name}` has an empty `{field}`");
    }
    Ok(())
}

fn append_unique_mounts(target: &mut Vec<FileSystemMount>, values: &[FileSystemMount]) {
    for value in values {
        if !target.iter().any(|existing| existing == value) {
            target.push(value.clone());
        }
    }
}

fn append_unique_rules(target: &mut Vec<FileSystemRule>, values: &[FileSystemRule]) {
    for value in values {
        if !target.iter().any(|existing| existing == value) {
            target.push(value.clone());
        }
    }
}

fn upsert_mount(target: &mut Vec<FileSystemMount>, path: &str, access: FileAccess) {
    if let Some(existing) = target.iter_mut().find(|mount| mount.path == path) {
        if existing.access < access {
            existing.access = access;
        }
        return;
    }
    target.push(FileSystemMount {
        path: path.to_string(),
        access,
    });
    target.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.access.cmp(&right.access))
    });
}

fn append_grant_rule_if_needed(filesystem: &mut FileSystemConfig, path: &str, access: FileAccess) {
    if !matching_rule_requires_grant(&filesystem.rules, path, access) {
        return;
    }
    filesystem.rules.push(FileSystemRule {
        path: path.to_string(),
        access,
    });
}

fn matching_rule_requires_grant(
    rules: &[FileSystemRule],
    path: &str,
    requested: FileAccess,
) -> bool {
    let path = Path::new(path);
    let workspace = Path::new("/");
    let mut best: Option<(usize, usize, FileAccess)> = None;
    for (order, rule) in rules.iter().enumerate() {
        if path_pattern_matches(&rule.path, path, workspace) {
            update_rule_access_match(&mut best, rule_specificity(&rule.path), order, rule.access);
        }
    }
    let Some((_, _, effective)) = best else {
        return false;
    };
    effective < requested
}

fn update_rule_access_match(
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

fn rule_specificity(pattern: &str) -> usize {
    pattern
        .chars()
        .filter(|ch| !matches!(ch, '*' | '?' | '[' | ']' | '{' | '}' | ','))
        .count()
}

fn writable_preset_name(
    selected: &str,
    user_defined: &BTreeSet<String>,
    presets: &BTreeMap<String, SandboxPresetConfig>,
) -> String {
    if user_defined.contains(selected) || !is_builtin_preset_name(selected) {
        return selected.to_string();
    }
    let local = format!("{selected}{BUILTIN_CUSTOM_PRESET_SUFFIX}");
    if presets.contains_key(&local) {
        return local;
    }
    local
}

fn prune_generated_builtin_presets(
    sandbox: &mut SandboxConfig,
    user_defined: &BTreeSet<String>,
    required_preset: &str,
) {
    sandbox.presets.retain(|name, _| {
        user_defined.contains(name) || name == required_preset || !is_builtin_preset_name(name)
    });
}

fn user_defined_sandbox_presets(config: &DuckAgentConfig) -> BTreeSet<String> {
    config
        .raw()
        .get("sandbox")
        .and_then(|value| value.get("presets"))
        .and_then(|value| value.as_object())
        .map(|object| object.keys().cloned().collect())
        .unwrap_or_default()
}

fn is_builtin_preset_name(name: &str) -> bool {
    matches!(
        name,
        DEFAULT_PRESET_NAME | READONLY_PRESET_NAME | DANGER_PRESET_NAME
    )
}

fn core_filesystem_paths() -> Vec<String> {
    vec![
        "~/.duckagent".to_string(),
        "~/.duckagent/**".to_string(),
        "~/.duckagent/config.json".to_string(),
    ]
}

fn file_access_label(access: FileAccess) -> &'static str {
    match access {
        FileAccess::None => "none",
        FileAccess::Ro => "ro",
        FileAccess::Rw => "rw",
    }
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

fn ensure_builtin_preset(presets: &mut BTreeMap<String, SandboxPresetConfig>, name: &str) {
    presets
        .entry(name.to_string())
        .or_insert_with(|| builtin_preset_config(name));
}

fn insert_builtin_preset(presets: &mut BTreeMap<String, SandboxPresetConfig>, name: &str) {
    presets.insert(name.to_string(), builtin_preset_config(name));
}

fn builtin_preset_config(name: &str) -> SandboxPresetConfig {
    let json = match name {
        DEFAULT_PRESET_NAME => WORKSPACE_PRESET_JSON,
        READONLY_PRESET_NAME => READONLY_PRESET_JSON,
        DANGER_PRESET_NAME => DANGER_PRESET_JSON,
        _ => panic!("unknown built-in sandbox preset `{name}`"),
    };
    let mut file: BuiltinPresetFile = serde_json::from_str(json).unwrap_or_else(|error| {
        panic!("built-in sandbox preset `{name}` must be valid JSON: {error}")
    });
    if file.sandbox.preset != name {
        panic!("built-in sandbox preset `{name}` file must set sandbox.preset to `{name}`");
    }
    file.sandbox.presets.remove(name).unwrap_or_else(|| {
        panic!("built-in sandbox preset `{name}` file must contain sandbox.presets.{name}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    static ENV_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn sandbox_parse_error(value: Value) -> String {
        serde_json::from_value::<SandboxConfig>(value)
            .expect_err("sandbox config should be rejected")
            .to_string()
    }

    #[test]
    fn default_workspace_contains_proxy_and_sensitive_denies() -> Result<()> {
        let config = SandboxConfig::default();
        let workspace = config.resolve(Some("workspace"))?.preset;
        assert_eq!(workspace.network.mode, NetworkMode::Proxy);
        assert_eq!(
            workspace.network.hosts.get("*"),
            Some(&PermissionAction::Ask)
        );
        assert!(
            workspace
                .filesystem
                .rules
                .iter()
                .any(|rule| rule.path == ".env" && rule.access == FileAccess::None)
        );
        assert!(
            workspace
                .filesystem
                .rules
                .iter()
                .any(|rule| rule.path == ".git" && rule.access == FileAccess::Ro)
        );
        assert!(
            !workspace
                .filesystem
                .rules
                .iter()
                .any(|rule| rule.path.contains("auth.json") || rule.path.contains(".codex"))
        );
        assert_eq!(
            workspace.env.permission_rules().get("*"),
            Some(&PermissionAction::Allow)
        );
        Ok(())
    }

    #[test]
    fn core_policy_is_applied_after_preset_resolution() -> Result<()> {
        let mut resolved = SandboxConfig::default().resolve(Some("danger"))?;
        assert!(
            !resolved
                .preset
                .filesystem
                .rules
                .iter()
                .any(|rule| rule.path == "~/.duckagent/**")
        );

        apply_core_filesystem_policy(&mut resolved);
        assert!(
            resolved
                .preset
                .filesystem
                .rules
                .iter()
                .any(|rule| rule.path == "~/.duckagent/**" && rule.access == FileAccess::None)
        );
        Ok(())
    }

    #[test]
    fn active_sandbox_preset_switches_to_valid_preset_only() -> Result<()> {
        let mut sandbox = SandboxConfig::default();
        set_active_sandbox_preset_in_config(&mut sandbox, "danger")?;
        assert_eq!(sandbox.preset, "danger");
        assert!(set_active_sandbox_preset_in_config(&mut sandbox, "missing").is_err());
        assert_eq!(sandbox.preset, "danger");
        Ok(())
    }

    #[test]
    fn filesystem_grant_for_builtin_creates_workspace_custom() -> Result<()> {
        let mut sandbox = SandboxConfig::default();
        sandbox.ensure_builtin_defaults();
        let user_defined = BTreeSet::new();

        let preset = append_filesystem_mount_to_sandbox_config(
            &mut sandbox,
            &user_defined,
            "workspace",
            "/tmp/shared-docs",
            FileAccess::Ro,
        )?;

        assert_eq!(preset, "workspace-custom");
        assert_eq!(sandbox.preset, "workspace-custom");
        let custom = sandbox.presets.get("workspace-custom").unwrap();
        assert_eq!(custom.extends.as_deref(), Some("workspace"));
        assert!(
            custom
                .filesystem
                .as_ref()
                .unwrap()
                .mounts
                .iter()
                .any(|mount| mount.path == "/tmp/shared-docs" && mount.access == FileAccess::Ro)
        );
        assert!(!sandbox.presets.contains_key("workspace"));

        sandbox.ensure_builtin_defaults();
        let resolved = sandbox.resolve(Some("workspace-custom"))?.preset;
        assert!(
            resolved
                .filesystem
                .mounts
                .iter()
                .any(|mount| mount.path == "." && mount.access == FileAccess::Rw)
        );
        assert!(
            resolved
                .filesystem
                .mounts
                .iter()
                .any(|mount| mount.path == "/tmp/shared-docs" && mount.access == FileAccess::Ro)
        );
        Ok(())
    }

    #[test]
    fn filesystem_grant_reuses_existing_workspace_custom() -> Result<()> {
        let mut sandbox = SandboxConfig::default();
        sandbox.ensure_builtin_defaults();
        sandbox.presets.insert(
            "workspace-custom".to_string(),
            SandboxPresetConfig {
                extends: Some("workspace".to_string()),
                filesystem: Some(FileSystemConfig {
                    mounts: vec![FileSystemMount {
                        path: "/tmp/old".to_string(),
                        access: FileAccess::Ro,
                    }],
                    rules: Vec::new(),
                }),
                network: None,
                env: None,
                permissions: None,
                ..Default::default()
            },
        );
        let user_defined = BTreeSet::from(["workspace-custom".to_string()]);

        let preset = append_filesystem_mount_to_sandbox_config(
            &mut sandbox,
            &user_defined,
            "workspace",
            "/tmp/new",
            FileAccess::Rw,
        )?;

        assert_eq!(preset, "workspace-custom");
        let mounts = &sandbox
            .presets
            .get("workspace-custom")
            .unwrap()
            .filesystem
            .as_ref()
            .unwrap()
            .mounts;
        assert!(
            mounts
                .iter()
                .any(|mount| mount.path == "/tmp/old" && mount.access == FileAccess::Ro)
        );
        assert!(
            mounts
                .iter()
                .any(|mount| mount.path == "/tmp/new" && mount.access == FileAccess::Rw)
        );
        Ok(())
    }

    #[test]
    fn filesystem_grant_modifies_user_defined_builtin_directly() -> Result<()> {
        let mut sandbox = SandboxConfig::default();
        sandbox.ensure_builtin_defaults();
        let user_defined = BTreeSet::from(["workspace".to_string()]);

        let preset = append_filesystem_mount_to_sandbox_config(
            &mut sandbox,
            &user_defined,
            "workspace",
            "/tmp/user-owned",
            FileAccess::Ro,
        )?;

        assert_eq!(preset, "workspace");
        assert_eq!(sandbox.preset, "workspace");
        assert!(
            sandbox
                .presets
                .get("workspace")
                .unwrap()
                .filesystem
                .as_ref()
                .unwrap()
                .mounts
                .iter()
                .any(|mount| mount.path == "/tmp/user-owned" && mount.access == FileAccess::Ro)
        );
        Ok(())
    }

    #[test]
    fn filesystem_grant_updates_existing_custom_preset_directly() -> Result<()> {
        let mut sandbox = SandboxConfig::default();
        sandbox.presets.insert(
            "strict".to_string(),
            SandboxPresetConfig {
                filesystem: Some(FileSystemConfig {
                    mounts: vec![FileSystemMount {
                        path: ".".to_string(),
                        access: FileAccess::Ro,
                    }],
                    rules: Vec::new(),
                }),
                network: Some(NetworkRulesConfig {
                    mode: Some(NetworkMode::Deny),
                    hosts: BTreeMap::from([("*".to_string(), PermissionAction::Deny)]),
                    addresses: BTreeMap::new(),
                }),
                ..Default::default()
            },
        );
        let user_defined = BTreeSet::from(["strict".to_string()]);

        let preset = append_filesystem_mount_to_sandbox_config(
            &mut sandbox,
            &user_defined,
            "strict",
            "/tmp/strict-extra",
            FileAccess::Ro,
        )?;

        assert_eq!(preset, "strict");
        assert!(
            sandbox
                .presets
                .get("strict")
                .unwrap()
                .filesystem
                .as_ref()
                .unwrap()
                .mounts
                .iter()
                .any(|mount| mount.path == "/tmp/strict-extra")
        );
        Ok(())
    }

    #[test]
    fn filesystem_grant_creates_filesystem_block_when_preset_has_none() -> Result<()> {
        let mut sandbox = SandboxConfig::default();
        sandbox.presets.insert(
            "child".to_string(),
            SandboxPresetConfig {
                extends: Some("workspace".to_string()),
                filesystem: None,
                network: None,
                env: None,
                permissions: None,
            },
        );
        let user_defined = BTreeSet::from(["child".to_string()]);

        let preset = append_filesystem_mount_to_sandbox_config(
            &mut sandbox,
            &user_defined,
            "child",
            "/tmp/no-filesystem-block",
            FileAccess::Ro,
        )?;

        assert_eq!(preset, "child");
        let filesystem = sandbox
            .presets
            .get("child")
            .unwrap()
            .filesystem
            .as_ref()
            .expect("grant should create filesystem block");
        assert_eq!(filesystem.rules.len(), 0);
        assert!(filesystem.mounts.iter().any(|mount| {
            mount.path == "/tmp/no-filesystem-block" && mount.access == FileAccess::Ro
        }));
        Ok(())
    }

    #[test]
    fn filesystem_grant_with_empty_rules_adds_mount_only() -> Result<()> {
        let mut sandbox = SandboxConfig::default();
        sandbox.presets.insert(
            "strict".to_string(),
            SandboxPresetConfig {
                filesystem: Some(FileSystemConfig {
                    mounts: Vec::new(),
                    rules: Vec::new(),
                }),
                network: Some(NetworkRulesConfig {
                    mode: Some(NetworkMode::Deny),
                    hosts: BTreeMap::from([("*".to_string(), PermissionAction::Deny)]),
                    addresses: BTreeMap::new(),
                }),
                ..Default::default()
            },
        );
        let user_defined = BTreeSet::from(["strict".to_string()]);

        append_filesystem_mount_to_sandbox_config(
            &mut sandbox,
            &user_defined,
            "strict",
            "/tmp/mount-only",
            FileAccess::Rw,
        )?;

        let filesystem = sandbox
            .presets
            .get("strict")
            .unwrap()
            .filesystem
            .as_ref()
            .unwrap();
        assert!(
            filesystem
                .mounts
                .iter()
                .any(|mount| mount.path == "/tmp/mount-only" && mount.access == FileAccess::Rw)
        );
        assert!(
            filesystem.rules.is_empty(),
            "no grant rule is needed when no existing rule blocks the path"
        );
        Ok(())
    }

    #[test]
    fn filesystem_grant_rejects_missing_custom_preset() {
        let mut sandbox = SandboxConfig::default();
        let err = append_filesystem_mount_to_sandbox_config(
            &mut sandbox,
            &BTreeSet::new(),
            "missing-custom",
            "/tmp/x",
            FileAccess::Ro,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("sandbox preset `missing-custom` not found"));
    }

    #[test]
    fn filesystem_grant_rejects_core_protected_path() {
        let mut sandbox = SandboxConfig::default();
        let home = dirs::home_dir().expect("home dir required for test");
        let err = append_filesystem_mount_to_sandbox_config(
            &mut sandbox,
            &BTreeSet::new(),
            "workspace",
            &home.join(".duckagent/config.json").to_string_lossy(),
            FileAccess::Rw,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("sandbox policy does not allow granting"));
    }

    #[test]
    fn sandbox_summary_declares_protected_user_path() {
        let summary = current_sandbox_summary();
        assert!(summary.contains("protected user path: `~/.duckagent`"));
        assert!(summary.contains("cannot be read, copied, modified, or granted to Agents"));
    }

    #[test]
    fn filesystem_grant_preserves_parent_none_rule_and_adds_child_ro_grant() -> Result<()> {
        let mut sandbox = sandbox_with_strict_rules(vec![FileSystemRule {
            path: "/foo".to_string(),
            access: FileAccess::None,
        }]);
        let user_defined = BTreeSet::from(["strict".to_string()]);

        let preset = append_filesystem_mount_to_sandbox_config(
            &mut sandbox,
            &user_defined,
            "strict",
            "/foo/test/1.md",
            FileAccess::Ro,
        )?;

        assert_eq!(preset, "strict");
        let strict = sandbox.presets.get("strict").unwrap();
        let filesystem = strict.filesystem.as_ref().unwrap();
        assert_eq!(filesystem.rules[0].path, "/foo");
        assert_eq!(filesystem.rules[0].access, FileAccess::None);
        assert_eq!(filesystem.rules.last().unwrap().path, "/foo/test/1.md");
        assert_eq!(filesystem.rules.last().unwrap().access, FileAccess::Ro);

        let resolved = sandbox.resolve(Some("strict"))?;
        assert_eq!(
            crate::sandbox::permissions::effective_file_access(
                &resolved,
                Path::new("/foo/test/1.md"),
                Path::new("/")
            ),
            FileAccess::Ro
        );
        assert_eq!(
            crate::sandbox::permissions::effective_file_access(
                &resolved,
                Path::new("/foo/other.md"),
                Path::new("/")
            ),
            FileAccess::None
        );
        Ok(())
    }

    #[test]
    fn filesystem_grant_appends_later_ro_rule_for_exact_none_conflict() -> Result<()> {
        let mut sandbox = sandbox_with_strict_rules(vec![FileSystemRule {
            path: "/foo/test/1.md".to_string(),
            access: FileAccess::None,
        }]);
        let user_defined = BTreeSet::from(["strict".to_string()]);

        append_filesystem_mount_to_sandbox_config(
            &mut sandbox,
            &user_defined,
            "strict",
            "/foo/test/1.md",
            FileAccess::Ro,
        )?;

        let rules = &sandbox
            .presets
            .get("strict")
            .unwrap()
            .filesystem
            .as_ref()
            .unwrap()
            .rules;
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].access, FileAccess::None);
        assert_eq!(rules[1].access, FileAccess::Ro);

        let resolved = sandbox.resolve(Some("strict"))?;
        assert_eq!(
            crate::sandbox::permissions::effective_file_access(
                &resolved,
                Path::new("/foo/test/1.md"),
                Path::new("/")
            ),
            FileAccess::Ro
        );
        Ok(())
    }

    #[test]
    fn filesystem_grant_appends_rw_rule_when_existing_rule_is_ro() -> Result<()> {
        let mut sandbox = sandbox_with_strict_rules(vec![FileSystemRule {
            path: "/foo/test".to_string(),
            access: FileAccess::Ro,
        }]);
        let user_defined = BTreeSet::from(["strict".to_string()]);

        append_filesystem_mount_to_sandbox_config(
            &mut sandbox,
            &user_defined,
            "strict",
            "/foo/test",
            FileAccess::Rw,
        )?;

        let resolved = sandbox.resolve(Some("strict"))?;
        assert_eq!(
            crate::sandbox::permissions::effective_file_access(
                &resolved,
                Path::new("/foo/test/1.md"),
                Path::new("/")
            ),
            FileAccess::Rw
        );
        Ok(())
    }

    #[test]
    fn filesystem_grant_does_not_downgrade_existing_rw_mount_or_rule() -> Result<()> {
        let mut sandbox = sandbox_with_strict_rules(vec![FileSystemRule {
            path: "/foo".to_string(),
            access: FileAccess::Rw,
        }]);
        sandbox
            .presets
            .get_mut("strict")
            .unwrap()
            .filesystem
            .as_mut()
            .unwrap()
            .mounts = vec![FileSystemMount {
            path: "/foo".to_string(),
            access: FileAccess::Rw,
        }];
        let user_defined = BTreeSet::from(["strict".to_string()]);

        append_filesystem_mount_to_sandbox_config(
            &mut sandbox,
            &user_defined,
            "strict",
            "/foo",
            FileAccess::Ro,
        )?;

        let filesystem = sandbox
            .presets
            .get("strict")
            .unwrap()
            .filesystem
            .as_ref()
            .unwrap();
        assert_eq!(filesystem.rules.len(), 1);
        assert_eq!(filesystem.rules[0].access, FileAccess::Rw);
        assert_eq!(filesystem.mounts[0].access, FileAccess::Rw);

        let resolved = sandbox.resolve(Some("strict"))?;
        assert_eq!(
            crate::sandbox::permissions::effective_file_access(
                &resolved,
                Path::new("/foo/a.md"),
                Path::new("/")
            ),
            FileAccess::Rw
        );
        Ok(())
    }

    #[test]
    fn upsert_mount_upgrades_but_does_not_downgrade_access() {
        let mut mounts = vec![FileSystemMount {
            path: "/tmp/project".to_string(),
            access: FileAccess::Ro,
        }];
        upsert_mount(&mut mounts, "/tmp/project", FileAccess::Rw);
        assert_eq!(mounts[0].access, FileAccess::Rw);

        upsert_mount(&mut mounts, "/tmp/project", FileAccess::Ro);
        assert_eq!(mounts[0].access, FileAccess::Rw);
    }

    fn sandbox_with_strict_rules(rules: Vec<FileSystemRule>) -> SandboxConfig {
        let mut sandbox = SandboxConfig::default();
        sandbox.preset = "strict".to_string();
        sandbox.presets.insert(
            "strict".to_string(),
            SandboxPresetConfig {
                filesystem: Some(FileSystemConfig {
                    mounts: vec![FileSystemMount {
                        path: "/foo".to_string(),
                        access: FileAccess::Rw,
                    }],
                    rules,
                }),
                network: Some(NetworkRulesConfig {
                    mode: Some(NetworkMode::Deny),
                    hosts: BTreeMap::from([("*".to_string(), PermissionAction::Deny)]),
                    addresses: BTreeMap::new(),
                }),
                ..Default::default()
            },
        );
        sandbox
    }

    #[test]
    fn builtin_preset_json_files_resolve() -> Result<()> {
        let config = SandboxConfig::default();
        let workspace = config.resolve(Some("workspace"))?.preset;
        let readonly = config.resolve(Some("readonly"))?.preset;
        let danger = config.resolve(Some("danger"))?.preset;

        assert_eq!(workspace.network.mode, NetworkMode::Proxy);
        assert!(
            workspace
                .filesystem
                .mounts
                .iter()
                .any(|mount| mount.path == "$TMPDIR" && mount.access == FileAccess::Rw)
        );
        assert_eq!(readonly.network.mode, NetworkMode::Deny);
        assert!(
            readonly
                .filesystem
                .mounts
                .iter()
                .all(|mount| mount.access == FileAccess::Ro)
        );
        assert!(
            readonly
                .filesystem
                .mounts
                .iter()
                .any(|mount| mount.path == "$TMPDIR" && mount.access == FileAccess::Ro)
        );
        assert_eq!(danger.network.mode, NetworkMode::Allow);
        assert!(
            danger
                .filesystem
                .mounts
                .iter()
                .any(|mount| mount.path == "*" && mount.access == FileAccess::Rw)
        );
        Ok(())
    }

    #[test]
    fn builtin_preset_json_files_are_copyable_config_fragments() -> Result<()> {
        for (name, json) in [
            ("workspace", WORKSPACE_PRESET_JSON),
            ("readonly", READONLY_PRESET_JSON),
            ("danger", DANGER_PRESET_JSON),
        ] {
            let mut file: BuiltinPresetFile = serde_json::from_str(json)?;
            file.sandbox.ensure_builtin_defaults();
            let resolved = file.sandbox.resolve(None)?;
            assert_eq!(resolved.name, name);
        }
        Ok(())
    }

    #[test]
    fn parses_config_shape() -> Result<()> {
        let value = json!({
            "preset": "workspace",
            "presets": {
                "workspace": {
                    "filesystem": {
                        "mounts": [
                            {"path": ".", "access": "rw"},
                            {"path": "$TMPDIR", "access": "rw"}
                        ],
                        "rules": [
                            {"path": "**/.env", "access": "none"},
                            {"path": "**/.git/**", "access": "ro"}
                        ]
                    },
                    "network": {
                        "mode": "deny",
                        "hosts": {"*": "ask", "localhost": "allow"}
                    },
                    "env": {
                        "CUSTOM_*": "allow",
                        "SECRET_*": "deny"
                    },
                    "permissions": {
                        "tools": {"context7_*": "ask"},
                        "shell": {"ls": "allow", "git push": "deny"}
                    }
                }
            }
        });
        let parsed: SandboxConfig = serde_json::from_value(value)?;
        let resolved = parsed.resolve(None)?.preset;
        assert_eq!(parsed.preset, "workspace");
        assert_eq!(resolved.network.mode, NetworkMode::Deny);
        assert_eq!(resolved.network.hosts["*"], PermissionAction::Ask);
        assert_eq!(resolved.filesystem.mounts.len(), 2);
        assert_eq!(
            resolved.env.permission_rules()["CUSTOM_*"],
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.env.permission_rules()["SECRET_*"],
            PermissionAction::Deny
        );
        assert_eq!(
            resolved.permissions.tools["context7_*"],
            PermissionAction::Ask
        );
        assert_eq!(
            resolved.permissions.shell.rules["git push"],
            PermissionAction::Deny
        );
        Ok(())
    }

    #[test]
    fn old_env_array_shape_is_rejected() {
        let value = json!({
            "preset": "workspace",
            "presets": {
                "workspace": {
                    "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                    "network": {"mode": "deny", "hosts": {"*": "deny"}},
                    "env": {
                        "allow": ["*"],
                        "deny": []
                    }
                }
            }
        });

        assert!(serde_json::from_value::<SandboxConfig>(value).is_err());
    }

    #[test]
    fn strict_schema_rejects_unknown_top_level_fields_with_english_error() {
        let error = sandbox_parse_error(json!({
            "preset": "workspace",
            "ipc": {
                "unix_sockets": {
                    "/var/run/docker.sock": "deny"
                }
            }
        }));

        assert!(error.contains("unknown field"));
        assert!(error.contains("ipc"));
        assert!(error.contains("preset"));
        assert!(error.contains("presets"));
    }

    #[test]
    fn strict_schema_rejects_unknown_filesystem_fields_with_english_error() {
        let error = sandbox_parse_error(json!({
            "preset": "custom",
            "presets": {
                "custom": {
                    "filesystem": {
                        "paths": {
                            "*": "ro"
                        },
                        "mounts": [{"path": ".", "access": "rw"}],
                        "rules": []
                    },
                    "network": {"mode": "deny", "hosts": {"*": "deny"}}
                }
            }
        }));

        assert!(error.contains("unknown field"));
        assert!(error.contains("paths"));
        assert!(error.contains("mounts"));
        assert!(error.contains("rules"));
    }

    #[test]
    fn strict_schema_rejects_unknown_mount_rule_and_permission_fields() {
        for (value, unknown, expected) in [
            (
                json!({
                    "preset": "custom",
                    "presets": {
                        "custom": {
                            "filesystem": {
                                "mounts": [{"path": ".", "access": "rw", "recursive": true}],
                                "rules": []
                            },
                            "network": {"mode": "deny", "hosts": {"*": "deny"}}
                        }
                    }
                }),
                "recursive",
                "access",
            ),
            (
                json!({
                    "preset": "custom",
                    "presets": {
                        "custom": {
                            "filesystem": {
                                "mounts": [{"path": ".", "access": "rw"}],
                                "rules": [{"path": "**/.env", "access": "none", "reason": "secret"}]
                            },
                            "network": {"mode": "deny", "hosts": {"*": "deny"}}
                        }
                    }
                }),
                "reason",
                "path",
            ),
            (
                json!({
                    "preset": "custom",
                    "presets": {
                        "custom": {
                            "filesystem": {
                                "mounts": [{"path": ".", "access": "rw"}],
                                "rules": []
                            },
                            "network": {"mode": "deny", "hosts": {"*": "deny"}},
                            "permissions": {
                                "toolz": {"read_file": "allow"}
                            }
                        }
                    }
                }),
                "toolz",
                "tools",
            ),
        ] {
            let error = sandbox_parse_error(value);
            assert!(error.contains("unknown field"), "{error}");
            assert!(error.contains(unknown), "{error}");
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn old_shell_array_shape_is_rejected() {
        let value = json!({
            "preset": "workspace",
            "presets": {
                "workspace": {
                    "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                    "network": {"mode": "deny", "hosts": {"*": "deny"}},
                    "permissions": {
                        "shell": {
                            "allow": ["ls*"],
                            "ask": [],
                            "deny": ["git push*"]
                        }
                    }
                }
            }
        });

        assert!(serde_json::from_value::<SandboxConfig>(value).is_err());
    }

    #[test]
    fn preset_name_only_config_can_use_builtin_presets() -> Result<()> {
        let value = json!({"preset": "readonly"});
        let mut parsed: SandboxConfig = serde_json::from_value(value)?;
        parsed.ensure_builtin_defaults();
        let resolved = parsed.resolve(None)?;

        assert_eq!(resolved.name, "readonly");
        assert_eq!(resolved.preset.network.mode, NetworkMode::Deny);
        assert!(
            resolved
                .preset
                .filesystem
                .mounts
                .iter()
                .all(|mount| mount.access == FileAccess::Ro)
        );
        Ok(())
    }

    #[test]
    fn user_defined_builtin_name_is_not_overwritten() -> Result<()> {
        let value = json!({
            "preset": "workspace",
            "presets": {
                "workspace": {
                    "filesystem": {
                        "mounts": [{"path": "custom", "access": "ro"}],
                        "rules": []
                    },
                    "network": {"mode": "deny", "hosts": {"*": "deny"}}
                }
            }
        });
        let mut parsed: SandboxConfig = serde_json::from_value(value)?;
        parsed.ensure_builtin_defaults();
        let resolved = parsed.resolve(None)?.preset;

        assert_eq!(resolved.filesystem.mounts[0].path, "custom");
        assert_eq!(resolved.filesystem.mounts[0].access, FileAccess::Ro);
        assert_eq!(resolved.network.mode, NetworkMode::Deny);
        Ok(())
    }

    #[test]
    fn extends_builtin_preset_adds_rules_without_losing_base() -> Result<()> {
        let value = json!({
            "preset": "workspace-plus-docs",
            "presets": {
                "workspace-plus-docs": {
                    "extends": "workspace",
                    "filesystem": {
                        "mounts": [{"path": "/Users/tt/Documents", "access": "ro"}],
                        "rules": [{"path": "**/secrets/**", "access": "none"}]
                    },
                    "network": {
                        "hosts": {"docs.example.com": "allow"}
                    },
                    "env": {
                        "DOCS_TOKEN": "allow",
                        "DOCS_SECRET": "deny"
                    },
                    "permissions": {
                        "shell": {"npm install": "ask"}
                    }
                }
            }
        });
        let mut parsed: SandboxConfig = serde_json::from_value(value)?;
        parsed.ensure_builtin_defaults();
        let resolved = parsed.resolve(None)?.preset;

        assert!(
            resolved
                .filesystem
                .mounts
                .iter()
                .any(|mount| mount.path == "." && mount.access == FileAccess::Rw)
        );
        assert!(
            resolved
                .filesystem
                .mounts
                .iter()
                .any(|mount| mount.path == "/Users/tt/Documents" && mount.access == FileAccess::Ro)
        );
        assert!(
            resolved
                .filesystem
                .rules
                .iter()
                .any(|rule| rule.path == ".env" && rule.access == FileAccess::None)
        );
        assert!(
            resolved
                .filesystem
                .rules
                .iter()
                .any(|rule| rule.path == "**/secrets/**" && rule.access == FileAccess::None)
        );
        assert_eq!(
            resolved.network.hosts["docs.example.com"],
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.env.permission_rules()["DOCS_TOKEN"],
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.env.permission_rules()["DOCS_SECRET"],
            PermissionAction::Deny
        );
        assert_eq!(
            resolved.permissions.shell.rules["npm install"],
            PermissionAction::Ask
        );
        Ok(())
    }

    #[test]
    fn extends_child_permission_maps_override_exact_parent_keys() -> Result<()> {
        let value = json!({
            "preset": "child",
            "presets": {
                "base": {
                    "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                    "network": {"mode": "deny", "hosts": {"*": "deny"}},
                    "env": {
                        "*": "allow",
                        "SECRET_*": "deny"
                    },
                    "permissions": {
                        "tools": {"github_*": "ask"},
                        "shell": {"git push": "ask"}
                    }
                },
                "child": {
                    "extends": "base",
                    "env": {
                        "SECRET_*": "ask"
                    },
                    "permissions": {
                        "tools": {"github_*": "allow"},
                        "shell": {"git push": "deny"}
                    }
                }
            }
        });

        let resolved = serde_json::from_value::<SandboxConfig>(value)?
            .resolve(None)?
            .preset;
        assert_eq!(
            resolved.env.permission_rules()["SECRET_*"],
            PermissionAction::Ask
        );
        assert_eq!(
            resolved.permissions.tools["github_*"],
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.permissions.shell.rules["git push"],
            PermissionAction::Deny
        );
        Ok(())
    }

    #[test]
    fn extends_child_omitted_env_inherits_parent_env() -> Result<()> {
        let value = json!({
            "preset": "child",
            "presets": {
                "base": {
                    "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                    "network": {"mode": "deny", "hosts": {"*": "deny"}},
                    "env": {
                        "*": "deny",
                        "PATH": "allow",
                        "AWS_*": "ask"
                    }
                },
                "child": {
                    "extends": "base",
                    "permissions": {
                        "shell": {"cargo test": "ask"}
                    }
                }
            }
        });

        let resolved = serde_json::from_value::<SandboxConfig>(value)?
            .resolve(None)?
            .preset;
        assert_eq!(resolved.env.permission_rules()["*"], PermissionAction::Deny);
        assert_eq!(
            resolved.env.permission_rules()["PATH"],
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.env.permission_rules()["AWS_*"],
            PermissionAction::Ask
        );
        Ok(())
    }

    #[test]
    fn extends_child_env_adds_keys_without_losing_parent_env() -> Result<()> {
        let value = json!({
            "preset": "child",
            "presets": {
                "base": {
                    "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                    "network": {"mode": "deny", "hosts": {"*": "deny"}},
                    "env": {
                        "*": "allow",
                        "SECRET_*": "deny"
                    }
                },
                "child": {
                    "extends": "base",
                    "env": {
                        "CI": "allow",
                        "AWS_*": "ask"
                    }
                }
            }
        });

        let resolved = serde_json::from_value::<SandboxConfig>(value)?
            .resolve(None)?
            .preset;
        assert_eq!(
            resolved.env.permission_rules()["*"],
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.env.permission_rules()["SECRET_*"],
            PermissionAction::Deny
        );
        assert_eq!(
            resolved.env.permission_rules()["CI"],
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.env.permission_rules()["AWS_*"],
            PermissionAction::Ask
        );
        Ok(())
    }

    #[test]
    fn extends_child_omitted_permissions_inherits_parent_permissions() -> Result<()> {
        let value = json!({
            "preset": "child",
            "presets": {
                "base": {
                    "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                    "network": {"mode": "deny", "hosts": {"*": "deny"}},
                    "permissions": {
                        "tools": {
                            "github_*": "ask",
                            "context7_*": "allow"
                        },
                        "shell": {
                            "git status": "allow",
                            "git push": "ask"
                        }
                    }
                },
                "child": {
                    "extends": "base",
                    "env": {
                        "CI": "allow"
                    }
                }
            }
        });

        let resolved = serde_json::from_value::<SandboxConfig>(value)?
            .resolve(None)?
            .preset;
        assert_eq!(
            resolved.permissions.tools["github_*"],
            PermissionAction::Ask
        );
        assert_eq!(
            resolved.permissions.tools["context7_*"],
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.permissions.shell.rules["git status"],
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.permissions.shell.rules["git push"],
            PermissionAction::Ask
        );
        Ok(())
    }

    #[test]
    fn extends_child_tool_permissions_add_without_losing_parent_tools() -> Result<()> {
        let value = json!({
            "preset": "child",
            "presets": {
                "base": {
                    "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                    "network": {"mode": "deny", "hosts": {"*": "deny"}},
                    "permissions": {
                        "tools": {
                            "github_*": "ask",
                            "context7_*": "allow"
                        }
                    }
                },
                "child": {
                    "extends": "base",
                    "permissions": {
                        "tools": {
                            "dangerous_*": "deny"
                        }
                    }
                }
            }
        });

        let resolved = serde_json::from_value::<SandboxConfig>(value)?
            .resolve(None)?
            .preset;
        assert_eq!(
            resolved.permissions.tools["github_*"],
            PermissionAction::Ask
        );
        assert_eq!(
            resolved.permissions.tools["context7_*"],
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.permissions.tools["dangerous_*"],
            PermissionAction::Deny
        );
        Ok(())
    }

    #[test]
    fn extends_child_shell_permissions_add_without_losing_parent_shell() -> Result<()> {
        let value = json!({
            "preset": "child",
            "presets": {
                "base": {
                    "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                    "network": {"mode": "deny", "hosts": {"*": "deny"}},
                    "permissions": {
                        "shell": {
                            "git status": "allow",
                            "git push": "ask"
                        }
                    }
                },
                "child": {
                    "extends": "base",
                    "permissions": {
                        "shell": {
                            "cargo test": "ask",
                            "rm -rf": "deny"
                        }
                    }
                }
            }
        });

        let resolved = serde_json::from_value::<SandboxConfig>(value)?
            .resolve(None)?
            .preset;
        assert_eq!(
            resolved.permissions.shell.rules["git status"],
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.permissions.shell.rules["git push"],
            PermissionAction::Ask
        );
        assert_eq!(
            resolved.permissions.shell.rules["cargo test"],
            PermissionAction::Ask
        );
        assert_eq!(
            resolved.permissions.shell.rules["rm -rf"],
            PermissionAction::Deny
        );
        Ok(())
    }

    #[test]
    fn extends_child_network_hosts_override_and_add_without_losing_parent_hosts() -> Result<()> {
        let value = json!({
            "preset": "child",
            "presets": {
                "base": {
                    "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                    "network": {
                        "mode": "proxy",
                        "hosts": {
                            "*": "ask",
                            "api.example.com": "ask",
                            "docs.example.com": "allow"
                        }
                    }
                },
                "child": {
                    "extends": "base",
                    "network": {
                        "hosts": {
                            "api.example.com": "allow",
                            "evil.example.com": "deny"
                        }
                    }
                }
            }
        });

        let resolved = serde_json::from_value::<SandboxConfig>(value)?
            .resolve(None)?
            .preset;
        assert_eq!(resolved.network.mode, NetworkMode::Proxy);
        assert_eq!(resolved.network.hosts["*"], PermissionAction::Ask);
        assert_eq!(
            resolved.network.hosts["api.example.com"],
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.network.hosts["docs.example.com"],
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.network.hosts["evil.example.com"],
            PermissionAction::Deny
        );
        Ok(())
    }

    #[test]
    fn extends_custom_preset_chains_cleanly() -> Result<()> {
        let value = json!({
            "preset": "custom-2",
            "presets": {
                "custom-1": {
                    "filesystem": {
                        "mounts": [{"path": ".", "access": "ro"}, {"path": "$TMPDIR", "access": "rw"}],
                        "rules": [{"path": "**/private/**", "access": "none"}]
                    },
                    "network": {"mode": "deny", "hosts": {"*": "deny"}}
                },
                "custom-2": {
                    "extends": "custom-1",
                    "filesystem": {
                        "mounts": [{"path": "build", "access": "rw"}],
                        "rules": [{"path": "**/generated-secret/**", "access": "none"}]
                    },
                    "network": {"mode": "proxy", "hosts": {"*": "ask", "localhost": "allow"}}
                }
            }
        });
        let parsed: SandboxConfig = serde_json::from_value(value)?;
        let resolved = parsed.resolve(None)?.preset;

        assert_eq!(resolved.network.mode, NetworkMode::Proxy);
        assert_eq!(resolved.network.hosts["*"], PermissionAction::Ask);
        assert_eq!(resolved.network.hosts["localhost"], PermissionAction::Allow);
        assert!(
            resolved
                .filesystem
                .mounts
                .iter()
                .any(|mount| mount.path == "$TMPDIR" && mount.access == FileAccess::Rw)
        );
        assert!(
            resolved
                .filesystem
                .mounts
                .iter()
                .any(|mount| mount.path == "build" && mount.access == FileAccess::Rw)
        );
        assert!(
            resolved.filesystem.rules.iter().any(
                |rule| rule.path == "**/generated-secret/**" && rule.access == FileAccess::None
            )
        );
        Ok(())
    }

    #[test]
    fn full_custom_preset_requires_core_fields() {
        let value = json!({
            "preset": "broken",
            "presets": {
                "broken": {
                    "filesystem": {
                        "mounts": [{"path": ".", "access": "rw"}],
                        "rules": []
                    }
                }
            }
        });
        let parsed: SandboxConfig = serde_json::from_value(value).unwrap();
        let error = parsed.resolve(None).unwrap_err().to_string();
        assert!(error.contains("missing `network`"));
    }

    #[test]
    fn circular_extends_is_rejected() {
        let value = json!({
            "preset": "a",
            "presets": {
                "a": {"extends": "b"},
                "b": {"extends": "a"}
            }
        });
        let parsed: SandboxConfig = serde_json::from_value(value).unwrap();
        let error = parsed.resolve(None).unwrap_err().to_string();
        assert!(error.contains("circular extends chain"));
    }

    #[test]
    fn network_rules_fill_wildcard_from_mode_when_missing() -> Result<()> {
        let value = json!({
            "preset": "custom",
            "presets": {
                "custom": {
                    "filesystem": {
                        "mounts": [{"path": ".", "access": "rw"}],
                        "rules": []
                    },
                    "network": {"mode": "allow", "hosts": {"api.example.com": "deny"}}
                }
            }
        });
        let parsed: SandboxConfig = serde_json::from_value(value)?;
        let resolved = parsed.resolve(None)?.preset;

        assert_eq!(resolved.network.hosts["*"], PermissionAction::Allow);
        assert_eq!(
            resolved.network.hosts["api.example.com"],
            PermissionAction::Deny
        );
        Ok(())
    }

    #[test]
    fn network_host_rules_support_case_insensitive_globs() -> Result<()> {
        let value = json!({
            "preset": "custom",
            "presets": {
                "custom": {
                    "filesystem": {
                        "mounts": [{"path": ".", "access": "rw"}],
                        "rules": []
                    },
                    "network": {
                        "mode": "proxy",
                        "hosts": {"*": "deny", "*.example.com": "allow"}
                    }
                }
            }
        });
        let parsed: SandboxConfig = serde_json::from_value(value)?;
        let resolved = parsed.resolve(None)?.preset;

        assert_eq!(
            resolved.network.action_for_host("Docs.Example.Com"),
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.network.action_for_host("other.invalid"),
            PermissionAction::Deny
        );
        Ok(())
    }

    #[test]
    fn network_host_rules_prefer_specific_deny_over_wide_allow() -> Result<()> {
        let value = json!({
            "preset": "custom",
            "presets": {
                "custom": {
                    "filesystem": {
                        "mounts": [{"path": ".", "access": "rw"}],
                        "rules": []
                    },
                    "network": {
                        "mode": "proxy",
                        "hosts": {
                            "*": "ask",
                            "*.example.com": "allow",
                            "blocked.example.com": "deny"
                        }
                    }
                }
            }
        });
        let parsed: SandboxConfig = serde_json::from_value(value)?;
        let resolved = parsed.resolve(None)?.preset;

        assert_eq!(
            resolved.network.action_for_host("docs.example.com"),
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.network.action_for_host("blocked.example.com"),
            PermissionAction::Deny
        );
        assert_eq!(
            resolved.network.action_for_host("unknown.invalid"),
            PermissionAction::Ask
        );
        Ok(())
    }

    #[test]
    fn old_network_domains_shape_is_rejected() {
        let value = json!({
            "preset": "custom",
            "presets": {
                "custom": {
                    "filesystem": {
                        "mounts": [{"path": ".", "access": "rw"}],
                        "rules": []
                    },
                    "network": {
                        "mode": "proxy",
                        "domains": {"*": "ask"}
                    }
                }
            }
        });

        assert!(serde_json::from_value::<SandboxConfig>(value).is_err());
    }

    #[test]
    fn invalid_network_address_rules_are_rejected() {
        let value = json!({
            "preset": "custom",
            "presets": {
                "custom": {
                    "filesystem": {
                        "mounts": [{"path": ".", "access": "rw"}],
                        "rules": []
                    },
                    "network": {
                        "mode": "proxy",
                        "hosts": {"*": "ask"},
                        "addresses": {"not-an-ip": "deny"}
                    }
                }
            }
        });
        let parsed = serde_json::from_value::<SandboxConfig>(value).unwrap();
        assert!(parsed.resolve(None).is_err());
    }

    #[test]
    fn unsupported_ports_and_ipc_fields_are_rejected() {
        for value in [
            json!({
                "preset": "custom",
                "presets": {
                    "custom": {
                        "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                        "network": {
                            "mode": "proxy",
                            "hosts": {"*": "ask"},
                            "ports": {"tcp:443": "allow"}
                        }
                    }
                }
            }),
            json!({
                "preset": "custom",
                "presets": {
                    "custom": {
                        "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                        "network": {"mode": "deny", "hosts": {"*": "deny"}},
                        "ipc": {
                            "unix_sockets": {"/var/run/docker.sock": "ask"}
                        }
                    }
                }
            }),
        ] {
            assert!(serde_json::from_value::<SandboxConfig>(value).is_err());
        }
    }

    #[test]
    fn network_hosts_and_addresses_use_specific_match_before_wildcards() -> Result<()> {
        let value = json!({
            "preset": "custom",
            "presets": {
                "custom": {
                    "filesystem": {
                        "mounts": [{"path": ".", "access": "rw"}],
                        "rules": []
                    },
                    "network": {
                        "mode": "proxy",
                        "hosts": {
                            "*": "deny",
                            "*.example.com": "ask",
                            "api.example.com": "allow"
                        },
                        "addresses": {
                            "*": "deny",
                            "127.0.0.0/8": "ask",
                            "127.0.0.1": "allow",
                            "169.254.0.0/16": "deny"
                        }
                    }
                }
            }
        });
        let resolved = serde_json::from_value::<SandboxConfig>(value)?
            .resolve(None)?
            .preset;

        assert_eq!(
            resolved.network.action_for_host("api.example.com"),
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.network.action_for_host("docs.example.com"),
            PermissionAction::Ask
        );
        assert_eq!(
            resolved.network.action_for_host("other.invalid"),
            PermissionAction::Deny
        );
        assert_eq!(
            resolved.network.action_for_address("127.0.0.1".parse()?),
            PermissionAction::Allow
        );
        assert_eq!(
            resolved.network.action_for_address("127.0.0.2".parse()?),
            PermissionAction::Ask
        );
        assert_eq!(
            resolved
                .network
                .action_for_address("169.254.169.254".parse()?),
            PermissionAction::Deny
        );
        Ok(())
    }

    #[test]
    fn parses_env_secret_and_request_filesystem_access_permission() -> Result<()> {
        let _guard = ENV_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env test lock poisoned");
        unsafe {
            std::env::set_var("DUCKAGENT_TEST_OPENAI_API_KEY", "sk-test");
            std::env::set_var("DUCKAGENT_TEST_OPENAI_BASE_URL", "https://api.openai.com");
        }
        let result = (|| -> Result<()> {
            let value = json!({
                "preset": "custom",
                "presets": {
                    "custom": {
                        "filesystem": {
                            "mounts": [{"path": "$CWD", "access": "rw"}],
                            "rules": [{"path": "$TMPDIR/secret/**", "access": "none"}]
                        },
                        "network": {
                            "mode": "proxy",
                            "hosts": {"*": "ask"},
                            "addresses": {"169.254.0.0/16": "deny"}
                        },
                        "env": {
                            "DUCKAGENT_TEST_OPENAI_API_KEY": {
                                "type": "secret",
                                "inject": {
                                    "url": "DUCKAGENT_TEST_OPENAI_BASE_URL",
                                    "header": "Authorization",
                                    "format": "Bearer {}"
                                }
                            }
                        },
                        "permissions": {
                            "tools": {
                                "request_filesystem_access": "ask"
                            }
                        }
                    }
                }
            });

            let resolved = serde_json::from_value::<SandboxConfig>(value)?
                .resolve(None)?
                .preset;
            assert_eq!(
                resolved.permissions.tools["request_filesystem_access"],
                PermissionAction::Ask
            );
            assert_eq!(
                resolved
                    .secrets
                    .exposed_env_placeholders()
                    .get("DUCKAGENT_TEST_OPENAI_API_KEY")
                    .map(String::as_str),
                Some("duckagent-secret:DUCKAGENT_TEST_OPENAI_API_KEY")
            );
            assert_eq!(
                resolved.secrets.0["DUCKAGENT_TEST_OPENAI_API_KEY"]
                    .inject
                    .header,
                "Authorization"
            );
            Ok(())
        })();
        unsafe {
            std::env::remove_var("DUCKAGENT_TEST_OPENAI_API_KEY");
            std::env::remove_var("DUCKAGENT_TEST_OPENAI_BASE_URL");
        }
        result
    }

    #[test]
    fn extends_merge_network_and_replace_env_secrets_by_key() -> Result<()> {
        let _guard = ENV_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env test lock poisoned");
        unsafe {
            std::env::set_var("DUCKAGENT_TEST_SERVICE_TOKEN", "base");
            std::env::set_var("DUCKAGENT_TEST_SERVICE_URL", "https://base.example.com");
            std::env::set_var("DUCKAGENT_TEST_CHILD_SERVICE_TOKEN", "child");
            std::env::set_var(
                "DUCKAGENT_TEST_CHILD_SERVICE_URL",
                "https://child.example.com",
            );
        }
        let result = (|| -> Result<()> {
            let value = json!({
                "preset": "child",
                "presets": {
                    "base": {
                        "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                        "network": {
                            "mode": "proxy",
                            "hosts": {"*": "ask", "api.example.com": "ask"},
                            "addresses": {"10.0.0.0/8": "ask"}
                        },
                        "env": {
                            "DUCKAGENT_TEST_SERVICE_TOKEN": {
                                "type": "secret",
                                "inject": {
                                    "url": "DUCKAGENT_TEST_SERVICE_URL",
                                    "header": "Authorization",
                                    "format": "Bearer {}"
                                }
                            }
                        }
                    },
                    "child": {
                        "extends": "base",
                        "network": {
                            "hosts": {"api.example.com": "allow"},
                            "addresses": {"10.0.0.1": "allow"}
                        },
                        "env": {
                            "DUCKAGENT_TEST_SERVICE_TOKEN": "deny",
                            "DUCKAGENT_TEST_CHILD_SERVICE_TOKEN": {
                                "type": "secret",
                                "inject": {
                                    "url": "DUCKAGENT_TEST_CHILD_SERVICE_URL",
                                    "header": "X-Api-Key",
                                    "format": "{}"
                                }
                            }
                        }
                    }
                }
            });

            let resolved = serde_json::from_value::<SandboxConfig>(value)?
                .resolve(None)?
                .preset;
            assert_eq!(
                resolved.network.hosts["api.example.com"],
                PermissionAction::Allow
            );
            assert_eq!(
                resolved.network.addresses["10.0.0.0/8"],
                PermissionAction::Ask
            );
            assert_eq!(
                resolved.network.addresses["10.0.0.1"],
                PermissionAction::Allow
            );
            assert!(
                !resolved
                    .secrets
                    .exposed_env_placeholders()
                    .contains_key("DUCKAGENT_TEST_SERVICE_TOKEN")
            );
            assert!(
                resolved
                    .secrets
                    .exposed_env_placeholders()
                    .contains_key("DUCKAGENT_TEST_CHILD_SERVICE_TOKEN")
            );
            assert_eq!(
                resolved.secrets.0["DUCKAGENT_TEST_CHILD_SERVICE_TOKEN"]
                    .inject
                    .header,
                "X-Api-Key"
            );
            Ok(())
        })();
        unsafe {
            std::env::remove_var("DUCKAGENT_TEST_SERVICE_TOKEN");
            std::env::remove_var("DUCKAGENT_TEST_SERVICE_URL");
            std::env::remove_var("DUCKAGENT_TEST_CHILD_SERVICE_TOKEN");
            std::env::remove_var("DUCKAGENT_TEST_CHILD_SERVICE_URL");
        }
        result
    }

    #[test]
    fn env_secret_requires_proxy_network_mode() {
        let _guard = ENV_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env test lock poisoned");
        unsafe {
            std::env::set_var("DUCKAGENT_TEST_API_TOKEN", "secret");
            std::env::set_var("DUCKAGENT_TEST_API_URL", "https://api.example.com");
        }
        let value = json!({
            "preset": "custom",
            "presets": {
                "custom": {
                    "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                    "network": {"mode": "allow", "hosts": {"*": "allow"}},
                    "env": {
                        "DUCKAGENT_TEST_API_TOKEN": {
                            "type": "secret",
                            "inject": {
                                "url": "DUCKAGENT_TEST_API_URL",
                                "header": "Authorization",
                                "format": "Bearer {}"
                            }
                        }
                    }
                }
            }
        });
        let parsed = serde_json::from_value::<SandboxConfig>(value).unwrap();
        let error = parsed.resolve(None).unwrap_err().to_string();
        assert!(error.contains("env secret proxy"));
        assert!(error.contains("network.mode = \"proxy\""));
        unsafe {
            std::env::remove_var("DUCKAGENT_TEST_API_TOKEN");
            std::env::remove_var("DUCKAGENT_TEST_API_URL");
        }
    }

    #[test]
    fn extends_cannot_make_env_secret_preset_non_proxy() {
        let _guard = ENV_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env test lock poisoned");
        unsafe {
            std::env::set_var("DUCKAGENT_TEST_API_TOKEN", "secret");
            std::env::set_var("DUCKAGENT_TEST_API_URL", "https://api.example.com");
        }
        let value = json!({
            "preset": "child",
            "presets": {
                "base": {
                    "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                    "network": {"mode": "proxy", "hosts": {"*": "ask"}},
                    "env": {
                        "DUCKAGENT_TEST_API_TOKEN": {
                            "type": "secret",
                            "inject": {
                                "url": "DUCKAGENT_TEST_API_URL",
                                "header": "Authorization",
                                "format": "Bearer {}"
                            }
                        }
                    }
                },
                "child": {
                    "extends": "base",
                    "network": {"mode": "allow", "hosts": {"*": "allow"}}
                }
            }
        });
        let parsed = serde_json::from_value::<SandboxConfig>(value).unwrap();
        let error = parsed.resolve(None).unwrap_err().to_string();
        assert!(error.contains("env secret proxy"));
        assert!(error.contains("network.mode = \"proxy\""));
        unsafe {
            std::env::remove_var("DUCKAGENT_TEST_API_TOKEN");
            std::env::remove_var("DUCKAGENT_TEST_API_URL");
        }
    }

    #[test]
    fn env_secrets_reject_invalid_shapes_missing_env_and_bad_urls() {
        let _guard = ENV_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env test lock poisoned");
        unsafe {
            std::env::remove_var("DUCKAGENT_TEST_BAD_SECRET");
            std::env::set_var("DUCKAGENT_TEST_BAD_URL", "not a url");
            std::env::set_var("DUCKAGENT_TEST_PRESENT_SECRET", "secret");
            std::env::set_var("DUCKAGENT_TEST_PRESENT_URL", "https://api.example.com");
        }
        for env in [
            json!({
                "DUCKAGENT_TEST_PRESENT_SECRET": {
                    "type": "credential",
                    "inject": {
                        "url": "DUCKAGENT_TEST_PRESENT_URL",
                        "header": "Authorization",
                        "format": "Bearer {}"
                    }
                }
            }),
            json!({
                "DUCKAGENT_TEST_BAD_SECRET": {
                    "type": "secret",
                    "inject": {
                        "url": "DUCKAGENT_TEST_PRESENT_URL",
                        "header": "Authorization",
                        "format": "Bearer {}"
                    }
                }
            }),
            json!({
                "DUCKAGENT_TEST_PRESENT_SECRET": {
                    "type": "secret",
                    "inject": {
                        "url": "DUCKAGENT_TEST_BAD_URL",
                        "header": "Authorization",
                        "format": "Bearer {}"
                    }
                }
            }),
            json!({
                "DUCKAGENT_TEST_PRESENT_SECRET": {
                    "type": "secret",
                    "inject": {
                        "url": "DUCKAGENT_TEST_PRESENT_URL",
                        "header": "Authorization",
                        "format": "Bearer"
                    }
                }
            }),
        ] {
            let value = json!({
                "preset": "custom",
                "presets": {
                    "custom": {
                        "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                        "network": {"mode": "proxy", "hosts": {"*": "ask"}},
                        "env": env
                    }
                }
            });
            let parsed = serde_json::from_value::<SandboxConfig>(value).unwrap();
            assert!(parsed.resolve(None).is_err());
        }
        unsafe {
            std::env::remove_var("DUCKAGENT_TEST_BAD_URL");
            std::env::remove_var("DUCKAGENT_TEST_PRESENT_SECRET");
            std::env::remove_var("DUCKAGENT_TEST_PRESENT_URL");
        }
    }

    #[test]
    fn old_top_level_secrets_schema_is_rejected() {
        let value = json!({
            "preset": "custom",
            "presets": {
                "custom": {
                    "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                    "network": {"mode": "proxy", "hosts": {"*": "ask"}},
                    "secrets": {
                        "openai": {
                            "source": "env:OPENAI_API_KEY",
                            "inject": {"type": "header", "name": "Authorization"}
                        }
                    }
                }
            }
        });
        assert!(serde_json::from_value::<SandboxConfig>(value).is_err());
    }
}
