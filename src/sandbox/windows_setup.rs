use crate::sandbox::config::{
    NetworkMode, ResolvedSandbox, resolve_sandbox, set_active_sandbox_preset,
    set_cli_sandbox_override,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const SETUP_VERSION: u32 = 3;
pub(crate) const SETUP_BACKEND: &str = "elevated_setup";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowsSandboxStartupRequirement {
    NotWindows,
    FullAccessPreset,
    SetupComplete,
    NeedsSetup { preset: String, reason: String },
}

impl WindowsSandboxStartupRequirement {
    pub fn needs_prompt(&self) -> bool {
        matches!(self, Self::NeedsSetup { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WindowsSandboxSetupMarker {
    pub version: u32,
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline_username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub online_username: Option<String>,
    #[serde(default)]
    pub proxy_ports: Vec<u16>,
    #[serde(default)]
    pub allow_local_binding: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

impl WindowsSandboxSetupMarker {
    pub fn current() -> Self {
        Self {
            version: SETUP_VERSION,
            backend: SETUP_BACKEND.to_string(),
            offline_username: None,
            online_username: None,
            proxy_ports: Vec::new(),
            allow_local_binding: false,
            created_at: None,
        }
    }

    pub fn is_current(&self) -> bool {
        self.version == SETUP_VERSION && self.backend == SETUP_BACKEND
    }

    pub fn satisfies_sandbox(&self, sandbox: &ResolvedSandbox) -> bool {
        if !self.is_current() || self.allow_local_binding {
            return false;
        }
        if matches!(sandbox.preset.network.mode, NetworkMode::Proxy) {
            self.proxy_ports.len() == 1 && self.proxy_ports[0] != 0
        } else {
            self.proxy_ports.is_empty()
        }
    }

    pub fn proxy_port_for_sandbox(&self, sandbox: &ResolvedSandbox) -> Option<u16> {
        if self.satisfies_sandbox(sandbox)
            && matches!(sandbox.preset.network.mode, NetworkMode::Proxy)
        {
            self.proxy_ports.first().copied()
        } else {
            None
        }
    }
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn startup_requirement() -> Result<WindowsSandboxStartupRequirement> {
    let sandbox = resolve_sandbox()?;
    Ok(startup_requirement_for_platform(
        &sandbox,
        setup_supports_sandbox(&sandbox),
        cfg!(target_os = "windows"),
    ))
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn startup_prompt_is_needed() -> Result<bool> {
    Ok(startup_requirement()?.needs_prompt())
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn activate_danger_for_current_process() -> Result<()> {
    set_active_sandbox_preset("danger").context("failed to persist sandbox preset `danger`")?;
    set_cli_sandbox_override(Some("danger".to_string()));
    Ok(())
}

pub fn run_elevated_setup() -> Result<()> {
    run_elevated_setup_impl()
}

pub fn ensure_setup_for_sandbox(
    sandbox: &crate::sandbox::config::ResolvedSandbox,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    ensure_setup_for_sandbox_impl(sandbox, env)
}

pub fn run_setup_helper(
    duckagent_home: PathBuf,
    proxy_mode: bool,
    proxy_ports: Vec<u16>,
    allow_local_binding: bool,
) -> Result<()> {
    run_setup_helper_impl(
        &duckagent_home,
        proxy_mode,
        proxy_ports,
        allow_local_binding,
    )
}

pub fn setup_is_complete() -> bool {
    let Some(home) = duckagent_home_dir() else {
        return false;
    };
    setup_is_complete_at(&home)
}

pub fn setup_is_complete_at(duckagent_home: &Path) -> bool {
    let marker_is_current = match load_marker_at(&setup_marker_path_at(duckagent_home)) {
        Ok(Some(marker)) => marker.is_current(),
        Ok(None) | Err(_) => false,
    };
    marker_is_current && setup_users_path_at(duckagent_home).exists()
}

pub fn setup_supports_sandbox(sandbox: &ResolvedSandbox) -> bool {
    if sandbox.is_full_access() {
        return true;
    }
    let Some(home) = duckagent_home_dir() else {
        return false;
    };
    setup_supports_sandbox_at(&home, sandbox)
}

pub fn setup_supports_sandbox_at(duckagent_home: &Path, sandbox: &ResolvedSandbox) -> bool {
    if sandbox.is_full_access() {
        return true;
    }
    let marker = match load_marker_at(&setup_marker_path_at(duckagent_home)) {
        Ok(Some(marker)) => marker,
        Ok(None) | Err(_) => return false,
    };
    marker.satisfies_sandbox(sandbox) && setup_users_path_at(duckagent_home).exists()
}

pub fn setup_marker_path() -> Result<PathBuf> {
    duckagent_home_dir()
        .map(|home| setup_marker_path_at(&home))
        .context("failed to resolve duckagent home directory")
}

#[cfg(target_os = "windows")]
pub(crate) fn prepare_managed_proxy_port(sandbox: &ResolvedSandbox) -> Result<u16> {
    if !matches!(sandbox.preset.network.mode, NetworkMode::Proxy) {
        anyhow::bail!("Windows managed proxy port was requested for a non-proxy sandbox preset");
    }
    let home = duckagent_home_dir().context("failed to resolve duckagent home directory")?;
    if let Some(port) = marker_proxy_port_for_sandbox_at(&home, sandbox)? {
        return Ok(port);
    }
    crate::sandbox::backends::windows::setup::run_elevated_setup(&home, true, &[], false)?;
    marker_proxy_port_for_sandbox_at(&home, sandbox)?.context(
        "Windows sandbox setup completed but did not record a managed proxy port for this preset",
    )
}

#[cfg(target_os = "windows")]
pub(crate) fn refresh_managed_proxy_port_after_bind_failure(
    sandbox: &ResolvedSandbox,
    busy_port: u16,
) -> Result<u16> {
    if !matches!(sandbox.preset.network.mode, NetworkMode::Proxy) {
        anyhow::bail!(
            "Windows managed proxy port refresh was requested for a non-proxy sandbox preset"
        );
    }
    let home = duckagent_home_dir().context("failed to resolve duckagent home directory")?;
    crate::sandbox::backends::windows::setup::run_elevated_setup(&home, true, &[], false)
        .with_context(|| {
            format!(
                "failed to refresh Windows sandbox setup after managed proxy port {busy_port} was busy"
            )
        })?;
    marker_proxy_port_for_sandbox_at(&home, sandbox)?.context(
        "Windows sandbox setup refresh completed but did not record a managed proxy port for this preset",
    )
}

fn marker_proxy_port_for_sandbox_at(
    duckagent_home: &Path,
    sandbox: &ResolvedSandbox,
) -> Result<Option<u16>> {
    let marker = match load_marker_at(&setup_marker_path_at(duckagent_home))? {
        Some(marker) => marker,
        None => return Ok(None),
    };
    if !setup_users_path_at(duckagent_home).exists() {
        return Ok(None);
    }
    Ok(marker.proxy_port_for_sandbox(sandbox))
}

fn startup_requirement_for_platform(
    sandbox: &ResolvedSandbox,
    setup_complete: bool,
    is_windows: bool,
) -> WindowsSandboxStartupRequirement {
    if !is_windows {
        return WindowsSandboxStartupRequirement::NotWindows;
    }
    if sandbox.is_full_access() {
        return WindowsSandboxStartupRequirement::FullAccessPreset;
    }
    if setup_complete {
        return WindowsSandboxStartupRequirement::SetupComplete;
    }
    WindowsSandboxStartupRequirement::NeedsSetup {
        preset: sandbox.name.clone(),
        reason: "Windows workspace/readonly sandbox requires an elevated setup before it can safely enforce filesystem and network policy".to_string(),
    }
}

fn load_marker_at(path: &Path) -> Result<Option<WindowsSandboxSetupMarker>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to read Windows sandbox setup marker: {}",
                    path.display()
                )
            });
        }
    };
    let marker = serde_json::from_str(&text).with_context(|| {
        format!(
            "failed to parse Windows sandbox setup marker: {}",
            path.display()
        )
    })?;
    Ok(Some(marker))
}

fn setup_marker_path_at(duckagent_home: &Path) -> PathBuf {
    duckagent_home
        .join("sandbox")
        .join("windows")
        .join("setup_marker.json")
}

fn setup_users_path_at(duckagent_home: &Path) -> PathBuf {
    duckagent_home
        .join("sandbox")
        .join("windows")
        .join("sandbox_users.json")
}

fn duckagent_home_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".duckagent"))
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn desired_network_setup(
    sandbox: &crate::sandbox::config::ResolvedSandbox,
    env: &BTreeMap<String, String>,
) -> Result<(bool, Vec<u16>, bool)> {
    if sandbox.is_full_access() {
        return Ok((false, Vec::new(), false));
    }
    let proxy_mode = matches!(sandbox.preset.network.mode, NetworkMode::Proxy);
    let proxy_ports = if proxy_mode {
        crate::sandbox::network_proxy::managed_proxy_addr_from_env(env)?
            .map(|addr| vec![addr.port()])
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Ok((proxy_mode, proxy_ports, false))
}

fn setup_required_error_message(
    sandbox_name: &str,
    proxy_mode: bool,
    proxy_ports: &[u16],
    allow_local_binding: bool,
) -> String {
    format!(
        "Windows sandbox requires elevated setup matching preset `{sandbox_name}` before it can run. Required setup: proxy_mode={proxy_mode}, proxy_ports={proxy_ports:?}, allow_local_binding={allow_local_binding}. Run `duck sandbox setup-windows` or choose the `danger` sandbox preset."
    )
}

#[cfg(target_os = "windows")]
fn run_elevated_setup_impl() -> Result<()> {
    let home = duckagent_home_dir().context("failed to resolve duckagent home directory")?;
    let sandbox = resolve_sandbox()?;
    let proxy_mode = matches!(sandbox.preset.network.mode, NetworkMode::Proxy);
    crate::sandbox::backends::windows::setup::run_elevated_setup(&home, proxy_mode, &[], false)
}

#[cfg(not(target_os = "windows"))]
fn run_elevated_setup_impl() -> Result<()> {
    anyhow::bail!("Windows elevated sandbox setup is only supported on Windows")
}

#[cfg(target_os = "windows")]
fn run_setup_helper_impl(
    duckagent_home: &Path,
    proxy_mode: bool,
    proxy_ports: Vec<u16>,
    allow_local_binding: bool,
) -> Result<()> {
    crate::sandbox::backends::windows::setup::run_setup_helper(
        duckagent_home,
        proxy_mode,
        &proxy_ports,
        allow_local_binding,
    )
}

#[cfg(target_os = "windows")]
fn ensure_setup_for_sandbox_impl(
    sandbox: &crate::sandbox::config::ResolvedSandbox,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    if sandbox.is_full_access() {
        return Ok(());
    }
    let home = duckagent_home_dir().context("failed to resolve duckagent home directory")?;
    let (proxy_mode, proxy_ports, allow_local_binding) = desired_network_setup(sandbox, env)?;
    if crate::sandbox::backends::windows::setup::setup_matches(
        &home,
        proxy_mode,
        &proxy_ports,
        allow_local_binding,
    ) {
        return Ok(());
    }
    anyhow::bail!(setup_required_error_message(
        &sandbox.name,
        proxy_mode,
        &proxy_ports,
        allow_local_binding
    ))
}

#[cfg(not(target_os = "windows"))]
fn ensure_setup_for_sandbox_impl(
    _sandbox: &crate::sandbox::config::ResolvedSandbox,
    _env: &BTreeMap<String, String>,
) -> Result<()> {
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn run_setup_helper_impl(
    _duckagent_home: &Path,
    _proxy_mode: bool,
    _proxy_ports: Vec<u16>,
    _allow_local_binding: bool,
) -> Result<()> {
    anyhow::bail!("Windows sandbox setup helper is only supported on Windows")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::config::SandboxConfig;

    #[test]
    fn danger_preset_does_not_need_windows_setup_prompt() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("danger"))?;
        assert_eq!(
            startup_requirement_for_platform(&sandbox, false, true),
            WindowsSandboxStartupRequirement::FullAccessPreset
        );
        Ok(())
    }

    #[test]
    fn workspace_preset_prompts_for_windows_setup_when_marker_missing() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        let requirement = startup_requirement_for_platform(&sandbox, false, true);
        assert!(requirement.needs_prompt());
        assert!(matches!(
            requirement,
            WindowsSandboxStartupRequirement::NeedsSetup { ref preset, .. } if preset == "workspace"
        ));
        Ok(())
    }

    #[test]
    fn workspace_preset_skips_prompt_after_setup_marker() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        assert_eq!(
            startup_requirement_for_platform(&sandbox, true, true),
            WindowsSandboxStartupRequirement::SetupComplete
        );
        Ok(())
    }

    #[test]
    fn non_windows_platform_never_prompts() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        assert_eq!(
            startup_requirement_for_platform(&sandbox, false, false),
            WindowsSandboxStartupRequirement::NotWindows
        );
        Ok(())
    }

    #[test]
    fn setup_marker_requires_current_version_and_backend() {
        assert!(WindowsSandboxSetupMarker::current().is_current());
        assert!(
            !WindowsSandboxSetupMarker {
                version: SETUP_VERSION + 1,
                backend: SETUP_BACKEND.to_string(),
                offline_username: None,
                online_username: None,
                proxy_ports: Vec::new(),
                allow_local_binding: false,
                created_at: None,
            }
            .is_current()
        );
        assert!(
            !WindowsSandboxSetupMarker {
                version: SETUP_VERSION,
                backend: "restricted_token".to_string(),
                offline_username: None,
                online_username: None,
                proxy_ports: Vec::new(),
                allow_local_binding: false,
                created_at: None,
            }
            .is_current()
        );
    }

    #[test]
    fn setup_marker_path_is_under_duckagent_sandbox_windows() {
        let path = setup_marker_path_at(Path::new("C:/Users/alice/.duckagent"));
        assert!(
            path.to_string_lossy()
                .replace('\\', "/")
                .ends_with(".duckagent/sandbox/windows/setup_marker.json")
        );
        let users_path = setup_users_path_at(Path::new("C:/Users/alice/.duckagent"));
        assert!(
            users_path
                .to_string_lossy()
                .replace('\\', "/")
                .ends_with(".duckagent/sandbox/windows/sandbox_users.json")
        );
    }

    #[test]
    fn workspace_setup_support_requires_proxy_marker_port() -> Result<()> {
        let root = tempfile::tempdir()?;
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        write_test_setup_state(root.path(), Vec::new())?;

        assert!(setup_is_complete_at(root.path()));
        assert!(!setup_supports_sandbox_at(root.path(), &sandbox));

        write_test_setup_state(root.path(), vec![48123])?;
        assert!(setup_supports_sandbox_at(root.path(), &sandbox));
        Ok(())
    }

    #[test]
    fn readonly_setup_support_rejects_proxy_marker_port() -> Result<()> {
        let root = tempfile::tempdir()?;
        let sandbox = SandboxConfig::default().resolve(Some("readonly"))?;
        write_test_setup_state(root.path(), vec![48123])?;

        assert!(setup_is_complete_at(root.path()));
        assert!(!setup_supports_sandbox_at(root.path(), &sandbox));

        write_test_setup_state(root.path(), Vec::new())?;
        assert!(setup_supports_sandbox_at(root.path(), &sandbox));
        Ok(())
    }

    #[test]
    fn setup_required_error_mentions_elevated_setup() {
        let message = setup_required_error_message("readonly", false, &[], false);
        assert!(message.contains("elevated setup"));
        assert!(message.contains("readonly"));
        assert!(message.contains("duck sandbox setup-windows"));
    }

    #[test]
    fn marker_proxy_port_for_sandbox_returns_stable_setup_port() -> Result<()> {
        let root = tempfile::tempdir()?;
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        write_test_setup_state(root.path(), vec![49234])?;

        assert_eq!(
            marker_proxy_port_for_sandbox_at(root.path(), &sandbox)?,
            Some(49234)
        );
        Ok(())
    }

    fn write_test_setup_state(duckagent_home: &Path, proxy_ports: Vec<u16>) -> Result<()> {
        let dir = setup_marker_path_at(duckagent_home)
            .parent()
            .expect("setup marker has a parent")
            .to_path_buf();
        fs::create_dir_all(&dir)?;
        let mut marker = WindowsSandboxSetupMarker::current();
        marker.proxy_ports = proxy_ports;
        fs::write(
            setup_marker_path_at(duckagent_home),
            serde_json::to_vec_pretty(&marker)?,
        )?;
        fs::write(setup_users_path_at(duckagent_home), b"test users")?;
        Ok(())
    }
}
