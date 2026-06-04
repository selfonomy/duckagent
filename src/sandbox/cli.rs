use crate::sandbox::config::{ResolvedSandbox, load_sandbox_config, resolve_sandbox};
use crate::sandbox::runner::{check_platform_capability, network_status};
use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::json;

#[derive(Debug, Args)]
pub struct SandboxCommand {
    #[command(subcommand)]
    pub command: SandboxSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum SandboxSubcommand {
    List,
    Get {
        #[arg(default_value = "")]
        name: String,
    },
    Check {
        #[arg(default_value = "")]
        name: String,
    },
    #[command(name = "setup-windows")]
    SetupWindows,
    #[command(name = "windows-setup-status")]
    WindowsSetupStatus,
}

pub fn run(command: SandboxCommand) -> Result<()> {
    match command.command {
        SandboxSubcommand::List => list_presets(),
        SandboxSubcommand::Get { name } => get_preset(name),
        SandboxSubcommand::Check { name } => check_preset(name),
        SandboxSubcommand::SetupWindows => setup_windows(),
        SandboxSubcommand::WindowsSetupStatus => windows_setup_status(),
    }
}

fn list_presets() -> Result<()> {
    let config = load_sandbox_config()?;
    let resolved = resolve_sandbox()?;
    let presets = config
        .presets
        .keys()
        .map(|name| {
            json!({
                "name": name,
                "active": *name == resolved.name
            })
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "active": resolved.name,
            "presets": presets
        }))?
    );
    Ok(())
}

fn get_preset(name: String) -> Result<()> {
    let config = load_sandbox_config()?;
    let resolved = resolve_named_or_active(&config, &name)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "name": resolved.name,
            "preset": resolved.preset
        }))?
    );
    Ok(())
}

fn check_preset(name: String) -> Result<()> {
    let config = load_sandbox_config()?;
    let resolved = resolve_named_or_active(&config, &name)?;
    let capability = check_platform_capability(&resolved);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "name": resolved.name,
            "platform": {
                "backend": capability.backend,
                "supported": capability.supported,
                "message": capability.message,
                "limitations": capability.limitations
            },
            "network": network_status(&resolved),
            "fail_closed": !capability.supported
        }))?
    );
    Ok(())
}

fn setup_windows() -> Result<()> {
    crate::sandbox::windows_setup::run_elevated_setup()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "platform": "windows",
            "setup_complete": crate::sandbox::windows_setup::setup_is_complete()
        }))?
    );
    Ok(())
}

fn windows_setup_status() -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "platform": {
                "windows": cfg!(target_os = "windows")
            },
            "setup_complete": crate::sandbox::windows_setup::setup_is_complete(),
            "marker_path": crate::sandbox::windows_setup::setup_marker_path()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        }))?
    );
    Ok(())
}

fn resolve_named_or_active(
    config: &crate::sandbox::config::SandboxConfig,
    name: &str,
) -> Result<ResolvedSandbox> {
    let selected = name.trim();
    if selected.is_empty() {
        resolve_sandbox()
    } else {
        config.resolve(Some(selected))
    }
}
