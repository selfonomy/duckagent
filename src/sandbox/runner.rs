use crate::approval::ApprovalProvider;
use crate::sandbox::config::{NetworkMode, PermissionAction, ResolvedSandbox, resolve_sandbox};
use crate::sandbox::network_proxy::{
    MANAGED_PROXY_ENV_KEY, ManagedNetworkProxy, start_if_supported,
};
use anyhow::{Context, Result, bail};
use clap::Args;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, Args)]
pub struct SandboxRunCommand {
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    #[arg(long = "env")]
    pub env: Vec<String>,
    #[arg(last = true, num_args = 1.., allow_hyphen_values = true)]
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct SandboxLinuxInnerCommand {
    #[arg(long = "proxy-route-spec")]
    pub proxy_route_spec: Option<String>,
    #[arg(last = true, num_args = 1.., allow_hyphen_values = true)]
    pub argv: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SandboxCapability {
    pub supported: bool,
    pub backend: String,
    pub message: String,
    pub limitations: Vec<String>,
}

static PARENT_MANAGED_PROXIES: OnceLock<Mutex<Vec<ManagedNetworkProxy>>> = OnceLock::new();
static SESSION_ALLOWED_ENV_KEYS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
#[cfg(test)]
static ENV_ALLOW_PERSIST_RECORDER: OnceLock<Mutex<Option<Vec<String>>>> = OnceLock::new();
pub(crate) const EXPLICIT_ENV_KEYS_ENV: &str = "DUCKAGENT_EXPLICIT_SANDBOX_ENV_KEYS";

pub fn run_hidden_sandbox_command(command: SandboxRunCommand) -> Result<()> {
    if command.argv.is_empty() {
        bail!("__sandbox-run requires a command after --");
    }
    let sandbox = resolve_sandbox()?;
    let explicit_env = parse_env_pairs(&command.env)?;
    let env = sanitize_environment(&sandbox, explicit_env)?;
    let cwd = command
        .cwd
        .unwrap_or(std::env::current_dir().context("failed to resolve sandbox cwd")?);
    crate::sandbox::windows_setup::ensure_setup_for_sandbox(&sandbox, &env)?;

    #[cfg(target_os = "macos")]
    let status = crate::sandbox::backends::macos::run_status(
        &sandbox,
        &command.argv[0],
        &command.argv[1..],
        &cwd,
        env,
    )?;

    #[cfg(target_os = "linux")]
    let status = crate::sandbox::backends::linux::run_status(
        &sandbox,
        &command.argv[0],
        &command.argv[1..],
        &cwd,
        env,
    )?;

    #[cfg(target_os = "windows")]
    let status = crate::sandbox::backends::windows::run_status(
        &sandbox,
        &command.argv[0],
        &command.argv[1..],
        &cwd,
        env,
    )?;

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        bail!("sandbox backend is not available for this platform");
    }

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(target_os = "linux")]
pub fn run_hidden_linux_inner_command(command: SandboxLinuxInnerCommand) -> Result<()> {
    if command.argv.is_empty() {
        bail!("__sandbox-linux-inner requires a command after --");
    }

    if let Some(spec) = command.proxy_route_spec.as_deref() {
        crate::sandbox::backends::linux_proxy_routing::activate_proxy_routes_in_netns(spec)
            .context("failed to activate sandbox proxy routes inside Linux netns")?;
    }
    set_no_new_privs().context("failed to apply no_new_privs in Linux sandbox")?;

    let status = Command::new(&command.argv[0])
        .args(&command.argv[1..])
        .status()
        .with_context(|| {
            format!(
                "failed to execute sandbox inner command `{}`",
                command.argv[0]
            )
        })?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(not(target_os = "linux"))]
pub fn run_hidden_linux_inner_command(_command: SandboxLinuxInnerCommand) -> Result<()> {
    bail!("__sandbox-linux-inner is only available on Linux")
}

pub fn sandbox_command_with_target(
    sandbox: &ResolvedSandbox,
    cwd: Option<&Path>,
    env: BTreeMap<String, String>,
    program: &str,
    args: &[String],
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<Command> {
    if bypass_sandbox_for_tests(sandbox) {
        let mut command = Command::new(program);
        command.args(args).envs(env);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        return Ok(command);
    }

    let mut command = sandbox_command(sandbox, cwd, env, approval_provider)?;
    command.arg(program).args(args);
    Ok(command)
}

pub fn sandbox_command(
    sandbox: &ResolvedSandbox,
    cwd: Option<&Path>,
    mut env: BTreeMap<String, String>,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<Command> {
    env.extend(parent_approved_env_for_sandbox(
        sandbox,
        approval_provider.clone(),
    )?);
    if !env.contains_key(MANAGED_PROXY_ENV_KEY) {
        env.extend(managed_proxy_env_for_parent(sandbox, approval_provider)?);
    }
    crate::sandbox::windows_setup::ensure_setup_for_sandbox(sandbox, &env)?;
    let mut command = Command::new(std::env::current_exe()?);
    command.arg("--sandbox").arg(&sandbox.name);
    command.arg("__sandbox-run");
    if let Some(cwd) = cwd {
        command.arg("--cwd").arg(cwd);
    }
    for (key, value) in env {
        command.arg("--env").arg(format!("{key}={value}"));
    }
    command.arg("--");
    Ok(command)
}

pub fn managed_proxy_env_for_parent(
    sandbox: &ResolvedSandbox,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<BTreeMap<String, String>> {
    #[cfg(target_os = "windows")]
    if matches!(sandbox.preset.network.mode, NetworkMode::Proxy)
        && let Some(env) = existing_parent_proxy_env()
    {
        return Ok(env);
    }

    let Some(proxy) = start_if_supported(sandbox, approval_provider)? else {
        return Ok(BTreeMap::new());
    };
    let env = proxy.env_overrides();
    keep_parent_proxy_alive(proxy);
    Ok(env)
}

pub fn parent_explicit_env_for_sandbox(
    sandbox: &ResolvedSandbox,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<BTreeMap<String, String>> {
    let mut env = parent_approved_env_for_sandbox(sandbox, approval_provider.clone())?;
    env.extend(managed_proxy_env_for_parent(sandbox, approval_provider)?);
    Ok(env)
}

fn keep_parent_proxy_alive(proxy: ManagedNetworkProxy) {
    PARENT_MANAGED_PROXIES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("managed proxy registry poisoned")
        .push(proxy);
}

#[cfg(target_os = "windows")]
fn existing_parent_proxy_env() -> Option<BTreeMap<String, String>> {
    PARENT_MANAGED_PROXIES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("managed proxy registry poisoned")
        .last()
        .map(ManagedNetworkProxy::env_overrides)
}

pub fn check_platform_capability(sandbox: &ResolvedSandbox) -> SandboxCapability {
    let backend = platform_backend_name().to_string();
    if matches!(sandbox.preset.network.mode, NetworkMode::Allow) && sandbox.is_full_access() {
        return SandboxCapability {
            supported: true,
            backend,
            message: "danger preset runs without an OS sandbox by design".to_string(),
            limitations: Vec::new(),
        };
    }

    let limitations = platform_limitations(sandbox);

    #[cfg(target_os = "linux")]
    {
        if !crate::sandbox::backends::linux::has_bwrap_backend() {
            return SandboxCapability {
                supported: false,
                backend,
                message:
                    "Linux sandbox requires system bubblewrap or duckagent vendored bubblewrap"
                        .to_string(),
                limitations,
            };
        }
        if let Some(message) = crate::sandbox::backends::linux::startup_preflight_error() {
            return SandboxCapability {
                supported: false,
                backend,
                message,
                limitations,
            };
        }
    }
    #[cfg(target_os = "windows")]
    {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let plan =
            crate::sandbox::backends::windows_plan::WindowsSandboxPlan::from_sandbox(sandbox, &cwd);
        if !sandbox.is_full_access()
            && !crate::sandbox::windows_setup::setup_supports_sandbox(sandbox)
        {
            return SandboxCapability {
                supported: false,
                backend,
                message: format!(
                    "Windows sandbox requires elevated setup matching this preset before it can run. Required enforcement: {}",
                    plan.required_enforcement_summary()
                ),
                limitations,
            };
        }
        let supported = plan.unsupported_reasons.is_empty();
        return SandboxCapability {
            supported,
            backend,
            message: if supported {
                plan.supported_runtime_summary()
            } else {
                plan.unsupported_runtime_summary()
            },
            limitations,
        };
    }
    #[cfg(not(target_os = "windows"))]
    {
        SandboxCapability {
            supported: true,
            backend,
            message: "sandbox backend is available for this platform".to_string(),
            limitations,
        }
    }
}

fn platform_limitations(sandbox: &ResolvedSandbox) -> Vec<String> {
    let mut limitations = Vec::new();

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if sandbox
        .preset
        .filesystem
        .rules
        .iter()
        .any(|rule| filesystem_pattern_contains_glob(&rule.path))
    {
        limitations.push(
            "filesystem glob rules for child processes are launch-time snapshots on this platform"
                .to_string(),
        );
    }

    if sandbox.preset.secrets.0.values().next().is_some() {
        limitations.push(
            "env secret injection only works for clients that use the configured URL environment variable rewritten to duckagent's reverse proxy"
                .to_string(),
        );
    }

    limitations
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn filesystem_pattern_contains_glob(pattern: &str) -> bool {
    pattern
        .chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | ']' | '{' | '}'))
}

pub fn network_status(sandbox: &ResolvedSandbox) -> serde_json::Value {
    let allow_hosts = allowed_network_hosts(sandbox);
    serde_json::json!({
        "mode": sandbox.preset.network.mode,
        "backend": platform_backend_name(),
        "allowed_hosts": allow_hosts,
        "wildcard": sandbox.preset.network.default_action(),
        "upstream_proxy": upstream_proxy_env(),
    })
}

fn allowed_network_hosts(sandbox: &ResolvedSandbox) -> Vec<String> {
    if matches!(
        sandbox.preset.network.default_action(),
        PermissionAction::Allow
    ) {
        return vec!["*".to_string()];
    }
    sandbox
        .preset
        .network
        .hosts
        .iter()
        .filter_map(|(host, action)| {
            (host != "*" && matches!(action, PermissionAction::Allow)).then_some(host.clone())
        })
        .collect()
}

pub fn bypass_sandbox_for_tests(sandbox: &ResolvedSandbox) -> bool {
    bypass_sandbox_for_tests_impl(sandbox)
}

#[cfg(test)]
fn bypass_sandbox_for_tests_impl(sandbox: &ResolvedSandbox) -> bool {
    sandbox.name == "danger" || sandbox.is_full_access()
}

#[cfg(not(test))]
fn bypass_sandbox_for_tests_impl(_sandbox: &ResolvedSandbox) -> bool {
    false
}

fn parse_env_pairs(values: &[String]) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for value in values {
        let Some((key, val)) = value.split_once('=') else {
            bail!("invalid --env value `{value}`; expected KEY=VALUE");
        };
        env.insert(key.to_string(), val.to_string());
    }
    Ok(env)
}

pub(crate) fn sanitize_environment(
    sandbox: &ResolvedSandbox,
    explicit_env: BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let policy = &sandbox.preset.env;
    let mut env = BTreeMap::new();

    for (key, value) in std::env::vars() {
        if matches!(
            env_action_for_key(policy, &key),
            Some(PermissionAction::Allow)
        ) {
            env.insert(key, value);
        }
    }

    for (key, value) in explicit_env {
        env.insert(key, value);
    }
    env.extend(sandbox.preset.secrets.exposed_env_placeholders());

    Ok(env)
}

fn parent_approved_env_for_sandbox(
    sandbox: &ResolvedSandbox,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<BTreeMap<String, String>> {
    let ask_keys = parent_env_keys_matching_action(&sandbox.preset.env, PermissionAction::Ask);
    if ask_keys.is_empty() {
        return Ok(BTreeMap::new());
    }

    let mut approved = BTreeMap::new();
    let mut pending = Vec::new();
    {
        let session = session_allowed_env_keys()
            .lock()
            .expect("session env approval cache poisoned");
        for key in ask_keys {
            if session.contains(&key) {
                if let Ok(value) = std::env::var(&key) {
                    approved.insert(key, value);
                }
            } else {
                pending.push(key);
            }
        }
    }

    if pending.is_empty() {
        return Ok(approved);
    }
    let Some(provider) = approval_provider else {
        return Ok(approved);
    };

    let command = format!("env-access {}", pending.join(", "));
    let rule_hits = pending
        .iter()
        .map(|key| crate::approval::RuleHit {
            rule_id: "sandbox.env.ask".to_string(),
            description: format!("environment variable `{key}` matched sandbox env ask rule"),
        })
        .collect::<Vec<_>>();
    let response = provider
        .request_approval(
            &command,
            &rule_hits,
            crate::approval::ApprovalDecision::options(),
        )
        .unwrap_or(crate::approval::ApprovalResponse {
            decision: crate::approval::ApprovalDecision::Forbidden,
        });
    audit_env_approval(sandbox, &pending, response.decision);

    match response.decision {
        crate::approval::ApprovalDecision::Once => {
            for key in pending {
                if let Ok(value) = std::env::var(&key) {
                    approved.insert(key, value);
                }
            }
        }
        crate::approval::ApprovalDecision::Session => {
            let mut session = session_allowed_env_keys()
                .lock()
                .expect("session env approval cache poisoned");
            for key in pending {
                session.insert(key.clone());
                if let Ok(value) = std::env::var(&key) {
                    approved.insert(key, value);
                }
            }
        }
        crate::approval::ApprovalDecision::Always => {
            let mut session = session_allowed_env_keys()
                .lock()
                .expect("session env approval cache poisoned");
            for key in pending {
                persist_env_allow_rule(&key)
                    .with_context(|| format!("failed to persist env allow rule for `{key}`"))?;
                session.insert(key.clone());
                if let Ok(value) = std::env::var(&key) {
                    approved.insert(key, value);
                }
            }
        }
        crate::approval::ApprovalDecision::Forbidden => {}
    }

    Ok(approved)
}

fn persist_env_allow_rule(key: &str) -> Result<()> {
    #[cfg(test)]
    {
        if let Some(recorder) = ENV_ALLOW_PERSIST_RECORDER.get() {
            let mut guard = recorder
                .lock()
                .expect("env allow persist recorder mutex poisoned");
            if let Some(keys) = guard.as_mut() {
                keys.push(key.to_string());
                return Ok(());
            }
        }
    }
    crate::sandbox::config::append_env_action_to_current_preset(key, PermissionAction::Allow)
}

fn audit_env_approval(
    sandbox: &ResolvedSandbox,
    keys: &[String],
    decision: crate::approval::ApprovalDecision,
) {
    let mut event = crate::audit::AuditEvent::new("env", "approval");
    event.sandbox = Some(sandbox.name.clone());
    event.outcome = approval_decision_label(decision).to_string();
    event.fields = serde_json::json!({
        "keys": keys,
        "key_count": keys.len(),
    });
    crate::audit::record(event);
}

fn approval_decision_label(decision: crate::approval::ApprovalDecision) -> &'static str {
    match decision {
        crate::approval::ApprovalDecision::Once => "once",
        crate::approval::ApprovalDecision::Session => "session",
        crate::approval::ApprovalDecision::Always => "always",
        crate::approval::ApprovalDecision::Forbidden => "blocked",
    }
}

pub(crate) fn explicit_env_from_current_process() -> BTreeMap<String, String> {
    let mut env = crate::sandbox::network_proxy::proxy_env_from_current_environment();
    if let Ok(raw) = std::env::var(EXPLICIT_ENV_KEYS_ENV)
        && let Ok(keys) = serde_json::from_str::<Vec<String>>(&raw)
    {
        for key in keys {
            if let Ok(value) = std::env::var(&key) {
                env.insert(key, value);
            }
        }
    }
    env
}

pub(crate) fn explicit_env_keys_marker(env: &BTreeMap<String, String>) -> Option<String> {
    let keys = env.keys().cloned().collect::<Vec<_>>();
    serde_json::to_string(&keys).ok()
}

fn parent_env_keys_matching_action(
    policy: &crate::sandbox::config::EnvPolicy,
    action: PermissionAction,
) -> Vec<String> {
    std::env::vars()
        .filter_map(|(key, _)| (env_action_for_key(policy, &key) == Some(action)).then_some(key))
        .collect()
}

fn env_action_for_key(
    policy: &crate::sandbox::config::EnvPolicy,
    key: &str,
) -> Option<PermissionAction> {
    let normalized_rules = policy
        .permission_rules()
        .into_iter()
        .map(|(pattern, action)| (pattern.to_ascii_lowercase(), action))
        .collect::<BTreeMap<_, _>>();
    crate::sandbox::shell_permissions::permission_action_for_pattern(
        &normalized_rules,
        &key.to_ascii_lowercase(),
        None,
    )
}

fn session_allowed_env_keys() -> &'static Mutex<HashSet<String>> {
    SESSION_ALLOWED_ENV_KEYS.get_or_init(|| Mutex::new(HashSet::new()))
}

#[cfg(target_os = "linux")]
fn set_no_new_privs() -> Result<()> {
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc != 0 {
        Err(std::io::Error::last_os_error()).context("prctl(PR_SET_NO_NEW_PRIVS) failed")
    } else {
        Ok(())
    }
}

fn upstream_proxy_env() -> Option<String> {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .into_iter()
    .find_map(|key| std::env::var(key).ok())
    .filter(|value| !value.trim().is_empty())
}

fn platform_backend_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "duckagent-macos-seatbelt"
    }
    #[cfg(target_os = "linux")]
    {
        "duckagent-linux-bubblewrap"
    }
    #[cfg(target_os = "windows")]
    {
        "duckagent-windows"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        "duckagent-unsupported"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{ApprovalDecision, ApprovalResponse, RuleHit};
    use crate::sandbox::config::{EnvEntry, SandboxConfig};
    use std::sync::Arc;
    use uuid::Uuid;

    struct RecordingApprovalProvider {
        prompts: Mutex<Vec<(String, Vec<RuleHit>)>>,
        decision: ApprovalDecision,
    }

    impl RecordingApprovalProvider {
        fn new(decision: ApprovalDecision) -> Self {
            Self {
                prompts: Mutex::new(Vec::new()),
                decision,
            }
        }
    }

    impl ApprovalProvider for RecordingApprovalProvider {
        fn request_approval(
            &self,
            command: &str,
            rule_hits: &[RuleHit],
            _options: [ApprovalDecision; 4],
        ) -> Option<ApprovalResponse> {
            self.prompts
                .lock()
                .expect("approval prompt recorder poisoned")
                .push((command.to_string(), rule_hits.to_vec()));
            Some(ApprovalResponse {
                decision: self.decision,
            })
        }
    }

    #[test]
    fn proxy_mode_allows_only_explicit_hosts_when_default_ask() -> Result<()> {
        let config = SandboxConfig::default();
        let sandbox = config.resolve(Some("workspace"))?;
        let hosts = allowed_network_hosts(&sandbox);
        assert!(hosts.contains(&"localhost".to_string()));
        assert!(!hosts.contains(&"*".to_string()));
        Ok(())
    }

    #[test]
    fn danger_maps_to_wildcard_network() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("danger"))?;
        assert_eq!(allowed_network_hosts(&sandbox), vec!["*".to_string()]);
        assert!(sandbox.is_full_access());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn platform_limitations_report_launch_time_glob_filesystem_rules() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        let limitations = platform_limitations(&sandbox);
        assert!(
            limitations
                .iter()
                .any(|item| item.contains("launch-time snapshots"))
        );
        Ok(())
    }

    #[test]
    fn platform_capability_includes_limitations_field() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        let capability = check_platform_capability(&sandbox);
        assert!(!capability.backend.trim().is_empty());
        // The field is intentionally always present so `sandbox check` can be
        // used as a stable platform-support probe in CI.
        let _ = capability.limitations;
        Ok(())
    }

    fn env_contains_key_case_insensitive(env: &BTreeMap<String, String>, key: &str) -> bool {
        env.keys()
            .any(|candidate| candidate.eq_ignore_ascii_case(key))
    }

    fn parent_env_has_key_case_insensitive(key: &str) -> bool {
        std::env::vars_os()
            .any(|(candidate, _)| candidate.to_string_lossy().eq_ignore_ascii_case(key))
    }

    #[test]
    fn sandbox_env_inherits_all_parent_keys_by_default() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        let env = sanitize_environment(&sandbox, BTreeMap::new())?;

        if parent_env_has_key_case_insensitive("PATH") {
            assert!(env_contains_key_case_insensitive(&env, "PATH"));
        }
        Ok(())
    }

    #[test]
    fn sandbox_env_deny_filters_parent_inheritance_case_insensitively() -> Result<()> {
        let mut sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        sandbox
            .preset
            .env
            .0
            .insert("path".to_string(), EnvEntry::Action(PermissionAction::Deny));

        let env = sanitize_environment(&sandbox, BTreeMap::new())?;
        assert!(!env_contains_key_case_insensitive(&env, "PATH"));
        Ok(())
    }

    #[test]
    fn sandbox_env_specific_allow_overrides_wildcard_deny() -> Result<()> {
        let mut sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        sandbox.preset.env.0.clear();
        sandbox
            .preset
            .env
            .0
            .insert("*".to_string(), EnvEntry::Action(PermissionAction::Deny));
        sandbox.preset.env.0.insert(
            "PATH".to_string(),
            EnvEntry::Action(PermissionAction::Allow),
        );

        let env = sanitize_environment(&sandbox, BTreeMap::new())?;
        if parent_env_has_key_case_insensitive("PATH") {
            assert!(env_contains_key_case_insensitive(&env, "PATH"));
        }
        Ok(())
    }

    #[test]
    fn sandbox_env_does_not_reject_explicit_env() -> Result<()> {
        let mut sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        sandbox.preset.env.0.insert(
            "*_TOKEN".to_string(),
            EnvEntry::Action(PermissionAction::Deny),
        );
        let mut explicit = BTreeMap::new();
        explicit.insert("MY_TOKEN".to_string(), "secret".to_string());

        let env = sanitize_environment(&sandbox, explicit)?;
        assert_eq!(env.get("MY_TOKEN").map(String::as_str), Some("secret"));
        Ok(())
    }

    #[test]
    fn sandbox_secret_placeholder_overrides_inherited_and_explicit_env() -> Result<()> {
        let _guard = ENV_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env test lock poisoned");
        unsafe {
            std::env::set_var("DUCKAGENT_TEST_SECRET_PLACEHOLDER", "real-parent-secret");
        }
        let result = (|| -> Result<()> {
            let mut sandbox = SandboxConfig::default().resolve(Some("danger"))?;
            sandbox.preset.secrets.0.insert(
                "DUCKAGENT_TEST_SECRET_PLACEHOLDER".to_string(),
                crate::sandbox::config::SecretConfig {
                    source_env: "DUCKAGENT_TEST_SECRET_PLACEHOLDER".to_string(),
                    url_env: "DUCKAGENT_TEST_SECRET_PLACEHOLDER_URL".to_string(),
                    inject: crate::sandbox::config::SecretInjectConfig {
                        header: "Authorization".to_string(),
                        format: "Bearer {}".to_string(),
                    },
                },
            );
            let explicit = BTreeMap::from([(
                "DUCKAGENT_TEST_SECRET_PLACEHOLDER".to_string(),
                "explicit-real-secret".to_string(),
            )]);

            let env = sanitize_environment(&sandbox, explicit)?;

            assert_eq!(
                env.get("DUCKAGENT_TEST_SECRET_PLACEHOLDER")
                    .map(String::as_str),
                Some("duckagent-secret:DUCKAGENT_TEST_SECRET_PLACEHOLDER")
            );
            Ok(())
        })();
        unsafe {
            std::env::remove_var("DUCKAGENT_TEST_SECRET_PLACEHOLDER");
        }
        result
    }

    #[test]
    fn sandbox_env_ask_aggregates_existing_keys_without_values() -> Result<()> {
        let _guard = ENV_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env test lock poisoned");
        unsafe {
            std::env::set_var("DUCKAGENT_TEST_ENV_A", "secret-a");
            std::env::set_var("DUCKAGENT_TEST_ENV_B", "secret-b");
        }

        let result = (|| -> Result<()> {
            let mut sandbox = SandboxConfig::default().resolve(Some("danger"))?;
            sandbox.preset.env.0.clear();
            sandbox.preset.env.0.insert(
                "DUCKAGENT_TEST_ENV_*".to_string(),
                EnvEntry::Action(PermissionAction::Ask),
            );
            let provider = Arc::new(RecordingApprovalProvider::new(ApprovalDecision::Once));

            let env = parent_explicit_env_for_sandbox(&sandbox, Some(provider.clone()))?;
            assert_eq!(
                env.get("DUCKAGENT_TEST_ENV_A").map(String::as_str),
                Some("secret-a")
            );
            assert_eq!(
                env.get("DUCKAGENT_TEST_ENV_B").map(String::as_str),
                Some("secret-b")
            );

            let prompts = provider
                .prompts
                .lock()
                .expect("approval prompt recorder poisoned");
            assert_eq!(prompts.len(), 1);
            assert!(prompts[0].0.contains("DUCKAGENT_TEST_ENV_A"));
            assert!(prompts[0].0.contains("DUCKAGENT_TEST_ENV_B"));
            assert!(!prompts[0].0.contains("secret-a"));
            assert!(!prompts[0].0.contains("secret-b"));
            Ok(())
        })();

        unsafe {
            std::env::remove_var("DUCKAGENT_TEST_ENV_A");
            std::env::remove_var("DUCKAGENT_TEST_ENV_B");
        }
        result
    }

    #[test]
    fn sandbox_env_ask_ignores_missing_parent_keys_without_prompt() -> Result<()> {
        let _guard = ENV_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env test lock poisoned");
        let key = format!("DUCKAGENT_TEST_ENV_MISSING_{}", Uuid::now_v7().simple());
        unsafe {
            std::env::remove_var(&key);
        }

        let mut sandbox = SandboxConfig::default().resolve(Some("danger"))?;
        sandbox.preset.env.0.clear();
        sandbox
            .preset
            .env
            .0
            .insert(key.clone(), EnvEntry::Action(PermissionAction::Ask));
        let provider = Arc::new(RecordingApprovalProvider::new(ApprovalDecision::Once));

        let env = parent_explicit_env_for_sandbox(&sandbox, Some(provider.clone()))?;
        assert!(!env.contains_key(&key));
        assert!(
            provider
                .prompts
                .lock()
                .expect("approval prompt recorder poisoned")
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn sandbox_env_ask_always_persists_each_existing_key_as_allow() -> Result<()> {
        let _guard = ENV_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env test lock poisoned");
        let prefix = format!("DUCKAGENT_TEST_ENV_ALWAYS_{}", Uuid::now_v7().simple());
        let key_a = format!("{prefix}_A");
        let key_b = format!("{prefix}_B");
        unsafe {
            std::env::set_var(&key_a, "secret-a");
            std::env::set_var(&key_b, "secret-b");
        }

        let recorder = ENV_ALLOW_PERSIST_RECORDER.get_or_init(|| Mutex::new(None));
        {
            let mut guard = recorder
                .lock()
                .expect("env allow persist recorder mutex poisoned");
            *guard = Some(Vec::new());
        }

        let result = (|| -> Result<()> {
            let mut sandbox = SandboxConfig::default().resolve(Some("danger"))?;
            sandbox.preset.env.0.clear();
            sandbox.preset.env.0.insert(
                format!("{prefix}_*"),
                EnvEntry::Action(PermissionAction::Ask),
            );
            let provider = Arc::new(RecordingApprovalProvider::new(ApprovalDecision::Always));

            let env = parent_explicit_env_for_sandbox(&sandbox, Some(provider.clone()))?;
            assert_eq!(env.get(&key_a).map(String::as_str), Some("secret-a"));
            assert_eq!(env.get(&key_b).map(String::as_str), Some("secret-b"));

            let prompts = provider
                .prompts
                .lock()
                .expect("approval prompt recorder poisoned");
            assert_eq!(prompts.len(), 1);

            let mut persisted = ENV_ALLOW_PERSIST_RECORDER
                .get()
                .expect("env allow persist recorder initialized")
                .lock()
                .expect("env allow persist recorder mutex poisoned")
                .clone()
                .unwrap_or_default();
            persisted.sort();
            let mut expected = vec![key_a.clone(), key_b.clone()];
            expected.sort();
            assert_eq!(persisted, expected);
            Ok(())
        })();

        {
            let mut guard = recorder
                .lock()
                .expect("env allow persist recorder mutex poisoned");
            *guard = None;
        }
        unsafe {
            std::env::remove_var(&key_a);
            std::env::remove_var(&key_b);
        }
        result
    }
}

#[cfg(test)]
static ENV_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
