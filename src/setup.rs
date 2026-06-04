use crate::auth::{
    CodexDeviceLogin, CodexDevicePoll, NousDeviceLogin, NousDevicePoll,
    codex_cli_credentials_if_usable, codex_saved_credentials, copilot_acp_available,
    poll_nous_device_code_login, poll_openai_codex_device_code_login,
    save_codex_cli_shared_credentials, save_provider_credentials, start_nous_device_code_login,
    start_openai_codex_device_code_login,
};
use crate::input::InputState;
use crate::model_config::SavedModelInput;
use crate::provider::{
    ApiMode, ModelCapabilities, ProviderKind, RuntimeProvider, UNKNOWN_CONTEXT_WINDOW_FALLBACK,
    fetch_provider_model_catalog, get_cached_model_context_window,
    refresh_models_dev_cache_background, save_model_capabilities_override,
    setup_provider_descriptors,
};
use crate::web::{
    BrowserFallbackMode, WebConfig, WebExtractConfig, WebExtractProviderKind, WebSearchConfig,
    WebSearchProviderKind,
};
use anyhow::{Context, Result, anyhow};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::style::{Color, Print, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, disable_raw_mode, enable_raw_mode};
use crossterm::{ExecutableCommand, QueueableCommand};
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::sync::mpsc;
use std::time::Duration;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const SETUP_CONTAINER_MAX_WIDTH_COLS: usize = 100;
const SETUP_CARD_TARGET_HEIGHT: usize = 34;
const SETUP_CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(700);
pub(crate) const SETUP_QR_DENSE_ROW_PREFIX: &str = "__DUCKAGENT_QR_DENSE_ROW__:";

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupFlowKind {
    Provider,
    Gateway,
    ModelManager,
    ProfileManager,
}

#[derive(Clone, Copy)]
pub(crate) struct SetupFlow {
    kind: SetupFlowKind,
    title: &'static str,
    steps: &'static [&'static str],
    first_picker_title: &'static str,
}

const PROVIDER_SETUP_FLOW: SetupFlow = SetupFlow {
    kind: SetupFlowKind::Provider,
    title: "DuckAgent Setup",
    steps: &["1. Select provider", "2. Auth", "3. Select model", "4. Web"],
    first_picker_title: "Provider",
};

pub(crate) const GATEWAY_SETUP_FLOW: SetupFlow = SetupFlow {
    kind: SetupFlowKind::Gateway,
    title: "DuckAgent Gateway Setup",
    steps: &["1. Select channel", "2. Configure", "3. Review"],
    first_picker_title: "Channel",
};

const MODEL_MANAGER_FLOW: SetupFlow = SetupFlow {
    kind: SetupFlowKind::ModelManager,
    title: "DuckAgent Models",
    steps: &["1. Select model", "2. Configure", "3. Use"],
    first_picker_title: "Model",
};

pub(crate) const PROFILE_MANAGER_FLOW: SetupFlow = SetupFlow {
    kind: SetupFlowKind::ProfileManager,
    title: "DuckAgent Profiles",
    steps: &["1. Select profile", "2. Use"],
    first_picker_title: "Profile",
};

#[derive(Clone, Copy)]
struct SetupTheme {
    app_bg: Color,
    card_bg: Color,
    fg: Color,
    muted_fg: Color,
    accent: Color,
    select_bg: Color,
    select_fg: Color,
    border_fg: Color,
}

const SETUP_THEME: SetupTheme = SetupTheme {
    app_bg: Color::Rgb {
        r: 40,
        g: 40,
        b: 40,
    },
    card_bg: Color::Rgb {
        r: 50,
        g: 48,
        b: 47,
    },
    fg: Color::Rgb {
        r: 235,
        g: 219,
        b: 178,
    },
    muted_fg: Color::Rgb {
        r: 168,
        g: 153,
        b: 132,
    },
    accent: Color::Rgb {
        r: 215,
        g: 153,
        b: 33,
    },
    select_bg: Color::Rgb {
        r: 80,
        g: 73,
        b: 69,
    },
    select_fg: Color::Rgb {
        r: 251,
        g: 241,
        b: 199,
    },
    border_fg: Color::Rgb {
        r: 102,
        g: 92,
        b: 84,
    },
};

#[derive(Debug)]
pub struct RuntimeSetupCancelled;

impl fmt::Display for RuntimeSetupCancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("runtime setup cancelled")
    }
}

impl Error for RuntimeSetupCancelled {}

pub fn is_runtime_setup_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RuntimeSetupCancelled>().is_some()
}

fn setup_cancelled_error() -> anyhow::Error {
    RuntimeSetupCancelled.into()
}

pub fn run_initial_runtime_setup() -> Result<RuntimeProvider> {
    run_saved_model_setup()
}

pub(crate) fn run_saved_model_setup() -> Result<RuntimeProvider> {
    match run_saved_model_setup_with_back(false)? {
        SetupAction::Submit(runtime) => Ok(runtime),
        SetupAction::Back => Err(setup_cancelled_error()),
    }
}

pub(crate) fn run_saved_model_setup_with_back(
    allow_back: bool,
) -> Result<SetupAction<RuntimeProvider>> {
    let input = match run_saved_model_input_setup_with_back(allow_back)? {
        SetupAction::Submit(input) => input,
        SetupAction::Back => return Ok(SetupAction::Back),
    };
    let runtime = crate::model_config::add_and_activate_saved_model(input)?;
    run_web_setup_after_model()?;
    Ok(SetupAction::Submit(runtime))
}

fn run_saved_model_input_setup_with_back(allow_back: bool) -> Result<SetupAction<SavedModelInput>> {
    refresh_models_dev_cache_background();
    'provider: loop {
        let provider = match prompt_provider_with_back(allow_back)? {
            SetupAction::Submit(provider) => provider,
            SetupAction::Back => return Ok(SetupAction::Back),
        };
        'base_url: loop {
            let base_url = match prompt_base_url(provider)? {
                SetupAction::Submit(value) => value,
                SetupAction::Back => continue 'provider,
            };
            'api_mode: loop {
                let api_mode = match prompt_api_mode(provider, base_url.as_deref())? {
                    SetupAction::Submit(value) => value,
                    SetupAction::Back => continue 'base_url,
                };
                let mut model_prefetch =
                    build_prefetch_runtime(provider, &base_url, api_mode, None)
                        .filter(|_| can_prefetch_models_without_api_key(provider))
                        .map(start_model_prefetch);
                'api_key: loop {
                    let api_key = match prompt_api_key(provider)? {
                        SetupAction::Submit(value) => value,
                        SetupAction::Back => {
                            if provider_has_api_mode_step(provider) {
                                continue 'api_mode;
                            }
                            if provider_has_base_url_step(provider) {
                                continue 'base_url;
                            }
                            continue 'provider;
                        }
                    };
                    match prepare_special_provider_auth(provider)? {
                        SetupAction::Submit(()) => {}
                        SetupAction::Back => {
                            if provider_has_api_key_step(provider) {
                                continue 'api_key;
                            }
                            if provider_has_api_mode_step(provider) {
                                continue 'api_mode;
                            }
                            if provider_has_base_url_step(provider) {
                                continue 'base_url;
                            }
                            continue 'provider;
                        }
                    }

                    let temp_runtime =
                        build_prefetch_runtime(provider, &base_url, api_mode, api_key.as_deref())
                            .ok_or_else(|| {
                            anyhow!("provider {} requires a base URL", provider.as_str())
                        })?;
                    if model_prefetch.is_none() {
                        model_prefetch = Some(start_model_prefetch(temp_runtime.clone()));
                    }

                    'model: loop {
                        let model = match prompt_model(&temp_runtime, model_prefetch.take())? {
                            SetupAction::Submit(value) => value,
                            SetupAction::Back => {
                                if provider_has_api_key_step(provider) {
                                    continue 'api_key;
                                }
                                if provider_has_api_mode_step(provider) {
                                    continue 'api_mode;
                                }
                                if provider_has_base_url_step(provider) {
                                    continue 'base_url;
                                }
                                continue 'provider;
                            }
                        };
                        let detected_context_window =
                            get_cached_model_context_window(provider.as_str(), &model);
                        let context_window =
                            match prompt_context_window(provider, &model, detected_context_window)?
                            {
                                SetupAction::Submit(value) => value,
                                SetupAction::Back => continue 'model,
                            };
                        if let Some(context_window) = context_window {
                            save_model_capabilities_override(
                                provider.as_str(),
                                &model,
                                ModelCapabilities {
                                    context_window: Some(context_window),
                                    ..ModelCapabilities::default()
                                },
                            )?;
                        }
                        return Ok(SetupAction::Submit(SavedModelInput {
                            provider,
                            model,
                            base_url,
                            api_mode,
                            api_key,
                        }));
                    }
                }
            }
        }
    }
}

pub fn run_windows_sandbox_setup_after_provider_if_needed() -> Result<()> {
    #[cfg(not(target_os = "windows"))]
    {
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        if !crate::sandbox::windows_setup::startup_prompt_is_needed()? {
            return Ok(());
        }

        match prompt_windows_sandbox_setup_choice()? {
            WindowsSandboxSetupChoice::SetupDefault => {
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(crate::sandbox::windows_setup::run_elevated_setup());
                });
                wait_setup_task(
                    "Windows sandbox",
                    "Launching setup helper. Approve the Administrator prompt if Windows asks.",
                    rx,
                )?;
                if crate::sandbox::windows_setup::startup_prompt_is_needed()? {
                    return Err(anyhow!(
                        "Windows sandbox setup did not complete; restart DuckAgent and choose setup again, or choose danger to run without sandbox"
                    ));
                }
                Ok(())
            }
            WindowsSandboxSetupChoice::RunDanger => {
                crate::sandbox::windows_setup::activate_danger_for_current_process()?;
                Ok(())
            }
            WindowsSandboxSetupChoice::Quit => Err(setup_cancelled_error()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
enum WindowsSandboxSetupChoice {
    SetupDefault,
    RunDanger,
    Quit,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn prompt_windows_sandbox_setup_choice() -> Result<WindowsSandboxSetupChoice> {
    let items = windows_sandbox_setup_picker_items();
    match run_picker(
        "Windows sandbox",
        "Set up the DuckAgent sandbox to protect files and control network access.",
        &items,
        false,
    )? {
        SetupAction::Submit(0) => Ok(WindowsSandboxSetupChoice::SetupDefault),
        SetupAction::Submit(1) => Ok(WindowsSandboxSetupChoice::RunDanger),
        SetupAction::Submit(2) => Ok(WindowsSandboxSetupChoice::Quit),
        SetupAction::Submit(_) => Err(anyhow!("invalid Windows sandbox setup choice")),
        SetupAction::Back => Err(setup_cancelled_error()),
    }
}

fn windows_sandbox_setup_picker_items() -> Vec<PickerItem> {
    vec![
        PickerItem {
            title: "Set up default sandbox (requires Administrator permissions)".to_string(),
            detail: "Creates the Windows sandbox users, ACLs, and network rules.".to_string(),
            model_columns: None,
        },
        PickerItem {
            title: "Run without sandbox (danger)".to_string(),
            detail: "Disables OS sandboxing and allows direct filesystem/network access."
                .to_string(),
            model_columns: None,
        },
        PickerItem {
            title: "Quit".to_string(),
            detail: "Exit without changing sandbox settings.".to_string(),
            model_columns: None,
        },
    ]
}

struct ModelPrefetch {
    rx: mpsc::Receiver<Result<(Vec<String>, Vec<PickerItem>)>>,
}

fn build_prefetch_runtime(
    provider: ProviderKind,
    base_url: &Option<String>,
    api_mode: Option<ApiMode>,
    api_key: Option<&str>,
) -> Option<RuntimeProvider> {
    Some(RuntimeProvider {
        model_id: None,
        provider,
        model: String::new(),
        base_url: base_url
            .clone()
            .or_else(|| provider.default_base_url().map(str::to_string))?,
        api_key: api_key.unwrap_or_default().to_string(),
        api_mode: api_mode.unwrap_or_else(|| provider.default_api_mode()),
        source: "setup".to_string(),
        account_id: None,
    })
}

fn can_prefetch_models_without_api_key(provider: ProviderKind) -> bool {
    matches!(provider, ProviderKind::OpenRouter) || !provider_has_api_key_step(provider)
}

fn start_model_prefetch(runtime: RuntimeProvider) -> ModelPrefetch {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(build_model_picker_items(&runtime));
    });
    ModelPrefetch { rx }
}

fn provider_has_base_url_step(provider: ProviderKind) -> bool {
    matches!(
        provider,
        ProviderKind::Bedrock | ProviderKind::Custom | ProviderKind::AzureFoundry
    )
}

fn provider_has_api_mode_step(provider: ProviderKind) -> bool {
    matches!(provider, ProviderKind::Custom | ProviderKind::AzureFoundry)
}

fn provider_has_api_key_step(provider: ProviderKind) -> bool {
    (!matches!(
        provider,
        ProviderKind::OpenAiCodex
            | ProviderKind::Nous
            | ProviderKind::QwenOauth
            | ProviderKind::GoogleGeminiCli
            | ProviderKind::Bedrock
            | ProviderKind::CopilotAcp
    ) && provider.requires_secret())
        || matches!(provider, ProviderKind::Custom | ProviderKind::AzureFoundry)
}

fn prompt_provider_with_back(allow_back: bool) -> Result<SetupAction<ProviderKind>> {
    let providers = setup_provider_descriptors();
    if providers.is_empty() {
        return Err(anyhow!("no fully wired providers are available"));
    }
    let items = providers
        .iter()
        .map(|provider| PickerItem {
            title: provider.name.to_string(),
            detail: provider.description.to_string(),
            model_columns: None,
        })
        .collect::<Vec<_>>();
    let index = match run_picker(
        "Select provider",
        "Type to filter providers. Choose the runtime entrance first.",
        &items,
        allow_back,
    )? {
        SetupAction::Submit(index) => index,
        SetupAction::Back => return Ok(SetupAction::Back),
    };
    Ok(SetupAction::Submit(providers[index].provider))
}

fn prompt_base_url(provider: ProviderKind) -> Result<SetupAction<Option<String>>> {
    if provider == ProviderKind::Bedrock {
        let region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_string());
        let input = match prompt_text(
            "AWS region",
            &format!("Default: {region}. Press Enter to accept it."),
            "",
            Some(&region),
            false,
            true,
            false,
        )? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return Ok(SetupAction::Back),
        };
        let region = if input.trim().is_empty() {
            region
        } else {
            input
        };
        return Ok(SetupAction::Submit(Some(format!(
            "https://bedrock-runtime.{}.amazonaws.com",
            region.trim()
        ))));
    }
    if provider == ProviderKind::CopilotAcp {
        return Ok(SetupAction::Submit(Some("acp://copilot".to_string())));
    }
    if matches!(provider, ProviderKind::Custom | ProviderKind::AzureFoundry) {
        let input = match prompt_text(
            "Base URL",
            "Enter the endpoint for this provider.",
            "",
            None,
            true,
            true,
            false,
        )? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return Ok(SetupAction::Back),
        };
        return Ok(SetupAction::Submit(Some(input)));
    }

    Ok(SetupAction::Submit(None))
}

fn prompt_api_mode(
    provider: ProviderKind,
    base_url: Option<&str>,
) -> Result<SetupAction<Option<ApiMode>>> {
    if !matches!(provider, ProviderKind::Custom | ProviderKind::AzureFoundry) {
        return Ok(SetupAction::Submit(None));
    }
    let inferred = base_url.and_then(|value| {
        if value.to_ascii_lowercase().contains("anthropic") {
            Some(ApiMode::AnthropicMessages)
        } else if value.to_ascii_lowercase().contains("codex") {
            Some(ApiMode::CodexResponses)
        } else {
            None
        }
    });
    let default = inferred.unwrap_or(ApiMode::ChatCompletions);
    loop {
        let input = match prompt_text(
            "API mode",
            &format!(
                "Use chat_completions, codex_responses, or anthropic_messages. Default: {}.",
                default.as_str()
            ),
            "",
            Some(default.as_str()),
            false,
            true,
            false,
        )? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return Ok(SetupAction::Back),
        };
        if input.is_empty() {
            return Ok(SetupAction::Submit(Some(default)));
        }
        if let Some(api_mode) = ApiMode::parse(&input) {
            return Ok(SetupAction::Submit(Some(api_mode)));
        }
        show_setup_message(
            "Invalid API mode",
            "Use chat_completions, codex_responses, or anthropic_messages.",
        )?;
    }
}

fn prompt_api_key(provider: ProviderKind) -> Result<SetupAction<Option<String>>> {
    if matches!(
        provider,
        ProviderKind::OpenAiCodex
            | ProviderKind::Nous
            | ProviderKind::QwenOauth
            | ProviderKind::GoogleGeminiCli
            | ProviderKind::Bedrock
            | ProviderKind::CopilotAcp
    ) {
        return Ok(SetupAction::Submit(None));
    }
    if matches!(provider, ProviderKind::Custom | ProviderKind::AzureFoundry) {
        let input = match prompt_text(
            "API key/token",
            "",
            &api_key_prompt_subtitle(provider),
            None,
            false,
            true,
            true,
        )? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return Ok(SetupAction::Back),
        };
        return Ok(SetupAction::Submit((!input.is_empty()).then_some(input)));
    }
    if !provider.requires_secret() {
        return Ok(SetupAction::Submit(None));
    }
    let input = match prompt_text(
        "API key/token",
        "",
        &api_key_prompt_subtitle(provider),
        None,
        true,
        true,
        true,
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return Ok(SetupAction::Back),
    };
    Ok(SetupAction::Submit(Some(input)))
}

fn api_key_prompt_subtitle(provider: ProviderKind) -> String {
    format!("Enter the {} API key or token", provider.display_name())
}

fn prepare_special_provider_auth(provider: ProviderKind) -> Result<SetupAction<()>> {
    match provider {
        ProviderKind::OpenAiCodex => {
            if codex_saved_credentials()?.is_some() {
                return Ok(SetupAction::Submit(()));
            }
            prepare_codex_auth()?;
        }
        ProviderKind::Nous => {
            if crate::auth::resolve_provider_credentials(provider, true).is_err() {
                let credentials = run_nous_device_code_login_prompt()?;
                save_provider_credentials(ProviderKind::Nous, credentials)?;
            }
        }
        ProviderKind::QwenOauth => {
            crate::auth::resolve_provider_credentials(provider, true).with_context(
                || "Qwen OAuth is not logged in. Run `qwen auth qwen-oauth` and retry setup.",
            )?;
        }
        ProviderKind::GoogleGeminiCli => {
            crate::auth::resolve_provider_credentials(provider, true).with_context(|| {
                "Google Gemini CLI OAuth is not logged in. Run Gemini CLI login and retry setup."
            })?;
        }
        ProviderKind::CopilotAcp => {
            if !copilot_acp_available() {
                return Err(anyhow!(
                    "Copilot ACP is unavailable. Install/login GitHub Copilot CLI and ensure `copilot --acp --stdio` works."
                ));
            }
        }
        _ => {}
    }
    Ok(SetupAction::Submit(()))
}

fn prepare_codex_auth() -> Result<SetupAction<()>> {
    if let Some(credentials) = codex_cli_credentials_if_usable()? {
        let items = vec![
            PickerItem {
                title: "Use existing Codex login".to_string(),
                detail: "Detected a local Codex login; share it with duckagent".to_string(),
                model_columns: None,
            },
            PickerItem {
                title: "Sign in separately".to_string(),
                detail: "Create a separate duckagent Codex login".to_string(),
                model_columns: None,
            },
        ];
        let choice = match run_picker("Codex auth", "", &items, true)? {
            SetupAction::Submit(index) => index,
            SetupAction::Back => return Ok(SetupAction::Back),
        };
        if choice == 0 {
            save_codex_cli_shared_credentials(credentials)?;
            return Ok(SetupAction::Submit(()));
        }
    }

    let credentials = run_codex_device_code_login_prompt()?;
    save_provider_credentials(ProviderKind::OpenAiCodex, credentials)?;
    Ok(SetupAction::Submit(()))
}

fn run_codex_device_code_login_prompt() -> Result<crate::auth::ProviderCredentials> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(start_openai_codex_device_code_login());
    });
    let login = wait_setup_task("Starting Codex login", "", rx)?;
    wait_codex_device_approval(login)
}

fn wait_codex_device_approval(login: CodexDeviceLogin) -> Result<crate::auth::ProviderCredentials> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn({
        let login = login.clone();
        move || {
            let mut interval = login.interval_seconds.max(1);
            loop {
                std::thread::sleep(Duration::from_secs(interval));
                match poll_openai_codex_device_code_login(&login) {
                    Ok(CodexDevicePoll::Complete(credentials)) => {
                        let _ = tx.send(Ok(credentials));
                        break;
                    }
                    Ok(CodexDevicePoll::SlowDown) => {
                        interval = (interval + 1).min(30);
                    }
                    Ok(CodexDevicePoll::Pending) => {}
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        break;
                    }
                }
            }
        }
    });

    wait_codex_device_task(&login, rx)
}

fn run_nous_device_code_login_prompt() -> Result<crate::auth::ProviderCredentials> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(start_nous_device_code_login());
    });
    let login = wait_setup_task("Starting Nous login", "", rx)?;
    wait_nous_device_approval(login)
}

fn wait_nous_device_approval(login: NousDeviceLogin) -> Result<crate::auth::ProviderCredentials> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn({
        let login = login.clone();
        move || {
            let mut interval = login.interval_seconds.max(1);
            loop {
                std::thread::sleep(Duration::from_secs(interval));
                match poll_nous_device_code_login(&login) {
                    Ok(NousDevicePoll::Complete(credentials)) => {
                        let _ = tx.send(Ok(credentials));
                        break;
                    }
                    Ok(NousDevicePoll::SlowDown) => {
                        interval = (interval + 1).min(30);
                    }
                    Ok(NousDevicePoll::Pending) => {}
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        break;
                    }
                }
            }
        }
    });

    wait_nous_device_task(&login, rx)
}

fn prompt_model(
    runtime: &RuntimeProvider,
    prefetch: Option<ModelPrefetch>,
) -> Result<SetupAction<String>> {
    let (models, items) = match fetch_model_picker_items_with_loading(runtime, prefetch) {
        Ok(result) => result,
        Err(error) if is_runtime_setup_cancelled(&error) => return Err(error),
        Err(_) => {
            let items = vec![PickerItem {
                title: "Manual input...".to_string(),
                detail: String::new(),
                model_columns: Some(ModelPickerColumns::unknown()),
            }];
            (Vec::new(), items)
        }
    };

    let index = match run_picker(
        "Select model",
        &format!(
            "Provider: {}. Type to filter models; numbers are filter text too.",
            runtime.provider.as_str()
        ),
        &items,
        true,
    )? {
        SetupAction::Submit(index) => index,
        SetupAction::Back => return Ok(SetupAction::Back),
    };
    if index < models.len() {
        Ok(SetupAction::Submit(models[index].clone()))
    } else {
        prompt_text(
            "Model id",
            "Enter the exact model id to use for this provider.",
            "",
            None,
            true,
            true,
            false,
        )
    }
}

fn prompt_context_window(
    provider: ProviderKind,
    model: &str,
    detected: Option<u64>,
) -> Result<SetupAction<Option<u64>>> {
    let detected_text = detected
        .map(format_compact_u64)
        .unwrap_or_else(|| "unknown".to_string());
    loop {
        let input = match prompt_text(
            "Context window",
            &format!(
                "Optional for {} / {}. Detected: {}. Empty uses detected metadata, or {} if unknown.",
                provider.as_str(),
                model,
                detected_text,
                format_compact_u64(UNKNOWN_CONTEXT_WINDOW_FALLBACK)
            ),
            "optional, e.g. 32k, 128k, 1m",
            None,
            false,
            true,
            false,
        )? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return Ok(SetupAction::Back),
        };
        if input.trim().is_empty() {
            return Ok(SetupAction::Submit(None));
        }
        match parse_context_window_input(&input) {
            Some(value) => return Ok(SetupAction::Submit(Some(value))),
            None => show_setup_message(
                "Invalid context window",
                "Use a positive token count such as 32768, 32k, 128k, or 1m.",
            )?,
        }
    }
}

fn run_web_setup_after_model() -> Result<()> {
    let search_provider = match prompt_web_search_provider()? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return Ok(()),
    };
    if search_provider == WebSearchProviderKind::Exa {
        let api_key = match prompt_text(
            "Web Search API key",
            "Optional Exa key. Empty uses Exa MCP free mode.",
            "optional",
            None,
            false,
            true,
            true,
        )? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => String::new(),
        };
        if !api_key.trim().is_empty() {
            crate::auth::save_web_credentials("exa", api_key)?;
        }
    }

    let extract_provider = match prompt_web_extract_provider()? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => WebExtractProviderKind::Local,
    };
    let browser_fallback = if extract_provider == WebExtractProviderKind::Local {
        match prompt_local_browser_fallback()? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => BrowserFallbackMode::Auto,
        }
    } else {
        BrowserFallbackMode::Auto
    };

    crate::web::save_web_config(WebConfig {
        search: WebSearchConfig {
            provider: search_provider,
        },
        extract: WebExtractConfig {
            provider: extract_provider,
            browser_fallback,
        },
    })
}

fn prompt_web_search_provider() -> Result<SetupAction<WebSearchProviderKind>> {
    let items = vec![
        PickerItem {
            title: "Exa".to_string(),
            detail: "Recommended web search through duckagent's internal Exa MCP backend."
                .to_string(),
            model_columns: None,
        },
        PickerItem {
            title: "Disabled".to_string(),
            detail: "Do not expose web_search to the agent.".to_string(),
            model_columns: None,
        },
    ];
    match run_picker(
        "Web Search",
        "Choose the default web search provider.",
        &items,
        true,
    )? {
        SetupAction::Submit(0) => Ok(SetupAction::Submit(WebSearchProviderKind::Exa)),
        SetupAction::Submit(1) => Ok(SetupAction::Submit(WebSearchProviderKind::Disabled)),
        SetupAction::Submit(_) => Err(anyhow!("invalid web search provider choice")),
        SetupAction::Back => Ok(SetupAction::Back),
    }
}

fn prompt_web_extract_provider() -> Result<SetupAction<WebExtractProviderKind>> {
    let items = vec![
        PickerItem {
            title: "Local".to_string(),
            detail: "HTTP extract with automatic local browser fallback for JS pages.".to_string(),
            model_columns: None,
        },
        PickerItem {
            title: "Exa".to_string(),
            detail: "Use Exa MCP web_fetch_exa for full-page markdown extraction.".to_string(),
            model_columns: None,
        },
        PickerItem {
            title: "Disabled".to_string(),
            detail: "Do not expose web_extract to the agent.".to_string(),
            model_columns: None,
        },
    ];
    match run_picker(
        "Web Extract",
        "Choose the default webpage extraction provider.",
        &items,
        true,
    )? {
        SetupAction::Submit(0) => Ok(SetupAction::Submit(WebExtractProviderKind::Local)),
        SetupAction::Submit(1) => Ok(SetupAction::Submit(WebExtractProviderKind::Exa)),
        SetupAction::Submit(2) => Ok(SetupAction::Submit(WebExtractProviderKind::Disabled)),
        SetupAction::Submit(_) => Err(anyhow!("invalid web extract provider choice")),
        SetupAction::Back => Ok(SetupAction::Back),
    }
}

fn prompt_local_browser_fallback() -> Result<SetupAction<BrowserFallbackMode>> {
    let items = vec![
        PickerItem {
            title: "Auto".to_string(),
            detail: "When HTTP extraction looks like a JS shell, try local Chrome automatically."
                .to_string(),
            model_columns: None,
        },
        PickerItem {
            title: "Off".to_string(),
            detail: "Use HTTP extraction only.".to_string(),
            model_columns: None,
        },
    ];
    match run_picker(
        "Local browser fallback",
        "Configure Local web_extract JavaScript fallback.",
        &items,
        true,
    )? {
        SetupAction::Submit(0) => Ok(SetupAction::Submit(BrowserFallbackMode::Auto)),
        SetupAction::Submit(1) => Ok(SetupAction::Submit(BrowserFallbackMode::Off)),
        SetupAction::Submit(_) => Err(anyhow!("invalid browser fallback choice")),
        SetupAction::Back => Ok(SetupAction::Back),
    }
}

fn parse_context_window_input(input: &str) -> Option<u64> {
    let normalized = input.trim().replace(['_', ','], "").to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }
    let (number, multiplier) = match normalized.chars().last()? {
        'k' => (&normalized[..normalized.len().saturating_sub(1)], 1_000u64),
        'm' => (
            &normalized[..normalized.len().saturating_sub(1)],
            1_000_000u64,
        ),
        _ => (normalized.as_str(), 1u64),
    };
    if number.is_empty() || !number.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let value = number.parse::<u64>().ok()?.checked_mul(multiplier)?;
    (value > 0).then_some(value)
}

fn fetch_model_picker_items_with_loading(
    runtime: &RuntimeProvider,
    prefetch: Option<ModelPrefetch>,
) -> Result<(Vec<String>, Vec<PickerItem>)> {
    let subtitle = format!("Provider: {}", runtime.provider.as_str());
    match prefetch {
        Some(prefetch) => wait_setup_task("Fetching models", &subtitle, prefetch.rx),
        None => {
            let runtime = runtime.clone();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let result = build_model_picker_items(&runtime);
                let _ = tx.send(result);
            });
            wait_setup_task("Fetching models", &subtitle, rx)
        }
    }
}

fn build_model_picker_items(runtime: &RuntimeProvider) -> Result<(Vec<String>, Vec<PickerItem>)> {
    let catalog = fetch_provider_model_catalog(runtime)?;
    let models = catalog.keys().cloned().collect::<Vec<_>>();
    let mut items = models
        .iter()
        .map(|model| PickerItem {
            title: model.clone(),
            detail: String::new(),
            model_columns: Some(model_columns(catalog.get(model))),
        })
        .collect::<Vec<_>>();
    items.push(PickerItem {
        title: "Manual input...".to_string(),
        detail: String::new(),
        model_columns: Some(ModelPickerColumns::unknown()),
    });
    Ok((models, items))
}

pub(crate) enum SetupAction<T> {
    Submit(T),
    Back,
}

pub(crate) enum PickerEditAction {
    Submit(usize),
    Delete(usize),
}

pub(crate) enum PickerManageAction {
    Submit(usize),
    Delete(usize),
    Back,
}

fn model_columns(capabilities: Option<&ModelCapabilities>) -> ModelPickerColumns {
    capabilities
        .map(format_model_columns)
        .unwrap_or_else(ModelPickerColumns::unknown)
}

fn format_model_columns(capabilities: &ModelCapabilities) -> ModelPickerColumns {
    ModelPickerColumns {
        input: format_cost(capabilities.input_cost),
        output: format_cost(capabilities.output_cost),
        context: capabilities
            .context_window
            .map(format_compact_u64)
            .unwrap_or_else(|| "-".to_string()),
    }
}

fn format_cost(cost: Option<f64>) -> String {
    match cost {
        Some(value) if value == 0.0 => "$0".to_string(),
        Some(value) if value >= 1.0 => format!("${value:.2}"),
        Some(value) => format!("${value:.4}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string(),
        None => "-".to_string(),
    }
}

fn format_compact_u64(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{}m", value / 1_000_000)
    } else if value >= 1_000 {
        format!("{}k", value / 1_000)
    } else {
        value.to_string()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PickerItem {
    pub(crate) title: String,
    pub(crate) detail: String,
    pub(crate) model_columns: Option<ModelPickerColumns>,
}

#[derive(Debug, Clone)]
pub(crate) struct ModelPickerColumns {
    input: String,
    output: String,
    context: String,
}

impl ModelPickerColumns {
    fn unknown() -> Self {
        Self {
            input: "-".to_string(),
            output: "-".to_string(),
            context: "-".to_string(),
        }
    }
}

pub(crate) fn run_picker(
    title: &str,
    subtitle: &str,
    items: &[PickerItem],
    allow_back: bool,
) -> Result<SetupAction<usize>> {
    run_picker_with_flow(PROVIDER_SETUP_FLOW, title, subtitle, items, allow_back)
}

pub(crate) fn run_model_manager_picker(
    title: &str,
    subtitle: &str,
    items: &[PickerItem],
) -> Result<PickerEditAction> {
    if items.is_empty() {
        return Err(anyhow!("picker has no items"));
    }

    let mut stdout = io::stdout();
    enable_raw_mode().context("failed to enable model manager picker raw mode")?;
    if let Err(error) = stdout.execute(Hide) {
        let _ = disable_raw_mode();
        return Err(error).context("failed to hide cursor for model manager picker");
    }

    let result = run_model_manager_picker_loop(&mut stdout, title, subtitle, items);

    let _ = stdout.execute(Show);
    let _ = stdout.execute(SetForegroundColor(Color::Reset));
    let _ = stdout.execute(SetBackgroundColor(Color::Reset));
    let _ = move_cursor_below_setup(&mut stdout);
    let _ = stdout.flush();
    disable_raw_mode().context("failed to disable model manager picker raw mode")?;

    result
}

fn run_model_manager_picker_loop(
    stdout: &mut io::Stdout,
    title: &str,
    subtitle: &str,
    items: &[PickerItem],
) -> Result<PickerEditAction> {
    let mut filter = InputState::new();
    let mut selected = 0usize;
    let mut cursor_visible = true;

    loop {
        let filtered = filtered_item_indices(items, &filter.text);
        if selected >= filtered.len() {
            selected = filtered.len().saturating_sub(1);
        }
        render_picker(
            stdout,
            MODEL_MANAGER_FLOW,
            title,
            subtitle,
            items,
            &filtered,
            &filter,
            selected,
            true,
            cursor_visible,
        )?;

        if !event::poll(SETUP_CURSOR_BLINK_INTERVAL)
            .context("failed to poll model manager picker key")?
        {
            cursor_visible = !cursor_visible;
            continue;
        }

        if let Event::Key(key) = event::read().context("failed to read model manager picker key")? {
            cursor_visible = true;
            match key.code {
                KeyCode::Esc => {
                    filter.clear();
                    selected = 0;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Err(setup_cancelled_error());
                }
                KeyCode::Enter => {
                    if let Some(index) = filtered.get(selected).copied() {
                        return Ok(PickerEditAction::Submit(index));
                    }
                }
                KeyCode::Delete => {
                    if filter.text.is_empty() {
                        if let Some(action) = model_manager_delete_selection(&filtered, selected) {
                            return Ok(action);
                        }
                    } else {
                        filter.delete_forward();
                        selected = 0;
                    }
                }
                KeyCode::Up => {
                    if !filtered.is_empty() {
                        selected = if selected == 0 {
                            filtered.len().saturating_sub(1)
                        } else {
                            selected.saturating_sub(1)
                        };
                    }
                }
                KeyCode::Down => {
                    if !filtered.is_empty() {
                        selected = (selected + 1) % filtered.len();
                    }
                }
                KeyCode::Backspace => {
                    if filter.text.is_empty() {
                        if let Some(action) = model_manager_delete_selection(&filtered, selected) {
                            return Ok(action);
                        }
                    } else {
                        filter.backspace();
                        selected = 0;
                    }
                }
                KeyCode::Left => filter.move_left(key.modifiers.contains(KeyModifiers::SHIFT)),
                KeyCode::Right => filter.move_right(key.modifiers.contains(KeyModifiers::SHIFT)),
                KeyCode::Char(c)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    filter.insert_char(c);
                    selected = 0;
                }
                _ => {}
            }
        }
    }
}

fn model_manager_delete_selection(filtered: &[usize], selected: usize) -> Option<PickerEditAction> {
    filtered
        .get(selected)
        .copied()
        .map(PickerEditAction::Delete)
}

pub(crate) fn run_picker_with_flow(
    flow: SetupFlow,
    title: &str,
    subtitle: &str,
    items: &[PickerItem],
    allow_back: bool,
) -> Result<SetupAction<usize>> {
    if items.is_empty() {
        return Err(anyhow!("picker has no items"));
    }

    let mut stdout = io::stdout();
    enable_raw_mode().context("failed to enable setup picker raw mode")?;
    if let Err(error) = stdout.execute(Hide) {
        let _ = disable_raw_mode();
        return Err(error).context("failed to hide cursor for setup picker");
    }

    let result = run_picker_loop(&mut stdout, flow, title, subtitle, items, allow_back);

    let _ = stdout.execute(Show);
    let _ = stdout.execute(SetForegroundColor(Color::Reset));
    let _ = stdout.execute(SetBackgroundColor(Color::Reset));
    let _ = move_cursor_below_setup(&mut stdout);
    let _ = stdout.flush();
    disable_raw_mode().context("failed to disable setup picker raw mode")?;

    result
}

pub(crate) fn run_edit_picker_with_flow(
    flow: SetupFlow,
    title: &str,
    subtitle: &str,
    items: &[PickerItem],
) -> Result<PickerManageAction> {
    if items.is_empty() {
        return Err(anyhow!("picker has no items"));
    }

    let mut stdout = io::stdout();
    enable_raw_mode().context("failed to enable setup edit picker raw mode")?;
    if let Err(error) = stdout.execute(Hide) {
        let _ = disable_raw_mode();
        return Err(error).context("failed to hide cursor for setup edit picker");
    }

    let result = run_edit_picker_loop(&mut stdout, flow, title, subtitle, items);

    let _ = stdout.execute(Show);
    let _ = stdout.execute(SetForegroundColor(Color::Reset));
    let _ = stdout.execute(SetBackgroundColor(Color::Reset));
    let _ = move_cursor_below_setup(&mut stdout);
    let _ = stdout.flush();
    disable_raw_mode().context("failed to disable setup edit picker raw mode")?;

    result
}

fn run_edit_picker_loop(
    stdout: &mut io::Stdout,
    flow: SetupFlow,
    title: &str,
    subtitle: &str,
    items: &[PickerItem],
) -> Result<PickerManageAction> {
    let mut filter = InputState::new();
    let mut selected = 0usize;
    let mut cursor_visible = true;

    loop {
        let filtered = filtered_item_indices(items, &filter.text);
        if selected >= filtered.len() {
            selected = filtered.len().saturating_sub(1);
        }
        render_picker(
            stdout,
            flow,
            title,
            subtitle,
            items,
            &filtered,
            &filter,
            selected,
            true,
            cursor_visible,
        )?;

        if !event::poll(SETUP_CURSOR_BLINK_INTERVAL)
            .context("failed to poll setup edit picker key")?
        {
            cursor_visible = !cursor_visible;
            continue;
        }

        if let Event::Key(key) = event::read().context("failed to read setup edit picker key")? {
            cursor_visible = true;
            match key.code {
                KeyCode::Esc => return Ok(PickerManageAction::Back),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Err(setup_cancelled_error());
                }
                KeyCode::Enter => {
                    if let Some(index) = filtered.get(selected).copied() {
                        return Ok(PickerManageAction::Submit(index));
                    }
                }
                KeyCode::Delete => {
                    if filter.text.is_empty() {
                        if let Some(index) = filtered.get(selected).copied() {
                            return Ok(PickerManageAction::Delete(index));
                        }
                    } else {
                        filter.delete_forward();
                        selected = 0;
                    }
                }
                KeyCode::Backspace => {
                    if filter.text.is_empty() {
                        if let Some(index) = filtered.get(selected).copied() {
                            return Ok(PickerManageAction::Delete(index));
                        }
                    } else {
                        filter.backspace();
                        selected = 0;
                    }
                }
                KeyCode::Up => {
                    if !filtered.is_empty() {
                        selected = if selected == 0 {
                            filtered.len().saturating_sub(1)
                        } else {
                            selected.saturating_sub(1)
                        };
                    }
                }
                KeyCode::Down => {
                    if !filtered.is_empty() {
                        selected = (selected + 1) % filtered.len();
                    }
                }
                KeyCode::Left if key.modifiers.is_empty() => {
                    return Ok(PickerManageAction::Back);
                }
                KeyCode::Left => filter.move_left(key.modifiers.contains(KeyModifiers::SHIFT)),
                KeyCode::Right => filter.move_right(key.modifiers.contains(KeyModifiers::SHIFT)),
                KeyCode::Char(c) => {
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                        filter.insert_char(c);
                    }
                    selected = 0;
                }
                _ => {}
            }
        }
    }
}

fn run_picker_loop(
    stdout: &mut io::Stdout,
    flow: SetupFlow,
    title: &str,
    subtitle: &str,
    items: &[PickerItem],
    allow_back: bool,
) -> Result<SetupAction<usize>> {
    let mut filter = InputState::new();
    let mut selected = 0usize;
    let mut cursor_visible = true;

    loop {
        let filtered = filtered_item_indices(items, &filter.text);
        if selected >= filtered.len() {
            selected = filtered.len().saturating_sub(1);
        }
        render_picker(
            stdout,
            flow,
            title,
            subtitle,
            items,
            &filtered,
            &filter,
            selected,
            allow_back,
            cursor_visible,
        )?;

        if !event::poll(SETUP_CURSOR_BLINK_INTERVAL).context("failed to poll setup picker key")? {
            cursor_visible = !cursor_visible;
            continue;
        }

        if let Event::Key(key) = event::read().context("failed to read setup picker key")? {
            cursor_visible = true;
            match key.code {
                KeyCode::Esc if allow_back => return Ok(SetupAction::Back),
                KeyCode::Esc => {}
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Err(setup_cancelled_error());
                }
                KeyCode::Enter => {
                    if let Some(index) = filtered.get(selected).copied() {
                        return Ok(SetupAction::Submit(index));
                    }
                }
                KeyCode::Up => {
                    if !filtered.is_empty() {
                        selected = if selected == 0 {
                            filtered.len().saturating_sub(1)
                        } else {
                            selected.saturating_sub(1)
                        };
                    }
                }
                KeyCode::Down => {
                    if !filtered.is_empty() {
                        selected = (selected + 1) % filtered.len();
                    }
                }
                KeyCode::Backspace => {
                    filter.backspace();
                    selected = 0;
                }
                KeyCode::Delete => {
                    filter.delete_forward();
                    selected = 0;
                }
                KeyCode::Left if allow_back && key.modifiers.is_empty() => {
                    return Ok(SetupAction::Back);
                }
                KeyCode::Left => filter.move_left(key.modifiers.contains(KeyModifiers::SHIFT)),
                KeyCode::Right => filter.move_right(key.modifiers.contains(KeyModifiers::SHIFT)),
                KeyCode::Char(c) => {
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                        filter.insert_char(c);
                    }
                    selected = 0;
                }
                _ => {}
            }
        }
    }
}

fn filtered_item_indices(items: &[PickerItem], filter: &str) -> Vec<usize> {
    let needle = filter.trim().to_ascii_lowercase();
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            // Filter only by the selectable value. Descriptions are explanatory text;
            // matching them makes one-letter searches useless (for example "w" used
            // to match "OpenAI-compatible gateway" before "Qwen").
            if needle.is_empty() || item.title.to_ascii_lowercase().contains(&needle) {
                Some(index)
            } else {
                None
            }
        })
        .collect()
}

fn render_picker(
    stdout: &mut io::Stdout,
    flow: SetupFlow,
    title: &str,
    _subtitle: &str,
    items: &[PickerItem],
    filtered: &[usize],
    filter: &InputState,
    selected: usize,
    allow_back: bool,
    cursor_visible: bool,
) -> Result<()> {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let cols_usize = cols as usize;
    let layout = setup_layout(cols_usize, rows);
    let left = layout.left;
    let top = layout.card_top;
    let width = layout.width;
    let card_height = layout.card_height;
    let step = setup_step_for_title(flow, title);
    let is_model_picker = matches!(step, SetupStep::Model);
    let is_model_manager_picker = matches!(flow.kind, SetupFlowKind::ModelManager);
    let is_profile_manager_picker = matches!(flow.kind, SetupFlowKind::ProfileManager);
    let is_gateway_channel_manager =
        matches!(flow.kind, SetupFlowKind::Gateway) && title == "Gateway channels";
    let header_row = top + 3;
    let list_start = top + 4;
    let visible_items = picker_visible_item_count(card_height);

    stdout.queue(SetBackgroundColor(SETUP_THEME.app_bg))?;
    clear_rows(stdout, 0, rows, cols_usize, SETUP_THEME.app_bg)?;

    for row in top..top + card_height {
        stdout.queue(MoveTo(left as u16, row as u16))?;
        stdout.queue(SetBackgroundColor(SETUP_THEME.card_bg))?;
        stdout.queue(Print(" ".repeat(width)))?;
    }

    draw_setup_title(stdout, &layout, flow)?;
    draw_setup_stepper(stdout, &layout, flow, step)?;
    draw_setup_input(
        stdout,
        left + 2,
        top + 1,
        width.saturating_sub(4),
        filter,
        "Type to filter",
        cursor_visible,
        false,
    )?;
    draw_picker_line(
        stdout,
        left,
        top + 2,
        width,
        &"─".repeat(width.saturating_sub(4)),
        SETUP_THEME.border_fg,
    )?;

    let row_width = width.saturating_sub(4);
    if is_model_manager_picker {
        draw_model_manager_header(stdout, left + 2, header_row, row_width)?;
    } else if is_model_picker {
        draw_model_header(stdout, left + 2, header_row, row_width)?;
    } else {
        let (title_label, detail_label) = picker_provider_header_labels(flow, step);
        draw_provider_header(
            stdout,
            left + 2,
            header_row,
            row_width,
            provider_title_column_width(items, filtered, row_width),
            title_label,
            detail_label,
        )?;
    }

    let first_visible = scroll_start_for_selection(selected, visible_items, filtered.len());
    for row_index in 0..visible_items {
        let row = list_start + row_index;
        stdout.queue(MoveTo(left as u16, row as u16))?;
        stdout.queue(SetBackgroundColor(SETUP_THEME.card_bg))?;
        stdout.queue(Print(" ".repeat(width)))?;

        let Some(filtered_index) = filtered.get(first_visible + row_index).copied() else {
            continue;
        };
        let item = &items[filtered_index];
        let selected_row = first_visible + row_index == selected;
        let fg = if selected_row {
            SETUP_THEME.select_fg
        } else {
            SETUP_THEME.fg
        };
        let bg = if selected_row {
            SETUP_THEME.select_bg
        } else {
            SETUP_THEME.card_bg
        };
        stdout.queue(MoveTo((left + 2) as u16, row as u16))?;
        stdout.queue(SetBackgroundColor(bg))?;
        stdout.queue(SetForegroundColor(fg))?;
        let rendered = if is_model_manager_picker {
            format_model_manager_row(item, row_width, selected_row)
        } else if is_model_picker {
            format_model_row(item, row_width, selected_row)
        } else {
            format_provider_row(
                item,
                row_width,
                provider_title_column_width(items, filtered, row_width),
                selected_row,
            )
        };
        stdout.queue(Print(rendered))?;
        stdout.queue(SetForegroundColor(Color::Reset))?;
        stdout.queue(SetBackgroundColor(SETUP_THEME.card_bg))?;
    }

    let footer_hint = if is_model_manager_picker {
        "↑↓ Move · Delete/Backspace Remove"
    } else if is_gateway_channel_manager {
        "↑↓ Move · Delete/Backspace Remove · ← Back"
    } else if allow_back {
        "↑↓ Move · ← Back"
    } else {
        "↑↓ Move"
    };
    let footer_action = if matches!(flow.kind, SetupFlowKind::ModelManager) {
        "↵ Main/Add"
    } else if is_profile_manager_picker {
        "↵ Use"
    } else if is_gateway_channel_manager {
        "↵ Add/Configure"
    } else {
        setup_footer_action_for_step(step)
    };
    draw_setup_footer(
        stdout,
        &layout,
        "",
        None,
        Some(footer_hint),
        footer_action,
        SETUP_THEME.muted_fg,
    )?;

    stdout.queue(SetForegroundColor(Color::Reset))?;
    stdout.queue(SetBackgroundColor(Color::Reset))?;
    let (cursor_col, cursor_row) =
        setup_input_cursor_position(left + 2, top + 1, filter, width.saturating_sub(4), false);
    stdout.queue(MoveTo(cursor_col, cursor_row))?;
    stdout.flush()?;
    Ok(())
}

fn provider_title_column_width(
    items: &[PickerItem],
    filtered: &[usize],
    row_width: usize,
) -> usize {
    let max_title = filtered
        .iter()
        .filter_map(|index| items.get(*index))
        .map(|item| UnicodeWidthStr::width(item.title.as_str()))
        .max()
        .unwrap_or(0);
    max_title.min(row_width / 2).max(12)
}

fn format_provider_row(
    item: &PickerItem,
    row_width: usize,
    title_width: usize,
    selected: bool,
) -> String {
    let marker = if selected { "› " } else { "  " };
    format_provider_cells(marker, &item.title, &item.detail, row_width, title_width)
}

fn draw_provider_header(
    stdout: &mut io::Stdout,
    left: usize,
    row: usize,
    row_width: usize,
    title_width: usize,
    title_label: &str,
    detail_label: &str,
) -> Result<()> {
    stdout.queue(MoveTo(left as u16, row as u16))?;
    stdout.queue(SetBackgroundColor(SETUP_THEME.card_bg))?;
    stdout.queue(SetForegroundColor(SETUP_THEME.muted_fg))?;
    stdout.queue(Print(format_provider_header(
        row_width,
        title_width,
        title_label,
        detail_label,
    )))?;
    stdout.queue(SetForegroundColor(Color::Reset))?;
    Ok(())
}

fn picker_provider_header_labels(flow: SetupFlow, step: SetupStep) -> (&'static str, &'static str) {
    match (flow.kind, step) {
        (SetupFlowKind::ProfileManager, SetupStep::Provider) => ("Profile", "Path"),
        (_, SetupStep::Auth) => ("Choice", "Details"),
        _ => (flow.first_picker_title, "Description"),
    }
}

fn format_provider_header(
    row_width: usize,
    title_width: usize,
    title_label: &str,
    detail_label: &str,
) -> String {
    format_provider_cells("  ", title_label, detail_label, row_width, title_width)
}

fn format_provider_cells(
    marker: &str,
    title: &str,
    detail: &str,
    row_width: usize,
    title_width: usize,
) -> String {
    let marker_width = UnicodeWidthStr::width(marker);
    let title = pad_or_truncate(title, title_width);
    let detail_width = row_width.saturating_sub(marker_width + title_width + 2);
    let detail = pad_or_truncate(detail, detail_width);
    pad_or_truncate(&format!("{marker}{title}  {detail}"), row_width)
}

#[derive(Clone, Copy)]
struct ModelColumnWidths {
    model: usize,
    input: usize,
    output: usize,
    context: usize,
}

fn model_column_widths(row_width: usize) -> ModelColumnWidths {
    let separator_width = 6usize;
    let fixed_width = 12usize.min(row_width / 5).max(7);
    let context_width = 10usize.min(row_width / 5).max(7);
    let fixed_total = fixed_width * 2 + context_width + separator_width;
    ModelColumnWidths {
        model: row_width.saturating_sub(fixed_total).max(14),
        input: fixed_width,
        output: fixed_width,
        context: context_width,
    }
}

fn draw_model_header(
    stdout: &mut io::Stdout,
    left: usize,
    row: usize,
    row_width: usize,
) -> Result<()> {
    stdout.queue(MoveTo(left as u16, row as u16))?;
    stdout.queue(SetBackgroundColor(SETUP_THEME.card_bg))?;
    stdout.queue(SetForegroundColor(SETUP_THEME.muted_fg))?;
    stdout.queue(Print(format_model_header(row_width)))?;
    stdout.queue(SetForegroundColor(Color::Reset))?;
    Ok(())
}

fn format_model_header(row_width: usize) -> String {
    let marker = "  ";
    let marker_width = UnicodeWidthStr::width(marker);
    let widths = model_column_widths(row_width.saturating_sub(marker_width));
    format_model_cells(
        marker, "Model", "Input", "Output", "Context", row_width, widths,
    )
}

fn format_model_row(item: &PickerItem, row_width: usize, selected: bool) -> String {
    let marker = if selected { "› " } else { "  " };
    let marker_width = UnicodeWidthStr::width(marker);
    let widths = model_column_widths(row_width.saturating_sub(marker_width));
    let columns = item
        .model_columns
        .as_ref()
        .cloned()
        .unwrap_or_else(ModelPickerColumns::unknown);
    format_model_cells(
        marker,
        &item.title,
        &columns.input,
        &columns.output,
        &columns.context,
        row_width,
        widths,
    )
}

fn format_model_cells(
    marker: &str,
    model: &str,
    input: &str,
    output: &str,
    context: &str,
    row_width: usize,
    widths: ModelColumnWidths,
) -> String {
    let line = format!(
        "{}{}  {}  {}  {}",
        marker,
        pad_or_truncate(model, widths.model),
        pad_or_truncate(input, widths.input),
        pad_or_truncate(output, widths.output),
        pad_or_truncate(context, widths.context),
    );
    pad_or_truncate(&line, row_width)
}

const MODEL_MANAGER_MAIN_WIDTH: usize = 4;
const MODEL_MANAGER_PROVIDER_WIDTH: usize = 14;
const MODEL_MANAGER_MODEL_WIDTH: usize = 30;
const MODEL_MANAGER_ENDPOINT_WIDTH: usize = 18;
const MODEL_MANAGER_KEY_WIDTH: usize = 8;
const MODEL_MANAGER_DATE_WIDTH: usize = 16;

pub(crate) fn format_model_manager_add_row() -> String {
    format_model_manager_title_cells("", "Add", "", "", "", "")
}

pub(crate) fn format_model_manager_saved_row(
    active: bool,
    provider: &str,
    model: &str,
    endpoint: &str,
    key: &str,
    date: &str,
) -> String {
    let main = if active { "*" } else { "" };
    format_model_manager_title_cells(main, provider, model, endpoint, key, date)
}

fn draw_model_manager_header(
    stdout: &mut io::Stdout,
    left: usize,
    row: usize,
    row_width: usize,
) -> Result<()> {
    stdout.queue(MoveTo(left as u16, row as u16))?;
    stdout.queue(SetBackgroundColor(SETUP_THEME.card_bg))?;
    stdout.queue(SetForegroundColor(SETUP_THEME.muted_fg))?;
    stdout.queue(Print(format_model_manager_header(row_width)))?;
    stdout.queue(SetForegroundColor(Color::Reset))?;
    Ok(())
}

fn format_model_manager_header(row_width: usize) -> String {
    format_model_manager_cells(
        "  ", "Main", "Provider", "Model", "Endpoint", "Key", "Date", row_width,
    )
}

fn format_model_manager_row(item: &PickerItem, row_width: usize, selected: bool) -> String {
    let marker = if selected { "› " } else { "  " };
    if item.detail.is_empty() {
        pad_or_truncate(&format!("{marker}{}", item.title), row_width)
    } else {
        format_model_manager_cells(marker, "", &item.title, &item.detail, "", "", "", row_width)
    }
}

fn format_model_manager_cells(
    marker: &str,
    main: &str,
    provider: &str,
    model: &str,
    endpoint: &str,
    key: &str,
    date: &str,
    row_width: usize,
) -> String {
    let line = format!(
        "{marker}{}",
        format_model_manager_title_cells(main, provider, model, endpoint, key, date)
    );
    pad_or_truncate(&line, row_width)
}

fn format_model_manager_title_cells(
    main: &str,
    provider: &str,
    model: &str,
    endpoint: &str,
    key: &str,
    date: &str,
) -> String {
    format!(
        "{}  {}  {}  {}  {}  {}",
        pad_or_truncate(main, MODEL_MANAGER_MAIN_WIDTH),
        pad_or_truncate(provider, MODEL_MANAGER_PROVIDER_WIDTH),
        pad_or_truncate(model, MODEL_MANAGER_MODEL_WIDTH),
        pad_or_truncate(endpoint, MODEL_MANAGER_ENDPOINT_WIDTH),
        pad_or_truncate(key, MODEL_MANAGER_KEY_WIDTH),
        pad_or_truncate(date, MODEL_MANAGER_DATE_WIDTH),
    )
}

fn scroll_start_for_selection(selected: usize, visible_items: usize, total: usize) -> usize {
    if total <= visible_items {
        0
    } else {
        let half = visible_items / 2;
        selected
            .saturating_sub(half)
            .min(total.saturating_sub(visible_items))
    }
}

fn picker_visible_item_count(card_height: usize) -> usize {
    // Rows used inside the card before the list:
    // top padding/input, divider, header, and one bottom padding row.
    card_height.saturating_sub(5).max(1)
}

fn draw_picker_line(
    stdout: &mut io::Stdout,
    left: usize,
    row: usize,
    width: usize,
    text: &str,
    fg: Color,
) -> Result<()> {
    stdout.queue(MoveTo((left + 2) as u16, row as u16))?;
    stdout.queue(SetBackgroundColor(SETUP_THEME.card_bg))?;
    stdout.queue(SetForegroundColor(fg))?;
    stdout.queue(Print(pad_or_truncate(
        &format!("  {text}"),
        width.saturating_sub(4),
    )))?;
    stdout.queue(SetForegroundColor(Color::Reset))?;
    Ok(())
}

fn draw_input_error(
    stdout: &mut io::Stdout,
    left: usize,
    row: usize,
    width: usize,
    text: &str,
) -> Result<()> {
    stdout.queue(MoveTo(left as u16, row as u16))?;
    stdout.queue(SetBackgroundColor(SETUP_THEME.card_bg))?;
    stdout.queue(SetForegroundColor(SETUP_THEME.accent))?;
    stdout.queue(Print(pad_or_truncate(text, width)))?;
    stdout.queue(SetForegroundColor(Color::Reset))?;
    Ok(())
}

fn draw_setup_footer(
    stdout: &mut io::Stdout,
    layout: &SetupLayout,
    fallback_left_text: &str,
    error: Option<&str>,
    left_hint: Option<&str>,
    right_action: &str,
    fg: Color,
) -> Result<()> {
    let content_width = layout.width;
    stdout.queue(MoveTo(layout.left as u16, layout.footer_row as u16))?;
    stdout.queue(SetBackgroundColor(SETUP_THEME.app_bg))?;
    stdout.queue(SetForegroundColor(fg))?;
    stdout.queue(Print(format_setup_footer_line(
        fallback_left_text,
        error,
        left_hint,
        right_action,
        content_width,
    )))?;
    stdout.queue(SetForegroundColor(Color::Reset))?;
    Ok(())
}

fn format_setup_footer_line(
    fallback_left_text: &str,
    error: Option<&str>,
    left_hint: Option<&str>,
    right_action: &str,
    content_width: usize,
) -> String {
    let left_text = error.or(left_hint).unwrap_or(fallback_left_text);
    let right_text = right_action;
    let inner_width = content_width;
    let right_width = UnicodeWidthStr::width(right_text);
    let left_budget = inner_width.saturating_sub(right_width).saturating_sub(1);
    let left_rendered = truncate_to_width(left_text, left_budget);
    let left_width = UnicodeWidthStr::width(left_rendered.as_str());
    let spacer = inner_width.saturating_sub(left_width + right_width);
    pad_or_truncate(
        &format!("{left_rendered}{}{right_text}", " ".repeat(spacer)),
        content_width,
    )
}

fn setup_footer_action_for_step(step: SetupStep) -> &'static str {
    match step {
        SetupStep::Model | SetupStep::Web => "↵ Confirm",
        SetupStep::Provider | SetupStep::Auth => "↵ Next",
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupStep {
    Provider,
    Auth,
    Model,
    Web,
}

fn setup_step_for_title(flow: SetupFlow, title: &str) -> SetupStep {
    match flow.kind {
        SetupFlowKind::Provider => match title {
            "Select provider" => SetupStep::Provider,
            "Select model" | "Model id" | "Context window" | "Fetching models" => SetupStep::Model,
            "Web Search" | "Web Search API key" | "Web Extract" | "Local browser fallback" => {
                SetupStep::Web
            }
            _ => SetupStep::Auth,
        },
        SetupFlowKind::Gateway => match title {
            "Select gateway channel" => SetupStep::Provider,
            "Review gateway" | "Gateway channel configured" => SetupStep::Model,
            _ => SetupStep::Auth,
        },
        SetupFlowKind::ModelManager => match title {
            "Models" => SetupStep::Provider,
            "Select model" | "Model id" | "Context window" | "Fetching models" => SetupStep::Model,
            _ => SetupStep::Auth,
        },
        SetupFlowKind::ProfileManager => match title {
            "Profiles" => SetupStep::Provider,
            _ => SetupStep::Auth,
        },
    }
}

fn draw_stepper(
    stdout: &mut io::Stdout,
    left: usize,
    row: usize,
    width: usize,
    flow: SetupFlow,
    current: SetupStep,
) -> Result<()> {
    let content_width = width.saturating_sub(4);
    stdout.queue(MoveTo((left + 2) as u16, row as u16))?;
    stdout.queue(SetBackgroundColor(SETUP_THEME.app_bg))?;

    let mut used = 0usize;
    for segment in stepper_segments(flow, current) {
        if used >= content_width {
            break;
        }
        stdout.queue(SetForegroundColor(if segment.active {
            SETUP_THEME.accent
        } else {
            SETUP_THEME.muted_fg
        }))?;
        let segment_text = truncate_to_width(&segment.text, content_width.saturating_sub(used));
        used = used.saturating_add(UnicodeWidthStr::width(segment_text.as_str()));
        stdout.queue(Print(segment_text))?;
    }
    stdout.queue(SetForegroundColor(Color::Reset))?;
    Ok(())
}

fn draw_setup_stepper(
    stdout: &mut io::Stdout,
    layout: &SetupLayout,
    flow: SetupFlow,
    current: SetupStep,
) -> Result<()> {
    let (draw_left, draw_width) = setup_stepper_draw_bounds(flow, layout);
    draw_stepper(
        stdout,
        draw_left,
        layout.stepper_row,
        draw_width,
        flow,
        current,
    )
}

fn draw_setup_title(stdout: &mut io::Stdout, layout: &SetupLayout, flow: SetupFlow) -> Result<()> {
    let title_width = UnicodeWidthStr::width(flow.title);
    let left = centered_setup_text_left(layout, title_width);
    stdout.queue(MoveTo(left as u16, layout.title_row as u16))?;
    stdout.queue(SetBackgroundColor(SETUP_THEME.app_bg))?;
    stdout.queue(SetForegroundColor(SETUP_THEME.fg))?;
    stdout.queue(Print(flow.title))?;
    stdout.queue(SetForegroundColor(Color::Reset))?;
    Ok(())
}

fn centered_setup_text_left(layout: &SetupLayout, text_width: usize) -> usize {
    layout
        .left
        .saturating_add(layout.width.saturating_sub(text_width) / 2)
}

fn setup_stepper_draw_bounds(flow: SetupFlow, layout: &SetupLayout) -> (usize, usize) {
    let plain = setup_stepper_plain_text(flow);
    let text_width = UnicodeWidthStr::width(plain.as_str());
    let row_left = centered_setup_text_left(layout, text_width);
    (row_left.saturating_sub(2), text_width.saturating_add(4))
}

struct StepperSegment {
    text: String,
    active: bool,
}

fn stepper_segments(flow: SetupFlow, current: SetupStep) -> Vec<StepperSegment> {
    let step_kinds = [
        SetupStep::Provider,
        SetupStep::Auth,
        SetupStep::Model,
        SetupStep::Web,
    ];
    let mut segments = Vec::new();
    for (index, label) in flow.steps.iter().enumerate() {
        if index > 0 {
            segments.push(StepperSegment {
                text: "  →  ".to_string(),
                active: false,
            });
        }
        let step = step_kinds.get(index).copied().unwrap_or(SetupStep::Model);
        segments.push(StepperSegment {
            text: (*label).to_string(),
            active: step == current,
        });
    }
    segments
}

fn setup_stepper_plain_text(flow: SetupFlow) -> String {
    stepper_segments(flow, SetupStep::Provider)
        .into_iter()
        .map(|segment| segment.text)
        .collect()
}

pub(crate) fn prompt_text(
    title: &str,
    subtitle: &str,
    placeholder: &str,
    initial: Option<&str>,
    required: bool,
    allow_back: bool,
    mask_input: bool,
) -> Result<SetupAction<String>> {
    prompt_text_with_flow(
        PROVIDER_SETUP_FLOW,
        title,
        subtitle,
        placeholder,
        initial,
        required,
        allow_back,
        mask_input,
    )
}

pub(crate) fn prompt_text_with_flow(
    flow: SetupFlow,
    title: &str,
    subtitle: &str,
    placeholder: &str,
    initial: Option<&str>,
    required: bool,
    allow_back: bool,
    mask_input: bool,
) -> Result<SetupAction<String>> {
    let mut stdout = io::stdout();
    enable_raw_mode().context("failed to enable setup text input raw mode")?;
    if let Err(error) = stdout.execute(Hide) {
        let _ = disable_raw_mode();
        return Err(error).context("failed to hide cursor for setup text input");
    }

    let mut input = InputState::new();
    if let Some(initial) = initial.filter(|value| !value.is_empty()) {
        input.insert_str(initial);
    }
    let mut error: Option<String> = None;
    let mut cursor_visible = true;

    let result = loop {
        render_text_prompt(
            &mut stdout,
            flow,
            title,
            subtitle,
            placeholder,
            error.as_deref(),
            &input,
            allow_back,
            cursor_visible,
            mask_input,
        )?;
        if !event::poll(SETUP_CURSOR_BLINK_INTERVAL)
            .context("failed to poll setup text input key")?
        {
            cursor_visible = !cursor_visible;
            continue;
        }

        if let Event::Key(key) = event::read().context("failed to read setup text input key")? {
            cursor_visible = true;
            match key.code {
                KeyCode::Esc if allow_back => break Ok(SetupAction::Back),
                KeyCode::Esc => {}
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err(setup_cancelled_error());
                }
                KeyCode::Enter => {
                    let value = input.text.trim().to_string();
                    if required && value.is_empty() {
                        error = Some("This value is required.".to_string());
                    } else {
                        break Ok(SetupAction::Submit(value));
                    }
                }
                KeyCode::Backspace => {
                    input.backspace();
                    error = None;
                }
                KeyCode::Delete => {
                    input.delete_forward();
                    error = None;
                }
                KeyCode::Left if allow_back && key.modifiers.is_empty() => {
                    break Ok(SetupAction::Back);
                }
                KeyCode::Left => input.move_left(key.modifiers.contains(KeyModifiers::SHIFT)),
                KeyCode::Right => input.move_right(key.modifiers.contains(KeyModifiers::SHIFT)),
                KeyCode::Char(c)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    input.insert_char(c);
                    error = None;
                }
                _ => {}
            }
        }
    };

    let _ = stdout.execute(Show);
    let _ = stdout.execute(SetForegroundColor(Color::Reset));
    let _ = stdout.execute(SetBackgroundColor(Color::Reset));
    let _ = move_cursor_below_setup(&mut stdout);
    let _ = stdout.flush();
    disable_raw_mode().context("failed to disable setup text input raw mode")?;
    result
}

pub(crate) fn prompt_confirm_with_flow(
    flow: SetupFlow,
    title: &str,
    lines: &[String],
    allow_back: bool,
) -> Result<SetupAction<()>> {
    let mut stdout = io::stdout();
    enable_raw_mode().context("failed to enable setup confirm raw mode")?;
    if let Err(error) = stdout.execute(Hide) {
        let _ = disable_raw_mode();
        return Err(error).context("failed to hide cursor for setup confirm");
    }

    let result = loop {
        render_confirm_prompt(&mut stdout, flow, title, lines, allow_back)?;
        if let Event::Key(key) = event::read().context("failed to read setup confirm key")? {
            match key.code {
                KeyCode::Esc if allow_back => break Ok(SetupAction::Back),
                KeyCode::Left if allow_back && key.modifiers.is_empty() => {
                    break Ok(SetupAction::Back);
                }
                KeyCode::Enter | KeyCode::Right => break Ok(SetupAction::Submit(())),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err(setup_cancelled_error());
                }
                _ => {}
            }
        }
    };

    let _ = stdout.execute(Show);
    let _ = stdout.execute(SetForegroundColor(Color::Reset));
    let _ = stdout.execute(SetBackgroundColor(Color::Reset));
    let _ = move_cursor_below_setup(&mut stdout);
    let _ = stdout.flush();
    disable_raw_mode().context("failed to disable setup confirm raw mode")?;
    result
}

pub(crate) fn prompt_confirm(
    title: &str,
    lines: &[String],
    allow_back: bool,
) -> Result<SetupAction<()>> {
    prompt_confirm_with_flow(PROVIDER_SETUP_FLOW, title, lines, allow_back)
}

pub(crate) fn show_setup_message(title: &str, subtitle: &str) -> Result<()> {
    show_setup_message_with_flow(PROVIDER_SETUP_FLOW, title, subtitle)
}

pub(crate) fn show_setup_message_with_flow(
    flow: SetupFlow,
    title: &str,
    subtitle: &str,
) -> Result<()> {
    let mut stdout = io::stdout();
    enable_raw_mode().context("failed to enable setup message raw mode")?;
    if let Err(error) = stdout.execute(Hide) {
        let _ = disable_raw_mode();
        return Err(error).context("failed to hide cursor for setup message");
    }
    render_message_prompt(&mut stdout, flow, title, subtitle)?;
    let _ = stdout.execute(Show);
    let _ = stdout.execute(SetForegroundColor(Color::Reset));
    let _ = stdout.execute(SetBackgroundColor(Color::Reset));
    let _ = move_cursor_below_setup(&mut stdout);
    disable_raw_mode().context("failed to disable setup message raw mode")
}

fn wait_setup_task<T>(title: &str, subtitle: &str, rx: mpsc::Receiver<Result<T>>) -> Result<T> {
    wait_setup_task_with_flow(PROVIDER_SETUP_FLOW, title, subtitle, rx)
}

pub(crate) fn wait_setup_task_with_flow<T>(
    flow: SetupFlow,
    title: &str,
    subtitle: &str,
    rx: mpsc::Receiver<Result<T>>,
) -> Result<T> {
    let mut stdout = io::stdout();
    enable_raw_mode().context("failed to enable setup loading raw mode")?;
    if let Err(error) = stdout.execute(Hide) {
        let _ = disable_raw_mode();
        return Err(error).context("failed to hide cursor for setup loading");
    }

    let mut phase = 0usize;
    let result = loop {
        let _ = stdout.execute(Hide);
        render_loading_prompt(&mut stdout, flow, title, subtitle, phase)?;
        match rx.try_recv() {
            Ok(result) => break result,
            Err(mpsc::TryRecvError::Disconnected) => {
                break Err(anyhow!("setup task ended without a result"));
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }

        std::thread::sleep(Duration::from_millis(120));
        phase = (phase + 1) % SETUP_SPINNER.len();
    };

    let _ = stdout.execute(Show);
    let _ = stdout.execute(SetForegroundColor(Color::Reset));
    let _ = stdout.execute(SetBackgroundColor(Color::Reset));
    let _ = move_cursor_below_setup(&mut stdout);
    disable_raw_mode().context("failed to disable setup loading raw mode")?;
    result
}

pub(crate) fn wait_setup_display_task_with_flow<T>(
    flow: SetupFlow,
    title: &str,
    lines: &[String],
    rx: mpsc::Receiver<Result<T>>,
) -> Result<T> {
    let mut stdout = io::stdout();
    enable_raw_mode().context("failed to enable setup display raw mode")?;
    if let Err(error) = stdout.execute(Hide) {
        let _ = disable_raw_mode();
        return Err(error).context("failed to hide cursor for setup display");
    }

    let mut phase = 0usize;
    let result = loop {
        let _ = stdout.execute(Hide);
        render_wait_display_prompt(&mut stdout, flow, title, lines, phase)?;
        match rx.try_recv() {
            Ok(result) => break result,
            Err(mpsc::TryRecvError::Disconnected) => {
                break Err(anyhow!("setup task ended without a result"));
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }

        std::thread::sleep(Duration::from_millis(120));
        phase = (phase + 1) % SETUP_SPINNER.len();
    };

    let _ = stdout.execute(Show);
    let _ = stdout.execute(SetForegroundColor(Color::Reset));
    let _ = stdout.execute(SetBackgroundColor(Color::Reset));
    let _ = move_cursor_below_setup(&mut stdout);
    disable_raw_mode().context("failed to disable setup display raw mode")?;
    result
}

fn wait_codex_device_task(
    login: &CodexDeviceLogin,
    rx: mpsc::Receiver<Result<crate::auth::ProviderCredentials>>,
) -> Result<crate::auth::ProviderCredentials> {
    let mut stdout = io::stdout();
    enable_raw_mode().context("failed to enable Codex login raw mode")?;
    if let Err(error) = stdout.execute(Hide) {
        let _ = disable_raw_mode();
        return Err(error).context("failed to hide cursor for Codex login");
    }

    let mut phase = 0usize;
    let result = loop {
        let _ = stdout.execute(Hide);
        render_codex_device_prompt(&mut stdout, login, phase)?;
        match rx.try_recv() {
            Ok(result) => break result,
            Err(mpsc::TryRecvError::Disconnected) => {
                break Err(anyhow!("Codex login ended without a result"));
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }

        std::thread::sleep(Duration::from_millis(120));
        phase = (phase + 1) % SETUP_SPINNER.len();
    };

    let _ = stdout.execute(Show);
    let _ = stdout.execute(SetForegroundColor(Color::Reset));
    let _ = stdout.execute(SetBackgroundColor(Color::Reset));
    let _ = move_cursor_below_setup(&mut stdout);
    disable_raw_mode().context("failed to disable Codex login raw mode")?;
    result
}

fn wait_nous_device_task(
    login: &NousDeviceLogin,
    rx: mpsc::Receiver<Result<crate::auth::ProviderCredentials>>,
) -> Result<crate::auth::ProviderCredentials> {
    let mut stdout = io::stdout();
    enable_raw_mode().context("failed to enable Nous login raw mode")?;
    if let Err(error) = stdout.execute(Hide) {
        let _ = disable_raw_mode();
        return Err(error).context("failed to hide cursor for Nous login");
    }

    let mut phase = 0usize;
    let result = loop {
        let _ = stdout.execute(Hide);
        render_nous_device_prompt(&mut stdout, login, phase)?;
        match rx.try_recv() {
            Ok(result) => break result,
            Err(mpsc::TryRecvError::Disconnected) => {
                break Err(anyhow!("Nous login ended without a result"));
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }

        std::thread::sleep(Duration::from_millis(120));
        phase = (phase + 1) % SETUP_SPINNER.len();
    };

    let _ = stdout.execute(Show);
    let _ = stdout.execute(SetForegroundColor(Color::Reset));
    let _ = stdout.execute(SetBackgroundColor(Color::Reset));
    let _ = move_cursor_below_setup(&mut stdout);
    disable_raw_mode().context("failed to disable Nous login raw mode")?;
    result
}

const SETUP_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn render_loading_prompt(
    stdout: &mut io::Stdout,
    flow: SetupFlow,
    title: &str,
    subtitle: &str,
    phase: usize,
) -> Result<()> {
    stdout.execute(Hide)?;
    let layout = setup_card(stdout, flow)?;
    draw_setup_stepper(stdout, &layout, flow, setup_step_for_title(flow, title))?;
    draw_picker_line(
        stdout,
        layout.left,
        layout.card_top + 2,
        layout.width,
        &format!(
            "{} {}",
            SETUP_SPINNER[phase % SETUP_SPINNER.len()],
            setup_loading_text(title, subtitle)
        ),
        SETUP_THEME.muted_fg,
    )?;
    stdout.queue(Hide)?;
    stdout.flush()?;
    Ok(())
}

fn render_wait_display_prompt(
    stdout: &mut io::Stdout,
    flow: SetupFlow,
    title: &str,
    lines: &[String],
    phase: usize,
) -> Result<()> {
    stdout.execute(Hide)?;
    let layout = setup_card(stdout, flow)?;
    draw_setup_stepper(stdout, &layout, flow, setup_step_for_title(flow, title))?;
    let left = layout.left;
    let top = layout.card_top;
    let width = layout.width;

    draw_picker_line(stdout, left, top + 1, width, title, SETUP_THEME.accent)?;
    let max_lines = layout.card_height.saturating_sub(5);
    for (index, line) in lines.iter().take(max_lines).enumerate() {
        draw_wait_display_line(stdout, left, top + 3 + index, width, line)?;
    }
    draw_setup_footer(
        stdout,
        &layout,
        "",
        None,
        Some("Keep this window open"),
        &format!("{} Waiting", SETUP_SPINNER[phase % SETUP_SPINNER.len()]),
        SETUP_THEME.muted_fg,
    )?;
    stdout.queue(Hide)?;
    stdout.flush()?;
    Ok(())
}

fn draw_wait_display_line(
    stdout: &mut io::Stdout,
    left: usize,
    row: usize,
    width: usize,
    line: &str,
) -> Result<()> {
    if let Some(cells) = line.strip_prefix(SETUP_QR_DENSE_ROW_PREFIX) {
        draw_qr_dense_row(stdout, left, row, width, cells)
    } else {
        let fg = if line.starts_with("http://")
            || line.starts_with("https://")
            || line.starts_with("URL: http://")
            || line.starts_with("URL: https://")
        {
            SETUP_THEME.accent
        } else {
            SETUP_THEME.fg
        };
        draw_picker_line(stdout, left, row, width, line, fg)
    }
}

fn draw_qr_dense_row(
    stdout: &mut io::Stdout,
    left: usize,
    row: usize,
    width: usize,
    cells: &str,
) -> Result<()> {
    if cells.is_empty() {
        return Ok(());
    }
    let inner_width = width.saturating_sub(4).max(1);
    let qr_width = UnicodeWidthStr::width(cells).min(inner_width);
    let qr_left = left + 2 + inner_width.saturating_sub(qr_width) / 2;
    stdout.queue(MoveTo(qr_left as u16, row as u16))?;
    stdout.queue(SetForegroundColor(Color::Black))?;
    stdout.queue(SetBackgroundColor(Color::White))?;
    stdout.queue(Print(truncate_to_width(cells, inner_width)))?;
    stdout.queue(SetForegroundColor(Color::Reset))?;
    stdout.queue(SetBackgroundColor(Color::Reset))?;
    Ok(())
}

fn render_codex_device_prompt(
    stdout: &mut io::Stdout,
    login: &CodexDeviceLogin,
    phase: usize,
) -> Result<()> {
    stdout.execute(Hide)?;
    let layout = setup_card(stdout, PROVIDER_SETUP_FLOW)?;
    draw_setup_stepper(stdout, &layout, PROVIDER_SETUP_FLOW, SetupStep::Auth)?;
    let left = layout.left;
    let top = layout.card_top;
    let width = layout.width;

    draw_picker_line(
        stdout,
        left,
        top + 2,
        width,
        "Open the Codex device page, sign in if needed, then enter this code.",
        SETUP_THEME.muted_fg,
    )?;
    draw_picker_line(
        stdout,
        left,
        top + 4,
        width,
        &login.verification_uri,
        SETUP_THEME.accent,
    )?;
    if !login.user_code.is_empty() {
        draw_picker_line(
            stdout,
            left,
            top + 6,
            width,
            &format!("Code: {}", login.user_code),
            SETUP_THEME.fg,
        )?;
    }
    draw_picker_line(
        stdout,
        left,
        top + 8,
        width,
        &format!(
            "{} Waiting for approval. DuckAgent will continue automatically.",
            SETUP_SPINNER[phase % SETUP_SPINNER.len()]
        ),
        SETUP_THEME.muted_fg,
    )?;
    stdout.queue(Hide)?;
    stdout.flush()?;
    Ok(())
}

fn render_nous_device_prompt(
    stdout: &mut io::Stdout,
    login: &NousDeviceLogin,
    phase: usize,
) -> Result<()> {
    stdout.execute(Hide)?;
    let layout = setup_card(stdout, PROVIDER_SETUP_FLOW)?;
    draw_setup_stepper(stdout, &layout, PROVIDER_SETUP_FLOW, SetupStep::Auth)?;
    let left = layout.left;
    let top = layout.card_top;
    let width = layout.width;

    draw_picker_line(
        stdout,
        left,
        top + 2,
        width,
        "Open the Nous device page, sign in if needed, then approve this code.",
        SETUP_THEME.muted_fg,
    )?;
    draw_picker_line(
        stdout,
        left,
        top + 4,
        width,
        &login.verification_uri,
        SETUP_THEME.accent,
    )?;
    if !login.user_code.is_empty() {
        draw_picker_line(
            stdout,
            left,
            top + 6,
            width,
            &format!("Code: {}", login.user_code),
            SETUP_THEME.fg,
        )?;
    }
    draw_picker_line(
        stdout,
        left,
        top + 8,
        width,
        &format!(
            "{} Waiting for approval. DuckAgent will continue automatically.",
            SETUP_SPINNER[phase % SETUP_SPINNER.len()]
        ),
        SETUP_THEME.muted_fg,
    )?;
    stdout.queue(Hide)?;
    stdout.flush()?;
    Ok(())
}

fn setup_loading_text(title: &str, subtitle: &str) -> String {
    if title == "Fetching models" {
        if let Some(provider) = subtitle.strip_prefix("Provider: ") {
            return format!("Fetching models of {provider}");
        }
    }
    if subtitle.trim().is_empty() {
        title.to_string()
    } else {
        format!("{title} {subtitle}")
    }
}

fn render_text_prompt(
    stdout: &mut io::Stdout,
    flow: SetupFlow,
    title: &str,
    subtitle: &str,
    placeholder: &str,
    error: Option<&str>,
    input: &InputState,
    allow_back: bool,
    cursor_visible: bool,
    mask_input: bool,
) -> Result<()> {
    let layout = setup_card(stdout, flow)?;
    let left = layout.left;
    let top = layout.card_top;
    let width = layout.width;
    let input_row = top + layout.card_height / 2;
    let step = setup_step_for_title(flow, title);
    draw_setup_stepper(stdout, &layout, flow, step)?;
    if !subtitle.is_empty() {
        draw_picker_line(
            stdout,
            left,
            input_row.saturating_sub(1),
            width,
            subtitle,
            SETUP_THEME.muted_fg,
        )?;
    }
    draw_setup_input(
        stdout,
        left + 2,
        input_row,
        width.saturating_sub(4),
        input,
        placeholder,
        cursor_visible,
        mask_input,
    )?;
    draw_picker_line(
        stdout,
        left,
        input_row + 1,
        width,
        &"─".repeat(width.saturating_sub(4)),
        SETUP_THEME.border_fg,
    )?;
    draw_setup_footer(
        stdout,
        &layout,
        "",
        None,
        if allow_back { Some("← Back") } else { None },
        setup_footer_action_for_step(step),
        SETUP_THEME.muted_fg,
    )?;
    if let Some(error) = error {
        draw_input_error(
            stdout,
            left + 4,
            input_row + 2,
            width.saturating_sub(6),
            error,
        )?;
    }
    let (cursor_col, cursor_row) = setup_input_cursor_position(
        left + 2,
        input_row,
        input,
        width.saturating_sub(4),
        mask_input,
    );
    stdout.queue(MoveTo(cursor_col, cursor_row))?;
    stdout.flush()?;
    Ok(())
}

fn render_confirm_prompt(
    stdout: &mut io::Stdout,
    flow: SetupFlow,
    title: &str,
    lines: &[String],
    allow_back: bool,
) -> Result<()> {
    let layout = setup_card(stdout, flow)?;
    let left = layout.left;
    let top = layout.card_top;
    let width = layout.width;
    draw_setup_stepper(stdout, &layout, flow, SetupStep::Model)?;
    draw_picker_line(stdout, left, top + 1, width, title, SETUP_THEME.accent)?;
    draw_picker_line(
        stdout,
        left,
        top + 2,
        width,
        &"─".repeat(width.saturating_sub(4)),
        SETUP_THEME.border_fg,
    )?;
    let max_lines = layout.card_height.saturating_sub(6);
    for (index, line) in lines.iter().take(max_lines).enumerate() {
        draw_picker_line(stdout, left, top + 4 + index, width, line, SETUP_THEME.fg)?;
    }
    draw_setup_footer(
        stdout,
        &layout,
        "",
        None,
        if allow_back { Some("← Back") } else { None },
        "↵ Confirm",
        SETUP_THEME.muted_fg,
    )?;
    stdout.queue(Hide)?;
    stdout.flush()?;
    Ok(())
}

fn render_message_prompt(
    stdout: &mut io::Stdout,
    flow: SetupFlow,
    title: &str,
    subtitle: &str,
) -> Result<()> {
    let layout = setup_card(stdout, flow)?;
    draw_setup_stepper(stdout, &layout, flow, setup_step_for_title(flow, title))?;
    draw_picker_line(
        stdout,
        layout.left,
        layout.card_top + 1,
        layout.width,
        title,
        SETUP_THEME.accent,
    )?;
    draw_picker_line(
        stdout,
        layout.left,
        layout.card_top + 2,
        layout.width,
        subtitle,
        SETUP_THEME.muted_fg,
    )?;
    stdout.flush()?;
    Ok(())
}

fn setup_card(stdout: &mut io::Stdout, flow: SetupFlow) -> Result<SetupLayout> {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let cols_usize = cols as usize;
    let layout = setup_layout(cols_usize, rows);

    stdout.queue(SetBackgroundColor(SETUP_THEME.app_bg))?;
    clear_rows(stdout, 0, rows, cols_usize, SETUP_THEME.app_bg)?;
    for row in layout.card_top..layout.card_top + layout.card_height {
        stdout.queue(MoveTo(layout.left as u16, row as u16))?;
        stdout.queue(SetBackgroundColor(SETUP_THEME.card_bg))?;
        stdout.queue(Print(" ".repeat(layout.width)))?;
    }
    draw_setup_title(stdout, &layout, flow)?;
    Ok(layout)
}

#[derive(Debug, Clone, Copy)]
struct SetupLayout {
    left: usize,
    title_row: usize,
    stepper_row: usize,
    card_top: usize,
    footer_row: usize,
    width: usize,
    card_height: usize,
}

fn setup_layout(cols: usize, rows: u16) -> SetupLayout {
    let width = cols
        .min(SETUP_CONTAINER_MAX_WIDTH_COLS)
        .max(40)
        .min(cols.max(1));
    let outer_rows = rows as usize;
    let chrome_height = 5usize;
    let card_height = outer_rows
        .saturating_sub(chrome_height)
        .min(SETUP_CARD_TARGET_HEIGHT)
        .max(1);
    let left = cols.saturating_sub(width) / 2;
    let total_height = card_height.saturating_add(chrome_height);
    let block_top = outer_rows.saturating_sub(total_height) / 2;
    let title_row = block_top;
    let stepper_row = title_row.saturating_add(2);
    let card_top = stepper_row.saturating_add(2);
    let footer_row = card_top.saturating_add(card_height);
    SetupLayout {
        left,
        title_row,
        stepper_row,
        card_top,
        footer_row,
        width,
        card_height,
    }
}

fn move_cursor_below_setup(stdout: &mut io::Stdout) -> Result<()> {
    let (_, rows) = terminal::size().unwrap_or((80, 24));
    let row = rows.saturating_sub(1) as usize;
    stdout.queue(MoveTo(0, row as u16))?;
    stdout.queue(Print("\n"))?;
    Ok(())
}

fn draw_setup_input(
    stdout: &mut io::Stdout,
    left: usize,
    row: usize,
    width: usize,
    input: &InputState,
    placeholder: &str,
    cursor_visible: bool,
    mask_input: bool,
) -> Result<()> {
    stdout.queue(MoveTo(left as u16, row as u16))?;
    stdout.queue(SetBackgroundColor(SETUP_THEME.card_bg))?;
    stdout.queue(SetForegroundColor(SETUP_THEME.fg))?;

    let available_width = width.saturating_sub(2);
    stdout.queue(SetForegroundColor(SETUP_THEME.accent))?;
    stdout.queue(Print("› "))?;
    stdout.queue(SetForegroundColor(SETUP_THEME.fg))?;

    if input.text.is_empty() && !placeholder.is_empty() {
        let placeholder = truncate_to_width(placeholder, available_width);
        let placeholder_width = UnicodeWidthStr::width(placeholder.as_str());
        stdout.queue(SetForegroundColor(SETUP_THEME.muted_fg))?;
        let mut chars = placeholder.chars();
        if let Some(first) = chars.next() {
            if cursor_visible {
                draw_setup_placeholder_cursor_cell(stdout, first)?;
                stdout.queue(SetForegroundColor(SETUP_THEME.muted_fg))?;
            } else {
                stdout.queue(Print(first))?;
            }
            for ch in chars {
                stdout.queue(Print(ch))?;
            }
        }
        stdout.queue(SetForegroundColor(SETUP_THEME.fg))?;
        let used = 2usize.saturating_add(placeholder_width).min(width);
        if used < width {
            stdout.queue(SetBackgroundColor(SETUP_THEME.card_bg))?;
            stdout.queue(Print(" ".repeat(width - used)))?;
        }
        stdout.queue(SetForegroundColor(Color::Reset))?;
        return Ok(());
    }

    let display_input = setup_display_input(input, mask_input);
    let layout = display_input.visible_layout(available_width);
    let selection = display_input.selection_range();
    let mut printed_width = 0usize;
    for (idx, ch) in display_input.text.chars().enumerate() {
        if idx < layout.start || idx >= layout.end {
            continue;
        }
        if idx == display_input.cursor && cursor_visible {
            draw_setup_cursor_cell(stdout, ch)?;
        } else if selection.as_ref().is_some_and(|range| range.contains(&idx)) {
            stdout.queue(SetBackgroundColor(SETUP_THEME.select_bg))?;
            stdout.queue(SetForegroundColor(SETUP_THEME.select_fg))?;
            stdout.queue(Print(ch))?;
            stdout.queue(SetBackgroundColor(SETUP_THEME.card_bg))?;
            stdout.queue(SetForegroundColor(SETUP_THEME.fg))?;
        } else {
            stdout.queue(Print(ch))?;
        }
        printed_width = printed_width.saturating_add(char_width(ch));
    }

    if display_input.cursor == display_input.text.chars().count() && cursor_visible {
        draw_setup_cursor_cell(stdout, ' ')?;
        printed_width = printed_width.saturating_add(1);
    }

    let used = 2usize.saturating_add(printed_width).min(width);
    if used < width {
        stdout.queue(SetBackgroundColor(SETUP_THEME.card_bg))?;
        stdout.queue(Print(" ".repeat(width - used)))?;
    }
    stdout.queue(SetForegroundColor(Color::Reset))?;
    Ok(())
}

fn setup_input_cursor_position(
    left: usize,
    row: usize,
    input: &InputState,
    width: usize,
    mask_input: bool,
) -> (u16, u16) {
    if input.text.is_empty() {
        return ((left + 2) as u16, row as u16);
    }
    let display_input = setup_display_input(input, mask_input);
    let available_width = width.saturating_sub(2);
    let layout = display_input.visible_layout(available_width);
    let cursor_width = display_input
        .text
        .chars()
        .enumerate()
        .filter(|(idx, _)| *idx >= layout.start && *idx < display_input.cursor)
        .map(|(_, ch)| char_width(ch))
        .sum::<usize>();
    ((left + 2 + cursor_width) as u16, row as u16)
}

fn setup_display_input(input: &InputState, mask_input: bool) -> InputState {
    if !mask_input {
        return input.clone();
    }
    let mut display = input.clone();
    display.text = "*".repeat(input.text.chars().count());
    display
}

fn draw_setup_cursor_cell(stdout: &mut io::Stdout, ch: char) -> Result<()> {
    stdout.queue(SetBackgroundColor(SETUP_THEME.accent))?;
    stdout.queue(SetForegroundColor(SETUP_THEME.app_bg))?;
    stdout.queue(Print(ch))?;
    stdout.queue(SetBackgroundColor(SETUP_THEME.card_bg))?;
    stdout.queue(SetForegroundColor(SETUP_THEME.fg))?;
    Ok(())
}

fn draw_setup_placeholder_cursor_cell(stdout: &mut io::Stdout, ch: char) -> Result<()> {
    stdout.queue(SetBackgroundColor(SETUP_THEME.accent))?;
    stdout.queue(SetForegroundColor(SETUP_THEME.muted_fg))?;
    stdout.queue(Print(ch))?;
    stdout.queue(SetBackgroundColor(SETUP_THEME.card_bg))?;
    stdout.queue(SetForegroundColor(SETUP_THEME.fg))?;
    Ok(())
}

fn clear_rows(
    stdout: &mut io::Stdout,
    start: u16,
    end: u16,
    width: usize,
    bg: Color,
) -> Result<()> {
    for row in start..end {
        stdout.queue(MoveTo(0, row))?;
        stdout.queue(Clear(ClearType::CurrentLine))?;
        stdout.queue(SetBackgroundColor(bg))?;
        stdout.queue(Print(" ".repeat(width)))?;
    }
    Ok(())
}

fn pad_or_truncate(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > width {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    if used < width {
        out.push_str(&" ".repeat(width - used));
    }
    out
}

fn truncate_to_width(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > width {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out
}

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_filter_matches_title_and_digits_without_description_noise() {
        let items = vec![
            PickerItem {
                title: "deepseek-v4-flash".to_string(),
                detail: "available model".to_string(),
                model_columns: None,
            },
            PickerItem {
                title: "gpt-4.1".to_string(),
                detail: "OpenAI".to_string(),
                model_columns: None,
            },
        ];

        assert_eq!(filtered_item_indices(&items, "4.1"), vec![1]);
        assert!(filtered_item_indices(&items, "open").is_empty());
        assert_eq!(filtered_item_indices(&items, "deep"), vec![0]);
    }

    #[test]
    fn picker_single_character_filter_does_not_match_description() {
        let items = vec![
            PickerItem {
                title: "openrouter".to_string(),
                detail: "OpenRouter OpenAI-compatible gateway".to_string(),
                model_columns: None,
            },
            PickerItem {
                title: "Qwen OAuth".to_string(),
                detail: "Provider registered but hidden until fully wired".to_string(),
                model_columns: None,
            },
        ];

        assert_eq!(filtered_item_indices(&items, "w"), vec![1]);
    }

    #[test]
    fn windows_sandbox_setup_picker_uses_codex_style_choices_without_legacy_fallback() {
        let items = windows_sandbox_setup_picker_items();
        let titles = items
            .iter()
            .map(|item| item.title.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            titles,
            vec![
                "Set up default sandbox (requires Administrator permissions)",
                "Run without sandbox (danger)",
                "Quit"
            ]
        );
        assert!(!titles.iter().any(|title| title.contains("non-admin")));
    }

    #[test]
    fn picker_row_aligns_description_column() {
        let short = PickerItem {
            title: "xai".to_string(),
            detail: "xAI endpoint".to_string(),
            model_columns: None,
        };
        let long = PickerItem {
            title: "openrouter".to_string(),
            detail: "OpenRouter gateway".to_string(),
            model_columns: None,
        };

        let first = format_provider_row(&short, 48, 10, false);
        let second = format_provider_row(&long, 48, 10, false);
        assert_eq!(
            first.find("xAI endpoint"),
            second.find("OpenRouter gateway")
        );
    }

    #[test]
    fn provider_header_aligns_description_column() {
        let header = format_provider_header(48, 10, "Provider", "Description");
        let row = format_provider_row(
            &PickerItem {
                title: "openrouter".to_string(),
                detail: "OpenRouter gateway".to_string(),
                model_columns: None,
            },
            48,
            10,
            false,
        );
        assert_eq!(header.find("Description"), row.find("OpenRouter gateway"));
        assert!(header.contains("Provider"));
    }

    #[test]
    fn auth_picker_uses_choice_header() {
        let (title, detail) = picker_provider_header_labels(PROVIDER_SETUP_FLOW, SetupStep::Auth);
        let header = format_provider_header(48, 10, title, detail);
        assert!(header.contains("Choice"));
        assert!(header.contains("Details"));
        assert!(!header.contains("Provider"));
    }

    #[test]
    fn gateway_picker_uses_channel_header() {
        let (title, detail) =
            picker_provider_header_labels(GATEWAY_SETUP_FLOW, SetupStep::Provider);
        let header = format_provider_header(48, 10, title, detail);
        assert!(header.contains("Channel"));
        assert!(header.contains("Description"));
        assert!(!header.contains("Provider"));
    }

    #[test]
    fn model_manager_header_uses_saved_model_columns() {
        let header = format_model_manager_header(90);
        assert!(header.contains("Main"));
        assert!(header.contains("Provider"));
        assert!(header.contains("Model"));
        assert!(header.contains("Endpoint"));
        assert!(header.contains("Key"));
        assert!(header.contains("Date"));
        assert!(!header.contains("Description"));
        assert_eq!(UnicodeWidthStr::width(header.as_str()), 90);
    }

    #[test]
    fn model_manager_row_uses_full_width_for_saved_model_record() {
        let item = PickerItem {
            title: format_model_manager_saved_row(
                true,
                "deepseek",
                "deepseek-chat",
                "api.deepseek.com",
                "...9f2a",
                "2026-05-17",
            ),
            detail: String::new(),
            model_columns: None,
        };
        let row = format_model_manager_row(&item, 100, true);
        assert!(row.contains("deepseek-chat"));
        assert!(row.contains("api.deepseek.com"));
        assert!(row.contains("...9f2a"));
        assert!(row.contains("*"));
        assert_eq!(UnicodeWidthStr::width(row.as_str()), 100);
    }

    #[test]
    fn model_manager_header_aligns_saved_model_columns() {
        let header = format_model_manager_header(100);
        let row = format_model_manager_row(
            &PickerItem {
                title: format_model_manager_saved_row(
                    false,
                    "deepseek",
                    "deepseek-v4-flash",
                    "api.deepseek.com",
                    "...1212",
                    "2026-05-17",
                ),
                detail: String::new(),
                model_columns: None,
            },
            100,
            false,
        );
        assert_eq!(header.find("Provider"), row.find("deepseek"));
        assert_eq!(header.find("Model"), row.find("deepseek-v4-flash"));
        assert_eq!(header.find("Endpoint"), row.find("api.deepseek.com"));
        assert_eq!(header.find("Key"), row.find("...1212"));
    }

    #[test]
    fn model_manager_delete_selection_targets_current_filtered_row() {
        match model_manager_delete_selection(&[0, 3, 5], 1) {
            Some(PickerEditAction::Delete(index)) => assert_eq!(index, 3),
            _ => panic!("expected delete action"),
        }
        assert!(model_manager_delete_selection(&[0], 2).is_none());
    }

    #[test]
    fn setup_back_step_visibility_matches_provider_requirements() {
        assert!(provider_has_api_key_step(ProviderKind::DeepSeek));
        assert!(!provider_has_base_url_step(ProviderKind::DeepSeek));
        assert!(!provider_has_api_mode_step(ProviderKind::DeepSeek));

        assert!(provider_has_base_url_step(ProviderKind::Custom));
        assert!(provider_has_api_mode_step(ProviderKind::Custom));
        assert!(provider_has_api_key_step(ProviderKind::Custom));

        assert!(provider_has_base_url_step(ProviderKind::Bedrock));
        assert!(!provider_has_api_key_step(ProviderKind::Bedrock));
    }

    #[test]
    fn model_columns_show_cost_and_context() {
        let capabilities = ModelCapabilities {
            input_cost: Some(0.29),
            output_cost: Some(2.86),
            context_window: Some(128_000),
            ..ModelCapabilities::default()
        };
        let columns = format_model_columns(&capabilities);
        assert_eq!(columns.input, "$0.29");
        assert_eq!(columns.output, "$2.86");
        assert_eq!(columns.context, "128k");
    }

    #[test]
    fn context_window_input_accepts_plain_k_and_m_values() {
        assert_eq!(parse_context_window_input("32768"), Some(32_768));
        assert_eq!(parse_context_window_input("32k"), Some(32_000));
        assert_eq!(parse_context_window_input("128K"), Some(128_000));
        assert_eq!(parse_context_window_input("1m"), Some(1_000_000));
        assert_eq!(parse_context_window_input("1,000,000"), Some(1_000_000));
        assert_eq!(parse_context_window_input("0"), None);
        assert_eq!(parse_context_window_input("abc"), None);
    }

    #[test]
    fn zero_model_cost_renders_without_dangling_decimal() {
        assert_eq!(format_cost(Some(0.0)), "$0");
    }

    #[test]
    fn setup_loading_text_compacts_fetching_models() {
        assert_eq!(
            setup_loading_text("Fetching models", "Provider: openrouter"),
            "Fetching models of openrouter"
        );
    }

    #[test]
    fn model_row_uses_structured_columns() {
        let item = PickerItem {
            title: "deepseek-v4-flash".to_string(),
            detail: String::new(),
            model_columns: Some(ModelPickerColumns {
                input: "$0.29".to_string(),
                output: "$2.86".to_string(),
                context: "128k".to_string(),
            }),
        };

        let header = format_model_header(80);
        let row = format_model_row(&item, 80, true);
        assert!(header.contains("Model"));
        assert!(header.contains("Input"));
        assert!(header.contains("Output"));
        assert!(header.contains("Context"));
        assert!(row.find("$0.29") < row.find("$2.86"));
        assert!(row.find("$2.86") < row.find("128k"));
        assert!(!row.contains("Input $0.29"));
        assert!(!row.contains("Output $2.86"));
    }

    #[test]
    fn setup_step_for_title_maps_provider_model_and_web_flow() {
        assert!(matches!(
            setup_step_for_title(PROVIDER_SETUP_FLOW, "Select provider"),
            SetupStep::Provider
        ));
        assert!(matches!(
            setup_step_for_title(PROVIDER_SETUP_FLOW, "API key/token"),
            SetupStep::Auth
        ));
        assert!(matches!(
            setup_step_for_title(PROVIDER_SETUP_FLOW, "Select model"),
            SetupStep::Model
        ));
        assert!(matches!(
            setup_step_for_title(PROVIDER_SETUP_FLOW, "Context window"),
            SetupStep::Model
        ));
        assert!(matches!(
            setup_step_for_title(PROVIDER_SETUP_FLOW, "Web Search"),
            SetupStep::Web
        ));
        assert!(matches!(
            setup_step_for_title(PROVIDER_SETUP_FLOW, "Local browser fallback"),
            SetupStep::Web
        ));
    }

    #[test]
    fn gateway_step_for_title_maps_to_channel_configure_review() {
        assert!(matches!(
            setup_step_for_title(GATEWAY_SETUP_FLOW, "Select gateway channel"),
            SetupStep::Provider
        ));
        assert!(matches!(
            setup_step_for_title(GATEWAY_SETUP_FLOW, "Bot id"),
            SetupStep::Auth
        ));
        assert!(matches!(
            setup_step_for_title(GATEWAY_SETUP_FLOW, "Review gateway"),
            SetupStep::Model
        ));
    }

    #[test]
    fn stepper_segments_highlight_without_brackets() {
        let segments = stepper_segments(PROVIDER_SETUP_FLOW, SetupStep::Auth);
        let joined = segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>();
        assert!(!joined.contains('['));
        assert!(!joined.contains(']'));
        assert!(
            segments
                .iter()
                .any(|segment| segment.active && segment.text == "2. Auth")
        );
    }

    #[test]
    fn provider_stepper_includes_web_step() {
        let segments = stepper_segments(PROVIDER_SETUP_FLOW, SetupStep::Web);
        let joined = segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>();
        assert!(joined.contains("1. Select provider"));
        assert!(joined.contains("2. Auth"));
        assert!(joined.contains("3. Select model"));
        assert!(joined.contains("4. Web"));
        assert!(
            segments
                .iter()
                .any(|segment| segment.active && segment.text == "4. Web")
        );
    }

    #[test]
    fn gateway_stepper_uses_gateway_labels() {
        let segments = stepper_segments(GATEWAY_SETUP_FLOW, SetupStep::Auth);
        let joined = segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>();
        assert!(joined.contains("1. Select channel"));
        assert!(joined.contains("2. Configure"));
        assert!(joined.contains("3. Review"));
        assert!(!joined.contains("Select model"));
    }

    #[test]
    fn profile_manager_flow_uses_profile_labels() {
        let segments = stepper_segments(PROFILE_MANAGER_FLOW, SetupStep::Provider);
        let joined = segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>();
        assert!(joined.contains("1. Select profile"));
        assert!(joined.contains("2. Use"));
        assert_eq!(
            picker_provider_header_labels(PROFILE_MANAGER_FLOW, SetupStep::Provider),
            ("Profile", "Path")
        );
        assert!(matches!(
            setup_step_for_title(PROFILE_MANAGER_FLOW, "Profiles"),
            SetupStep::Provider
        ));
    }

    #[test]
    fn api_key_prompt_subtitle_uses_provider_name() {
        assert_eq!(
            api_key_prompt_subtitle(ProviderKind::DeepSeek),
            "Enter the deepseek API key or token"
        );
    }

    #[test]
    fn footer_places_enter_hint_on_the_right() {
        let right = "↵ Next";
        let content_width = 40usize;
        let line = format_setup_footer_line("↑↓ Move", None, Some("↑↓ Move"), right, content_width);
        assert!(line.starts_with("↑↓ Move"));
        assert!(line.ends_with(right));
        assert_eq!(UnicodeWidthStr::width(line.as_str()), content_width);
    }

    #[test]
    fn footer_action_matches_setup_step() {
        assert_eq!(setup_footer_action_for_step(SetupStep::Provider), "↵ Next");
        assert_eq!(setup_footer_action_for_step(SetupStep::Auth), "↵ Next");
        assert_eq!(setup_footer_action_for_step(SetupStep::Model), "↵ Confirm");
    }

    #[test]
    fn setup_layout_places_chrome_outside_card() {
        let layout = setup_layout(120, 40);
        assert!(layout.title_row < layout.stepper_row);
        assert!(layout.stepper_row < layout.card_top);
        assert_eq!(layout.footer_row, layout.card_top + layout.card_height);
        assert_eq!(layout.left, 10);
        assert_eq!(layout.width, 100);
    }

    #[test]
    fn setup_stepper_draw_width_includes_inner_padding() {
        let text_width =
            UnicodeWidthStr::width(setup_stepper_plain_text(PROVIDER_SETUP_FLOW).as_str());
        assert!(text_width > 4);
        // draw_stepper reserves two columns of internal padding on each side.
        assert_eq!(text_width.saturating_add(4).saturating_sub(4), text_width);
    }

    #[test]
    fn setup_stepper_centers_visible_text_not_padding() {
        let layout = setup_layout(120, 40);
        let (draw_left, draw_width) = setup_stepper_draw_bounds(PROVIDER_SETUP_FLOW, &layout);
        let text_width =
            UnicodeWidthStr::width(setup_stepper_plain_text(PROVIDER_SETUP_FLOW).as_str());
        let visible_left = draw_left + 2;
        let visible_right = visible_left + draw_width.saturating_sub(4);
        let card_right = layout.left + layout.width;
        let left_margin = visible_left.saturating_sub(layout.left);
        let right_margin = card_right.saturating_sub(visible_right);

        assert_eq!(draw_width, text_width + 4);
        assert!(left_margin.abs_diff(right_margin) <= 1);
    }

    #[test]
    fn setup_title_centers_above_stepper() {
        let layout = setup_layout(120, 40);
        let title_width = UnicodeWidthStr::width(PROVIDER_SETUP_FLOW.title);
        let title_left = centered_setup_text_left(&layout, title_width);
        let title_right = title_left + title_width;
        let card_right = layout.left + layout.width;
        let left_margin = title_left.saturating_sub(layout.left);
        let right_margin = card_right.saturating_sub(title_right);

        assert_eq!(PROVIDER_SETUP_FLOW.title, "DuckAgent Setup");
        assert_eq!(layout.stepper_row, layout.title_row + 2);
        assert!(left_margin.abs_diff(right_margin) <= 1);
    }

    #[test]
    fn gateway_title_is_distinct_from_provider_setup() {
        assert_eq!(GATEWAY_SETUP_FLOW.title, "DuckAgent Gateway Setup");
        assert_ne!(GATEWAY_SETUP_FLOW.steps, PROVIDER_SETUP_FLOW.steps);
    }

    #[test]
    fn setup_title_has_blank_row_before_stepper() {
        let layout = setup_layout(120, 40);
        assert_eq!(layout.stepper_row.saturating_sub(layout.title_row), 2);
    }

    #[test]
    fn picker_visible_rows_are_container_height_minus_fixed_chrome() {
        assert_eq!(picker_visible_item_count(22), 17);
        assert_eq!(picker_visible_item_count(4), 1);
    }

    #[test]
    fn setup_input_cursor_stays_at_start_when_placeholder_is_visible() {
        let input = InputState::new();
        assert_eq!(
            setup_input_cursor_position(10, 5, &input, 30, false),
            (12, 5)
        );
    }

    #[test]
    fn setup_secret_input_uses_masked_display_text() {
        let mut input = InputState::new();
        input.insert_str("sk-secret");
        let display = setup_display_input(&input, true);
        assert_eq!(display.text, "*********");
        assert_eq!(input.text, "sk-secret");
    }

    #[test]
    fn picker_scroll_keeps_selection_visible() {
        assert_eq!(scroll_start_for_selection(0, 5, 20), 0);
        assert_eq!(scroll_start_for_selection(10, 5, 20), 8);
        assert_eq!(scroll_start_for_selection(19, 5, 20), 15);
    }
}
