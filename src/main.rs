mod agent;
mod approval;
mod audit;
mod auth;
mod capabilities;
mod character_card;
mod client;
mod context_projection;
mod cron;
mod gateway;
mod input;
mod instructions;
mod markdown;
mod mcp;
mod memory;
mod model;
mod model_config;
mod model_manager;
mod process_manager;
mod profile_manager;
mod profiles;
mod provider;
mod sandbox;
mod session;
mod session_control;
mod setup;
mod skills;
mod tools;
mod tui;
mod utils;
mod web;

use crate::agent::AgentRuntime;
use crate::client::ModelClient;
use crate::provider::{
    ApiMode, RuntimeOverride, fetch_provider_models, get_model_capabilities,
    refresh_models_dev_cache_background, resolve_runtime_context_window, resolve_runtime_provider,
    save_global_runtime_config,
};
use crate::session::SessionManager;
use crate::setup::{
    is_runtime_setup_cancelled, run_initial_runtime_setup,
    run_windows_sandbox_setup_after_provider_if_needed,
};
use crate::tui::ChatUi;
use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::process;
const SYSTEM_PROMPT: &str = include_str!("prompts/system-prompt.md");

#[derive(Debug, Parser)]
#[command(name = "duck", version)]
struct Cli {
    /// Use a named profile for this duck process.
    #[arg(long, global = true)]
    profile: Option<String>,
    /// Override the active sandbox preset for this duck process.
    #[arg(long, global = true)]
    sandbox: Option<String>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Select or manage profiles.
    #[command(name = "profiles", alias = "profile")]
    Profiles,
    /// Manage saved models.
    Model,
    Session(SessionCommand),
    Runtime(RuntimeCommand),
    Gateway(gateway::GatewayCommand),
    Mcp(mcp::cli::McpCommand),
    Sandbox(sandbox::cli::SandboxCommand),
    #[command(name = "__sandbox-run", hide = true)]
    SandboxRun(sandbox::runner::SandboxRunCommand),
    #[command(name = "__sandbox-linux-inner", hide = true)]
    SandboxLinuxInner(sandbox::runner::SandboxLinuxInnerCommand),
    #[command(name = "__sandbox-windows-setup-helper", hide = true)]
    SandboxWindowsSetupHelper(SandboxWindowsSetupHelperCommand),
    #[command(name = "__process-supervisor", hide = true)]
    ProcessSupervisor(process_manager::ProcessSupervisorCommand),
}

#[derive(Debug, Args)]
struct SandboxWindowsSetupHelperCommand {
    #[arg(long)]
    duckagent_home: PathBuf,
    #[arg(long, default_value_t = false)]
    proxy: bool,
    #[arg(long = "proxy-port")]
    proxy_ports: Vec<u16>,
    #[arg(long, default_value_t = false)]
    allow_local_binding: bool,
}

#[derive(Debug, Args)]
struct SessionCommand {
    #[command(subcommand)]
    command: SessionSubcommand,
}

#[derive(Debug, Subcommand)]
enum SessionSubcommand {
    Compact {
        #[arg(long)]
        id: String,
        #[arg(long)]
        json: String,
    },
    GetAllMessages {
        #[arg(long)]
        id: String,
    },
    SetRuntime {
        #[arg(long)]
        id: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        api_mode: Option<String>,
    },
    ShowRuntime {
        #[arg(long)]
        id: String,
    },
}

#[derive(Debug, Args)]
struct RuntimeCommand {
    #[command(subcommand)]
    command: RuntimeSubcommand,
}

#[derive(Debug, Subcommand)]
enum RuntimeSubcommand {
    Resolve {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        api_mode: Option<String>,
    },
    ListModels {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        api_mode: Option<String>,
    },
    Capabilities {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        api_mode: Option<String>,
    },
}

fn main() {
    if let Err(err) = run() {
        if is_runtime_setup_cancelled(&err) {
            process::exit(0);
        }
        eprintln!("duck error: {err:#}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    profiles::set_cli_profile_override(cli.profile.clone())?;
    sandbox::set_cli_sandbox_override(cli.sandbox.clone());
    let _ = dotenvy::dotenv();

    if let Some(command) = cli.command {
        return run_cli_command(command);
    }

    profiles::pin_active_profile()?;
    let session_manager = SessionManager::new_default()?;
    refresh_models_dev_cache_background();
    let runtime = match resolve_runtime_provider(None, None) {
        Ok(runtime) => runtime,
        Err(_) => run_initial_runtime_setup()?,
    };
    run_windows_sandbox_setup_after_provider_if_needed()?;
    let client = ModelClient::from_runtime(runtime.clone())?;
    let session_id = session_manager.create_session_with_runtime_and_source(
        None,
        SYSTEM_PROMPT,
        runtime.session_config(),
        "tui",
    )?;
    let agent = AgentRuntime::new(client, session_manager)?;
    let mut ui = ChatUi::new(agent, session_id);
    ui.run()
}

fn run_cli_command(command: Commands) -> Result<()> {
    match command {
        Commands::Profiles => profile_manager::run_profile_manager()?,
        Commands::Model => {
            if let Some(runtime) = model_manager::run_model_manager()? {
                println!(
                    "Using model: {} / {}",
                    runtime.provider.as_str(),
                    runtime.model
                );
            }
        }
        Commands::Session(session_command) => match session_command.command {
            SessionSubcommand::Compact { id, json } => {
                let session_manager = SessionManager::new_default()?;
                session_manager.handle_cli_compact(&id, &json)?;
            }
            SessionSubcommand::GetAllMessages { id } => {
                let session_manager = SessionManager::new_default()?;
                let output = session_manager.handle_cli_get_all_messages(&id)?;
                println!("{output}");
            }
            SessionSubcommand::SetRuntime {
                id,
                provider,
                model,
                base_url,
                api_mode,
            } => {
                let session_manager = SessionManager::new_default()?;
                let override_config = runtime_override(provider, model, base_url, api_mode, None)?;
                let current = session_manager.get_runtime_config(&id)?;
                let runtime = resolve_runtime_provider(Some(&current), Some(&override_config))?;
                session_manager.update_runtime_config(&id, runtime.session_config())?;
                save_global_runtime_config(&runtime)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&runtime.session_config())?
                );
            }
            SessionSubcommand::ShowRuntime { id } => {
                let session_manager = SessionManager::new_default()?;
                let runtime = session_manager.get_runtime_config(&id)?;
                println!("{}", serde_json::to_string_pretty(&runtime)?);
            }
        },
        Commands::Runtime(runtime_command) => match runtime_command.command {
            RuntimeSubcommand::Resolve {
                provider,
                model,
                base_url,
                api_mode,
            } => {
                let override_config = runtime_override(provider, model, base_url, api_mode, None)?;
                let runtime = resolve_runtime_provider(None, Some(&override_config))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&runtime.session_config())?
                );
            }
            RuntimeSubcommand::ListModels {
                provider,
                model,
                base_url,
                api_mode,
            } => {
                let override_config = runtime_override(provider, model, base_url, api_mode, None)?;
                let runtime = resolve_runtime_provider(None, Some(&override_config))?;
                let models = fetch_provider_models(&runtime)?;
                println!("{}", serde_json::to_string_pretty(&models)?);
            }
            RuntimeSubcommand::Capabilities {
                provider,
                model,
                base_url,
                api_mode,
            } => {
                let override_config = runtime_override(provider, model, base_url, api_mode, None)?;
                let runtime = resolve_runtime_provider(None, Some(&override_config))?;
                let mut capabilities =
                    get_model_capabilities(runtime.provider.as_str(), &runtime.model)?
                        .unwrap_or_default();
                if capabilities.context_window.is_none() {
                    capabilities.context_window = Some(resolve_runtime_context_window(&runtime));
                }
                println!("{}", serde_json::to_string_pretty(&capabilities)?);
            }
        },
        Commands::Gateway(command) => {
            profiles::pin_active_profile()?;
            let session_manager = SessionManager::new_default()?;
            gateway::run(command, session_manager.clone(), SYSTEM_PROMPT)?;
        }
        Commands::Mcp(command) => {
            mcp::cli::run(command)?;
        }
        Commands::Sandbox(command) => {
            sandbox::cli::run(command)?;
        }
        Commands::SandboxRun(command) => {
            sandbox::runner::run_hidden_sandbox_command(command)?;
        }
        Commands::SandboxLinuxInner(command) => {
            sandbox::runner::run_hidden_linux_inner_command(command)?;
        }
        Commands::SandboxWindowsSetupHelper(command) => {
            sandbox::windows_setup::run_setup_helper(
                command.duckagent_home,
                command.proxy,
                command.proxy_ports,
                command.allow_local_binding,
            )?;
        }
        Commands::ProcessSupervisor(command) => {
            process_manager::run_process_supervisor(command)?;
        }
    }

    Ok(())
}

fn runtime_override(
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    api_mode: Option<String>,
    api_key: Option<String>,
) -> Result<RuntimeOverride> {
    let api_mode = api_mode
        .as_deref()
        .map(|value| {
            ApiMode::parse(value).ok_or_else(|| anyhow::anyhow!("unsupported api_mode: {value}"))
        })
        .transpose()?;

    Ok(RuntimeOverride {
        provider,
        model,
        base_url,
        api_mode,
        api_key,
    })
}

#[cfg(test)]
mod tests {
    use super::{Cli, Commands, SYSTEM_PROMPT};
    use clap::Parser;

    #[test]
    fn system_prompt_keeps_stable_agent_orchestration_policy() {
        assert!(SYSTEM_PROMPT.contains("local runtime with tool-calling support"));
        assert!(SYSTEM_PROMPT.contains("Profile, avatar card, memory"));
        assert!(SYSTEM_PROMPT.contains("Follow them unless they conflict"));
        assert!(SYSTEM_PROMPT.contains("Do not expose hidden reasoning"));
        assert!(SYSTEM_PROMPT.contains("Never invent live local facts"));
        assert!(SYSTEM_PROMPT.contains("Your only native tool is `call_capability`"));
        assert!(SYSTEM_PROMPT.contains("Available MainAgent capabilities"));
        assert!(SYSTEM_PROMPT.contains("call runtime capabilities directly"));
        assert!(SYSTEM_PROMPT.contains("Do not invent delegation or orchestration capabilities"));
        assert!(SYSTEM_PROMPT.contains("recover exact details with the listed"));
        assert!(!SYSTEM_PROMPT.to_lowercase().contains("duckagent"));
        assert!(!SYSTEM_PROMPT.contains("command-line AI agent"));
        assert!(!SYSTEM_PROMPT.contains("Your job is"));
        assert!(!SYSTEM_PROMPT.contains("Be practical"));
        assert!(!SYSTEM_PROMPT.contains("equal co-builder"));
        assert!(!SYSTEM_PROMPT.contains("name=\"load_skill\""));
        assert!(!SYSTEM_PROMPT.contains("return_mode"));
    }

    #[test]
    fn profiles_without_subcommand_opens_profile_picker() {
        let cli = Cli::try_parse_from(["duck", "profiles"]).expect("profiles command parses");
        match cli.command {
            Some(Commands::Profiles) => {}
            other => panic!("expected profiles command, got {other:?}"),
        }
    }

    #[test]
    fn profile_alias_opens_profile_picker() {
        let cli = Cli::try_parse_from(["duck", "profile"]).expect("profile alias parses");
        match cli.command {
            Some(Commands::Profiles) => {}
            other => panic!("expected profiles command, got {other:?}"),
        }
    }

    #[test]
    fn profiles_subcommands_are_not_supported() {
        assert!(Cli::try_parse_from(["duck", "profiles", "add", "work"]).is_err());
        assert!(Cli::try_parse_from(["duck", "profiles", "current"]).is_err());
        assert!(Cli::try_parse_from(["duck", "profiles", "init", "work"]).is_err());
        assert!(Cli::try_parse_from(["duck", "profiles", "list"]).is_err());
        assert!(Cli::try_parse_from(["duck", "profiles", "path"]).is_err());
        assert!(Cli::try_parse_from(["duck", "profiles", "use", "work"]).is_err());
    }
}
