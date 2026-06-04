use crate::auth::{load_auth_store, resolve_provider_credentials, save_provider_credentials};
use crate::profiles;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

mod config;
mod registry;
pub use config::{
    load_global_runtime_config, load_model_capabilities_override,
    load_provider_model_capability_overrides, save_global_runtime_config,
    save_model_capabilities_override,
};
pub use registry::{ApiMode, ProviderKind};

pub const UNKNOWN_CONTEXT_WINDOW_FALLBACK: u64 = 32_000;

const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const MODELS_DEV_CACHE_RELATIVE_PATH: &str = "cache/models_dev_cache.json";
const PROVIDER_MODELS_CACHE_DIR_RELATIVE_PATH: &str = "cache/provider_models";
const MODEL_REFRESH_BACKOFFS: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(60),
];

static MODELS_DEV_REFRESH_RUNNING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SessionRuntimeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_mode: Option<ApiMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_source: Option<String>,
}

impl SessionRuntimeConfig {
    pub fn is_empty(&self) -> bool {
        self.provider.is_none()
            && self.model_id.is_none()
            && self.model.is_none()
            && self.base_url.is_none()
            && self.api_mode.is_none()
            && self.provider_source.is_none()
    }
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeOverride {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_mode: Option<ApiMode>,
    pub api_key: Option<String>,
}

impl RuntimeOverride {
    fn is_empty(&self) -> bool {
        self.provider
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
            && self
                .model
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            && self
                .base_url
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            && self.api_mode.is_none()
            && self
                .api_key
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeProvider {
    pub model_id: Option<String>,
    pub provider: ProviderKind,
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub api_mode: ApiMode,
    pub source: String,
    pub account_id: Option<String>,
}

impl RuntimeProvider {
    pub fn session_config(&self) -> SessionRuntimeConfig {
        SessionRuntimeConfig {
            model_id: self.model_id.clone(),
            provider: Some(self.provider.as_str().to_string()),
            model: Some(self.model.clone()),
            base_url: Some(self.base_url.clone()),
            api_mode: Some(self.api_mode),
            provider_source: Some(self.source.clone()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelCapabilities {
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub input_cost: Option<f64>,
    #[serde(default)]
    pub output_cost: Option<f64>,
    #[serde(default)]
    pub supports_reasoning: Option<bool>,
    #[serde(default)]
    pub supports_tools: Option<bool>,
    #[serde(default)]
    pub supports_vision: Option<bool>,
    #[serde(default)]
    pub family: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProviderDescriptor {
    pub provider: ProviderKind,
    pub name: &'static str,
    pub description: &'static str,
}

pub fn all_provider_kinds() -> &'static [ProviderKind] {
    &[
        ProviderKind::Nous,
        ProviderKind::OpenRouter,
        ProviderKind::AiGateway,
        ProviderKind::Anthropic,
        ProviderKind::OpenAi,
        ProviderKind::OpenAiCodex,
        ProviderKind::Xiaomi,
        ProviderKind::Nvidia,
        ProviderKind::QwenOauth,
        ProviderKind::Copilot,
        ProviderKind::CopilotAcp,
        ProviderKind::HuggingFace,
        ProviderKind::Gemini,
        ProviderKind::GoogleGeminiCli,
        ProviderKind::DeepSeek,
        ProviderKind::Xai,
        ProviderKind::Zai,
        ProviderKind::KimiCoding,
        ProviderKind::KimiCodingCn,
        ProviderKind::Stepfun,
        ProviderKind::Minimax,
        ProviderKind::MinimaxCn,
        ProviderKind::Alibaba,
        ProviderKind::OllamaCloud,
        ProviderKind::Arcee,
        ProviderKind::Kilocode,
        ProviderKind::OpencodeZen,
        ProviderKind::OpencodeGo,
        ProviderKind::Bedrock,
        ProviderKind::AzureFoundry,
        ProviderKind::Custom,
    ]
}

pub fn setup_provider_descriptors() -> Vec<ProviderDescriptor> {
    all_provider_kinds()
        .iter()
        .copied()
        .filter(|provider| provider.is_setup_wired())
        .map(|provider| ProviderDescriptor {
            provider,
            name: provider.display_name(),
            description: provider_description(provider),
        })
        .collect()
}

fn provider_description(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Nous => "Nous Portal device-code login",
        ProviderKind::OpenRouter => "OpenRouter OpenAI-compatible gateway",
        ProviderKind::AiGateway => "Vercel AI Gateway OpenAI-compatible endpoint",
        ProviderKind::Anthropic => "Anthropic Messages API",
        ProviderKind::OpenAi => "OpenAI Chat Completions API",
        ProviderKind::OpenAiCodex => "OpenAI Codex Responses runtime",
        ProviderKind::Xiaomi => "Xiaomi MiMo OpenAI-compatible endpoint",
        ProviderKind::Nvidia => "NVIDIA NIM OpenAI-compatible endpoint",
        ProviderKind::HuggingFace => "Hugging Face router",
        ProviderKind::DeepSeek => "DeepSeek official OpenAI-compatible API",
        ProviderKind::Xai => "xAI OpenAI-compatible endpoint",
        ProviderKind::Zai => "Z.ai / GLM OpenAI-compatible endpoint",
        ProviderKind::KimiCoding => "Moonshot/Kimi coding endpoint",
        ProviderKind::KimiCodingCn => "Moonshot/Kimi China endpoint",
        ProviderKind::Stepfun => "StepFun Step Plan endpoint",
        ProviderKind::Minimax => "MiniMax Anthropic-compatible endpoint",
        ProviderKind::MinimaxCn => "MiniMax CN Anthropic-compatible endpoint",
        ProviderKind::Alibaba => "Alibaba DashScope compatible endpoint",
        ProviderKind::OllamaCloud => "Ollama Cloud endpoint",
        ProviderKind::Arcee => "Arcee OpenAI-compatible endpoint",
        ProviderKind::Kilocode => "Kilo Code gateway",
        ProviderKind::OpencodeZen => "OpenCode Zen endpoint",
        ProviderKind::OpencodeGo => "OpenCode Go endpoint",
        ProviderKind::AzureFoundry => "Custom Azure Foundry deployment",
        ProviderKind::Custom => "Custom endpoint",
        _ => "Provider registered but hidden until fully wired",
    }
}

pub fn resolve_runtime_provider(
    session: Option<&SessionRuntimeConfig>,
    override_config: Option<&RuntimeOverride>,
) -> Result<RuntimeProvider> {
    let override_config = override_config.filter(|config| !config.is_empty());
    if override_config.is_none() {
        if let Some(model_id) = session
            .and_then(|config| config.model_id.as_deref())
            .and_then(|model_id| crate::model_config::model_id_from_session(Some(model_id)))
        {
            return crate::model_config::resolve_saved_model_runtime_by_id(&model_id);
        }
        if let Some(runtime) = crate::model_config::resolve_active_saved_model_runtime()? {
            return Ok(runtime);
        }
        bail!("no active model configured; run `duck model`");
    }

    let global = load_global_runtime_config().unwrap_or_default();
    let auth = load_auth_store().unwrap_or_default();

    let provider_name = override_config
        .and_then(|config| non_empty(config.provider.as_deref()))
        .map(str::to_string)
        .or_else(|| session.and_then(|config| config.provider.clone()))
        .or_else(|| global.provider.clone())
        .or_else(infer_provider_from_env)
        .ok_or_else(|| {
            anyhow!(
                "unable to resolve provider from CLI/session/config/auth/environment; setup required"
            )
        })?;
    let provider = ProviderKind::parse(&provider_name)
        .with_context(|| format!("unsupported provider: {provider_name}"))?;
    let auth_entry = auth.providers.get(provider.as_str());
    let session_matches_provider = session
        .and_then(|config| config.provider.as_deref())
        .is_some_and(|owner| provider_name_matches(owner, provider));
    let global_matches_provider = global
        .provider
        .as_deref()
        .is_some_and(|owner| provider_name_matches(owner, provider));

    let model = override_config
        .and_then(|config| non_empty(config.model.as_deref()))
        .map(str::to_string)
        .or_else(|| {
            session
                .filter(|_| session_matches_provider)
                .and_then(|config| config.model.clone())
        })
        .or_else(|| {
            global_matches_provider
                .then(|| global.model.clone())
                .flatten()
        })
        .or_else(|| read_env_any(provider.model_env_keys()))
        .or_else(|| curated_provider_models(provider).first().cloned())
        .ok_or_else(|| anyhow!("missing model for provider {}", provider.as_str()))?;

    let resolved_credentials = resolve_provider_credentials(provider, true).ok().flatten();

    let api_key = override_config
        .and_then(|config| non_empty(config.api_key.as_deref()))
        .map(str::to_string)
        .or_else(|| {
            resolved_credentials
                .as_ref()
                .map(|credentials| credentials.as_api_key())
        })
        .or_else(|| {
            auth_entry.and_then(|entry| {
                entry
                    .api_key
                    .clone()
                    .or_else(|| entry.token.clone())
                    .filter(|value| !value.trim().is_empty())
            })
        })
        .or_else(|| read_env_any(provider.api_key_env_keys()))
        .unwrap_or_default();

    let base_url = override_config
        .and_then(|config| non_empty(config.base_url.as_deref()))
        .map(str::to_string)
        .or_else(|| {
            session
                .filter(|_| session_matches_provider)
                .and_then(|config| config.base_url.clone())
        })
        .or_else(|| auth_entry.and_then(|entry| entry.base_url.clone()))
        .or_else(|| {
            resolved_credentials
                .as_ref()
                .and_then(|credentials| credentials.base_url.clone())
        })
        .or_else(|| {
            global_matches_provider
                .then(|| global.base_url.clone())
                .flatten()
        })
        .or_else(|| read_env_any(provider.base_url_env_keys()))
        .or_else(|| kimi_coding_base_url_from_key(provider, &api_key))
        .or_else(|| provider.default_base_url().map(str::to_string))
        .ok_or_else(|| anyhow!("missing base URL for provider {}", provider.as_str()))?;

    let configured_api_mode = override_config
        .and_then(|config| config.api_mode)
        .or_else(|| {
            session
                .filter(|_| session_matches_provider)
                .and_then(|config| config.api_mode)
        })
        .or_else(|| global_matches_provider.then_some(global.api_mode).flatten());
    let api_mode = resolve_provider_api_mode(provider, &model, &base_url, configured_api_mode);
    let missing_secret = api_key.is_empty()
        && provider.requires_secret()
        && !matches!(provider, ProviderKind::Custom | ProviderKind::AzureFoundry);
    if missing_secret {
        bail!("missing API key/token for provider {}", provider.as_str());
    }

    let source = if override_config.is_some() {
        "override"
    } else if session.is_some() && session.is_some_and(|config| !config.is_empty()) {
        "session"
    } else if global.provider.is_some() || auth_entry.is_some() {
        "config"
    } else {
        "env"
    };

    Ok(RuntimeProvider {
        model_id: None,
        provider,
        model,
        base_url,
        api_key,
        api_mode,
        source: source.to_string(),
        account_id: None,
    })
}

pub fn resolve_provider_api_mode(
    provider: ProviderKind,
    model: &str,
    base_url: &str,
    configured: Option<ApiMode>,
) -> ApiMode {
    match provider {
        ProviderKind::OpenAiCodex => return ApiMode::CodexResponses,
        ProviderKind::Gemini => return ApiMode::GeminiNative,
        ProviderKind::GoogleGeminiCli => return ApiMode::GeminiCloudcode,
        ProviderKind::Bedrock => return ApiMode::BedrockConverse,
        ProviderKind::CopilotAcp => return ApiMode::CopilotAcp,
        ProviderKind::Minimax | ProviderKind::MinimaxCn => return ApiMode::AnthropicMessages,
        _ => {}
    }

    if let Some(api_mode) = provider_model_api_mode(provider, model, base_url) {
        return api_mode;
    }

    if let Some(api_mode) = configured {
        return api_mode;
    }

    infer_api_mode_from_base_url(base_url).unwrap_or_else(|| provider.default_api_mode())
}

fn provider_model_api_mode(provider: ProviderKind, model: &str, base_url: &str) -> Option<ApiMode> {
    let model = normalize_provider_model_id(provider, model).to_ascii_lowercase();
    match provider {
        // `codex_responses` is the Responses wire transport, not a synonym for
        // the OpenAI Codex OAuth provider. OpenCode Zen exposes GPT-family
        // models through /v1/responses while sharing the OpenCode API key/base
        // URL, so stale persisted api_mode must not override this routing.
        ProviderKind::OpencodeZen if model.starts_with("gpt-") => Some(ApiMode::CodexResponses),
        ProviderKind::OpencodeZen if model.starts_with("claude-") => {
            Some(ApiMode::AnthropicMessages)
        }
        ProviderKind::OpencodeZen => Some(ApiMode::ChatCompletions),
        ProviderKind::OpencodeGo if model.starts_with("minimax-") => {
            Some(ApiMode::AnthropicMessages)
        }
        ProviderKind::OpencodeGo => Some(ApiMode::ChatCompletions),
        ProviderKind::KimiCoding if is_kimi_coding_plan_base_url(base_url) => {
            Some(ApiMode::AnthropicMessages)
        }
        _ => None,
    }
}

fn normalize_provider_model_id(provider: ProviderKind, model: &str) -> String {
    let trimmed = model.trim();
    let prefix = format!("{}/", provider.as_str());
    if trimmed
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(&prefix))
    {
        trimmed[prefix.len()..].to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn save_provider_auth(
    provider: ProviderKind,
    api_key: Option<String>,
    base_url: Option<String>,
) -> Result<()> {
    let Some(api_key) = api_key.filter(|value| !value.trim().is_empty()) else {
        return Ok(());
    };
    save_provider_credentials(
        provider,
        crate::auth::ProviderCredentials {
            api_key: None,
            token: api_key,
            refresh_token: None,
            expires_at: None,
            agent_key_expires_at: None,
            project_id: None,
            source: "setup".to_string(),
            source_path: None,
            ownership: None,
            base_url,
            portal_base_url: None,
            client_id: None,
            scope: None,
        },
    )
}

pub fn fetch_provider_models(runtime: &RuntimeProvider) -> Result<Vec<String>> {
    Ok(fetch_provider_model_catalog(runtime)?
        .keys()
        .cloned()
        .collect())
}

pub fn fetch_provider_model_catalog(
    runtime: &RuntimeProvider,
) -> Result<BTreeMap<String, ModelCapabilities>> {
    let mut catalog = match model_catalog_strategy(runtime.provider) {
        ModelCatalogStrategy::ModelsDev => Ok(models_dev_catalog_until_data(runtime.provider)),
        ModelCatalogStrategy::IndependentProvider => provider_independent_catalog(runtime),
        ModelCatalogStrategy::ManualOrProvider => Ok(manual_or_cached_provider_catalog(runtime)),
    }?;
    apply_configured_model_capability_overrides(runtime.provider.as_str(), &mut catalog)?;
    Ok(catalog)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelCatalogStrategy {
    ModelsDev,
    IndependentProvider,
    ManualOrProvider,
}

fn model_catalog_strategy(provider: ProviderKind) -> ModelCatalogStrategy {
    match provider {
        // Codex is intentionally provider-owned: OpenAI's public models.dev
        // entry describes API models, not ChatGPT Codex OAuth model limits.
        ProviderKind::OpenAiCodex | ProviderKind::Nous | ProviderKind::Arcee => {
            ModelCatalogStrategy::IndependentProvider
        }
        ProviderKind::Custom | ProviderKind::AzureFoundry => ModelCatalogStrategy::ManualOrProvider,
        _ if models_dev_provider_id(provider.as_str()).is_some() => ModelCatalogStrategy::ModelsDev,
        _ => ModelCatalogStrategy::IndependentProvider,
    }
}

fn provider_independent_catalog(
    runtime: &RuntimeProvider,
) -> Result<BTreeMap<String, ModelCapabilities>> {
    let mut catalog = curated_provider_catalog(runtime.provider);

    if let Ok(cache_path) = provider_models_cache_path(runtime) {
        if let Ok(Some(cache)) = read_provider_models_cache(&cache_path) {
            merge_provider_cache(&mut catalog, cache);
            refresh_provider_models_cache_background(runtime.clone(), cache_path);
            return Ok(catalog);
        }
    }

    if !catalog.is_empty() {
        if let Ok(cache_path) = provider_models_cache_path(runtime) {
            refresh_provider_models_cache_background(runtime.clone(), cache_path);
        }
        return Ok(catalog);
    }

    match fetch_provider_models_live_catalog(runtime) {
        Ok(cache) => {
            if let Ok(cache_path) = provider_models_cache_path(runtime) {
                write_provider_models_cache(&cache_path, &cache);
            }
            merge_provider_cache(&mut catalog, cache);
            Ok(catalog)
        }
        Err(error) => Err(error),
    }
}

fn curated_provider_catalog(provider: ProviderKind) -> BTreeMap<String, ModelCapabilities> {
    let mut catalog = BTreeMap::new();
    for model in curated_provider_models(provider) {
        let capabilities = curated_model_capabilities(provider, &model).unwrap_or_default();
        catalog.entry(model).or_insert(capabilities);
    }
    catalog
}

fn manual_or_cached_provider_catalog(
    runtime: &RuntimeProvider,
) -> BTreeMap<String, ModelCapabilities> {
    if let Ok(cache_path) = provider_models_cache_path(runtime) {
        if let Ok(Some(cache)) = read_provider_models_cache(&cache_path) {
            refresh_provider_models_cache_background(runtime.clone(), cache_path);
            return provider_cache_to_catalog(cache);
        }
        refresh_provider_models_cache_background(runtime.clone(), cache_path);
    }
    BTreeMap::new()
}

fn merge_provider_cache(
    catalog: &mut BTreeMap<String, ModelCapabilities>,
    cache: ProviderModelsCache,
) {
    for model in cache.models {
        catalog.entry(model).or_default();
    }
    for (model, capabilities) in cache.capabilities {
        catalog
            .entry(model)
            .and_modify(|existing| merge_model_capabilities(existing, capabilities.clone()))
            .or_insert(capabilities);
    }
}

fn provider_cache_to_catalog(cache: ProviderModelsCache) -> BTreeMap<String, ModelCapabilities> {
    let mut catalog = BTreeMap::new();
    merge_provider_cache(&mut catalog, cache);
    catalog
}

fn models_dev_catalog_until_data(provider: ProviderKind) -> BTreeMap<String, ModelCapabilities> {
    let mut attempt = 0usize;
    loop {
        match load_cached_or_fetch_models_dev_json() {
            Ok(models_dev) => {
                refresh_models_dev_cache_background();
                let catalog = models_dev_catalog_from_value(provider, &models_dev);
                if !catalog.is_empty() {
                    return catalog;
                }
                let delay = MODEL_REFRESH_BACKOFFS
                    .get(attempt)
                    .copied()
                    .unwrap_or(*MODEL_REFRESH_BACKOFFS.last().unwrap());
                attempt = attempt.saturating_add(1);
                std::thread::sleep(delay);
            }
            Err(_) => {
                let delay = MODEL_REFRESH_BACKOFFS
                    .get(attempt)
                    .copied()
                    .unwrap_or(*MODEL_REFRESH_BACKOFFS.last().unwrap());
                attempt = attempt.saturating_add(1);
                std::thread::sleep(delay);
            }
        }
    }
}

fn models_dev_catalog_from_value(
    provider: ProviderKind,
    models_dev: &Value,
) -> BTreeMap<String, ModelCapabilities> {
    let mut catalog = curated_provider_catalog(provider);
    let Some(provider_id) = models_dev_provider_id(provider.as_str()) else {
        return catalog;
    };
    let Some(models) = models_dev
        .get(provider_id)
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    else {
        return catalog;
    };
    for (model, entry) in models {
        catalog.insert(model.clone(), capabilities_from_models_dev_entry(entry));
    }
    catalog
}

pub fn get_model_capabilities(provider: &str, model: &str) -> Result<Option<ModelCapabilities>> {
    let models_dev = load_models_dev_json()?;
    lookup_model_capabilities(&models_dev, provider, model)
}

pub fn get_cached_model_capabilities(
    provider: &str,
    model: &str,
) -> Result<Option<ModelCapabilities>> {
    let cache_path = models_dev_cache_path()?;
    let cached = match fs::read_to_string(&cache_path) {
        Ok(cached) => cached,
        Err(error) => {
            let configured = load_model_capabilities_override(provider, model)?;
            if configured.is_some() {
                return Ok(configured);
            }
            return Err(error).with_context(|| {
                format!("failed to read models.dev cache: {}", cache_path.display())
            });
        }
    };
    let models_dev = serde_json::from_str(&cached).with_context(|| {
        format!(
            "failed to parse cached models.dev JSON: {}",
            cache_path.display()
        )
    })?;
    lookup_model_capabilities(&models_dev, provider, model)
}

pub fn get_cached_model_context_window(provider: &str, model: &str) -> Option<u64> {
    get_cached_model_capabilities(provider, model)
        .ok()
        .flatten()
        .and_then(|capabilities| capabilities.context_window)
}

pub fn resolve_runtime_context_window(runtime: &RuntimeProvider) -> u64 {
    load_model_capabilities_override(runtime.provider.as_str(), &runtime.model)
        .ok()
        .flatten()
        .and_then(|capabilities| capabilities.context_window)
        .or_else(|| get_cached_model_context_window(runtime.provider.as_str(), &runtime.model))
        .unwrap_or(UNKNOWN_CONTEXT_WINDOW_FALLBACK)
}

pub fn refresh_models_dev_cache_background() {
    if MODELS_DEV_REFRESH_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let Ok(cache_path) = models_dev_cache_path() else {
            MODELS_DEV_REFRESH_RUNNING.store(false, Ordering::SeqCst);
            return;
        };
        let mut attempt = 0usize;
        loop {
            match fetch_models_dev_json_from_network(&cache_path) {
                Ok(_) => {
                    MODELS_DEV_REFRESH_RUNNING.store(false, Ordering::SeqCst);
                    return;
                }
                Err(_) => {
                    let delay = MODEL_REFRESH_BACKOFFS
                        .get(attempt)
                        .copied()
                        .unwrap_or(*MODEL_REFRESH_BACKOFFS.last().unwrap());
                    attempt = attempt.saturating_add(1);
                    std::thread::sleep(delay);
                }
            }
        }
    });
}

fn lookup_model_capabilities(
    models_dev: &Value,
    provider: &str,
    model: &str,
) -> Result<Option<ModelCapabilities>> {
    let mut resolved = None;
    if let Some(provider_kind) = ProviderKind::parse(provider) {
        if let Some(capabilities) = curated_model_capabilities(provider_kind, model) {
            resolved = Some(capabilities);
        }
    }
    if resolved.is_none() {
        let provider_id = models_dev_provider_id(provider).unwrap_or(provider);
        if let Some(entry) = find_model_entry(models_dev, provider_id, model) {
            resolved = Some(capabilities_from_models_dev_entry(entry));
        }
    }
    if resolved.is_none() && matches!(provider, "custom" | "azure-foundry") {
        resolved = find_global_exact_model_capabilities(models_dev, model);
    }
    if let Some(configured) = load_model_capabilities_override(provider, model)? {
        match resolved.as_mut() {
            Some(capabilities) => override_model_capabilities(capabilities, configured),
            None => resolved = Some(configured),
        }
    }
    Ok(resolved)
}

fn capabilities_from_models_dev_entry(entry: &Value) -> ModelCapabilities {
    let modalities_input = entry
        .get("modalities")
        .and_then(|modalities| modalities.get("input"))
        .and_then(Value::as_array);
    let has_vision_modality = modalities_input
        .map(|items| {
            items.iter().any(|item| {
                item.as_str()
                    .is_some_and(|value| matches!(value, "image" | "video" | "pdf"))
            })
        })
        .unwrap_or(false);

    ModelCapabilities {
        context_window: entry
            .get("limit")
            .and_then(|limit| limit.get("context"))
            .and_then(Value::as_u64)
            .or_else(|| entry.get("context_window").and_then(Value::as_u64)),
        max_output_tokens: entry
            .get("limit")
            .and_then(|limit| limit.get("output"))
            .and_then(Value::as_u64)
            .or_else(|| entry.get("max_output_tokens").and_then(Value::as_u64)),
        input_cost: entry
            .get("cost")
            .and_then(|cost| cost.get("input"))
            .and_then(Value::as_f64),
        output_cost: entry
            .get("cost")
            .and_then(|cost| cost.get("output"))
            .and_then(Value::as_f64),
        supports_reasoning: entry
            .get("reasoning")
            .and_then(Value::as_bool)
            .or_else(|| entry.get("supports_reasoning").and_then(Value::as_bool)),
        supports_tools: entry
            .get("tool_call")
            .and_then(Value::as_bool)
            .or_else(|| entry.get("supports_tools").and_then(Value::as_bool)),
        supports_vision: entry
            .get("attachment")
            .and_then(Value::as_bool)
            .or(Some(has_vision_modality)),
        family: entry
            .get("family")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn find_global_exact_model_capabilities(
    models_dev: &Value,
    model: &str,
) -> Option<ModelCapabilities> {
    let providers = models_dev.as_object()?;
    let mut best: Option<ModelCapabilities> = None;
    for provider_value in providers.values() {
        let Some(models) = provider_value.get("models").and_then(Value::as_object) else {
            continue;
        };
        let Some(entry) = models.get(model).or_else(|| {
            models
                .iter()
                .find(|(id, _)| id.eq_ignore_ascii_case(model))
                .map(|(_, value)| value)
        }) else {
            continue;
        };
        let candidate = capabilities_from_models_dev_entry(entry);
        if candidate.context_window.is_none() {
            continue;
        }
        best = match best {
            Some(mut existing) => {
                if candidate.context_window < existing.context_window {
                    existing.context_window = candidate.context_window;
                }
                Some(existing)
            }
            None => Some(candidate),
        };
    }
    best
}

fn find_model_entry<'a>(
    models_dev: &'a Value,
    provider_id: &str,
    model: &str,
) -> Option<&'a Value> {
    let provider = models_dev.get(provider_id)?;
    let models = provider.get("models").and_then(Value::as_object)?;
    models.get(model).or_else(|| {
        models
            .iter()
            .find(|(id, value)| {
                id.eq_ignore_ascii_case(model)
                    || value
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id.eq_ignore_ascii_case(model))
            })
            .map(|(_, value)| value)
    })
}

fn models_dev_provider_id(provider: &str) -> Option<&'static str> {
    match provider {
        "openrouter" => Some("openrouter"),
        "ai-gateway" => Some("vercel"),
        "anthropic" => Some("anthropic"),
        "openai" => Some("openai"),
        "xiaomi" => Some("xiaomi"),
        "nvidia" => Some("nvidia"),
        "qwen-oauth" | "alibaba" => Some("alibaba"),
        "copilot" | "copilot-acp" => Some("github-copilot"),
        "huggingface" => Some("huggingface"),
        "gemini" | "google-gemini-cli" => Some("google"),
        "deepseek" => Some("deepseek"),
        "xai" => Some("xai"),
        "zai" => Some("zai"),
        "kimi-coding" | "kimi-coding-cn" => Some("kimi-for-coding"),
        "stepfun" => Some("stepfun"),
        "minimax" => Some("minimax"),
        "minimax-cn" => Some("minimax-cn"),
        "ollama-cloud" => Some("ollama-cloud"),
        "opencode-zen" => Some("opencode"),
        "opencode-go" => Some("opencode-go"),
        "kilocode" => Some("kilo"),
        "bedrock" => Some("amazon-bedrock"),
        _ => None,
    }
}

fn infer_provider_from_env() -> Option<String> {
    for provider in all_provider_kinds()
        .iter()
        .filter(|provider| provider.is_setup_wired())
    {
        if read_env_any(provider.api_key_env_keys()).is_some() {
            return Some(provider.as_str().to_string());
        }
    }

    if read_env_any(&["CUSTOM_BASE_URL", "OPENAI_BASE_URL"]).is_some() {
        return Some(ProviderKind::Custom.as_str().to_string());
    }

    None
}

fn infer_api_mode_from_base_url(base_url: &str) -> Option<ApiMode> {
    let lowered = base_url.to_ascii_lowercase();
    if lowered.contains("anthropic") {
        Some(ApiMode::AnthropicMessages)
    } else if lowered.contains("responses") || lowered.contains("codex") {
        Some(ApiMode::CodexResponses)
    } else {
        None
    }
}

fn kimi_coding_base_url_from_key(provider: ProviderKind, api_key: &str) -> Option<String> {
    if provider == ProviderKind::KimiCoding
        && api_key.trim_start().starts_with("sk-kimi-")
        && read_env_any(&["KIMI_BASE_URL"]).is_none()
    {
        // Kimi Coding Plan keys use api.kimi.com/coding, which speaks the
        // Anthropic Messages protocol. Do not append /v1 here: the Anthropic
        // request URL builder appends /v1/messages, yielding /coding/v1/messages.
        return Some("https://api.kimi.com/coding".to_string());
    }
    None
}

fn is_kimi_coding_plan_base_url(base_url: &str) -> bool {
    let trimmed = base_url.trim().trim_end_matches('/').to_ascii_lowercase();
    trimmed.contains("://api.kimi.com") && trimmed.contains("/coding")
}

fn read_env_any(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        env::var(key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn provider_name_matches(value: &str, provider: ProviderKind) -> bool {
    ProviderKind::parse(value).is_some_and(|parsed| parsed == provider)
}

fn refresh_provider_models_cache_background(runtime: RuntimeProvider, cache_path: PathBuf) {
    std::thread::spawn(move || provider_models_retry_loop(runtime, cache_path));
}

fn provider_models_retry_loop(runtime: RuntimeProvider, cache_path: PathBuf) {
    let mut attempt = 0usize;
    loop {
        match fetch_provider_models_live_catalog(&runtime) {
            Ok(cache) => {
                write_provider_models_cache(&cache_path, &cache);
                return;
            }
            Err(_) => {
                let delay = MODEL_REFRESH_BACKOFFS
                    .get(attempt)
                    .copied()
                    .unwrap_or(*MODEL_REFRESH_BACKOFFS.last().unwrap());
                attempt = attempt.saturating_add(1);
                std::thread::sleep(delay);
            }
        }
    }
}

fn fetch_provider_models_live_catalog(runtime: &RuntimeProvider) -> Result<ProviderModelsCache> {
    let models_urls = provider_models_urls(&runtime.base_url)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("failed to build reqwest client for provider model listing")?;

    let mut last_error: Option<anyhow::Error> = None;
    for models_url in models_urls {
        let mut request = client.get(&models_url);
        if !runtime.api_key.is_empty() {
            request = request.bearer_auth(&runtime.api_key);
        }
        if matches!(
            runtime.provider,
            ProviderKind::Anthropic | ProviderKind::Minimax | ProviderKind::MinimaxCn
        ) {
            request = request.header("x-api-key", &runtime.api_key);
            request = request.header("anthropic-version", "2023-06-01");
        }
        let response = match request.send() {
            Ok(response) => response,
            Err(error) => {
                last_error = Some(anyhow!(
                    "failed to fetch provider models from {models_url}: {error}"
                ));
                continue;
            }
        };
        let response = match response.error_for_status() {
            Ok(response) => response,
            Err(error) => {
                last_error = Some(anyhow!(
                    "provider model listing returned error for {models_url}: {error}"
                ));
                continue;
            }
        };
        let payload: Value = response.json().with_context(|| {
            format!("failed to parse provider model listing response: {models_url}")
        })?;
        return Ok(extract_provider_models_cache(&payload));
    }
    Err(last_error.unwrap_or_else(|| anyhow!("no provider model listing URLs were available")))
}

fn provider_models_urls(base_url: &str) -> Result<Vec<String>> {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/models") {
        return Ok(vec![trimmed.to_string()]);
    }
    let primary = if trimmed.ends_with("/chat/completions") {
        format!("{}/models", trimmed.trim_end_matches("/chat/completions"))
    } else if trimmed.ends_with("/responses") {
        format!("{}/models", trimmed.trim_end_matches("/responses"))
    } else {
        format!("{trimmed}/models")
    };

    let mut urls = vec![primary.clone()];
    if !trimmed.ends_with("/v1") && !trimmed.contains("/v1/") && !primary.ends_with("/v1/models") {
        urls.push(format!("{trimmed}/v1/models"));
    }
    Ok(urls)
}

#[derive(Debug, Serialize, Deserialize)]
struct ProviderModelsCache {
    models: Vec<String>,
    #[serde(default)]
    capabilities: BTreeMap<String, ModelCapabilities>,
}

fn read_provider_models_cache(cache_path: &PathBuf) -> Result<Option<ProviderModelsCache>> {
    let Ok(cached) = fs::read_to_string(cache_path) else {
        return Ok(None);
    };
    let cache: ProviderModelsCache =
        serde_json::from_str(&cached).context("failed to parse provider models cache")?;
    Ok(Some(cache))
}

fn write_provider_models_cache(cache_path: &PathBuf, cache: &ProviderModelsCache) {
    if let Some(parent) = cache_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(&cache) {
        let _ = fs::write(cache_path, text);
    }
}

fn provider_models_cache_path(runtime: &RuntimeProvider) -> Result<PathBuf> {
    let dir = duckagent_path(PROVIDER_MODELS_CACHE_DIR_RELATIVE_PATH)?;
    let key = format!(
        "{}-{}",
        runtime.provider.as_str(),
        sanitize_cache_key(&runtime.base_url)
    );
    Ok(dir.join(format!("{key}.json")))
}

fn sanitize_cache_key(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn extract_provider_models_cache(payload: &Value) -> ProviderModelsCache {
    let mut models = Vec::new();
    let mut capabilities = BTreeMap::new();
    if let Some(items) = payload.get("data").and_then(Value::as_array) {
        for item in items {
            let Some(model) = item
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| item.get("name").and_then(Value::as_str).map(str::to_string))
            else {
                continue;
            };
            models.push(model.clone());
            let model_capabilities = capabilities_from_provider_model_entry(item);
            if !model_capabilities.is_empty() {
                capabilities.insert(model, model_capabilities);
            }
        }
    }
    ProviderModelsCache {
        models,
        capabilities,
    }
}

fn capabilities_from_provider_model_entry(entry: &Value) -> ModelCapabilities {
    let context_window = entry
        .get("context_length")
        .and_then(Value::as_u64)
        .or_else(|| {
            entry
                .get("top_provider")
                .and_then(|provider| provider.get("context_length"))
                .and_then(Value::as_u64)
        });
    let max_output_tokens = entry
        .get("top_provider")
        .and_then(|provider| provider.get("max_completion_tokens"))
        .and_then(Value::as_u64);
    let input_cost = entry
        .get("pricing")
        .and_then(|pricing| pricing.get("prompt"))
        .and_then(parse_json_f64)
        .map(openrouter_per_token_to_per_million_tokens);
    let output_cost = entry
        .get("pricing")
        .and_then(|pricing| pricing.get("completion"))
        .and_then(parse_json_f64)
        .map(openrouter_per_token_to_per_million_tokens);
    let supported_parameters = entry.get("supported_parameters").and_then(Value::as_array);
    ModelCapabilities {
        context_window,
        max_output_tokens,
        input_cost,
        output_cost,
        supports_reasoning: supported_parameters.map(|parameters| {
            parameters.iter().any(|parameter| {
                parameter
                    .as_str()
                    .is_some_and(|value| matches!(value, "reasoning" | "include_reasoning"))
            })
        }),
        supports_tools: supported_parameters.map(|parameters| {
            parameters
                .iter()
                .any(|parameter| parameter.as_str().is_some_and(|value| value == "tools"))
        }),
        supports_vision: entry
            .get("architecture")
            .and_then(|architecture| architecture.get("input_modalities"))
            .and_then(Value::as_array)
            .map(|modalities| {
                modalities.iter().any(|modality| {
                    modality
                        .as_str()
                        .is_some_and(|value| matches!(value, "image" | "video" | "audio" | "pdf"))
                })
            }),
        family: None,
    }
}

fn parse_json_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
}

fn openrouter_per_token_to_per_million_tokens(value: f64) -> f64 {
    value * 1_000_000.0
}

fn merge_model_capabilities(existing: &mut ModelCapabilities, incoming: ModelCapabilities) {
    existing.context_window = existing.context_window.or(incoming.context_window);
    existing.max_output_tokens = existing.max_output_tokens.or(incoming.max_output_tokens);
    existing.input_cost = existing.input_cost.or(incoming.input_cost);
    existing.output_cost = existing.output_cost.or(incoming.output_cost);
    existing.supports_reasoning = existing.supports_reasoning.or(incoming.supports_reasoning);
    existing.supports_tools = existing.supports_tools.or(incoming.supports_tools);
    existing.supports_vision = existing.supports_vision.or(incoming.supports_vision);
    existing.family = existing.family.clone().or(incoming.family);
}

fn override_model_capabilities(existing: &mut ModelCapabilities, incoming: ModelCapabilities) {
    if incoming.context_window.is_some() {
        existing.context_window = incoming.context_window;
    }
    if incoming.max_output_tokens.is_some() {
        existing.max_output_tokens = incoming.max_output_tokens;
    }
    if incoming.input_cost.is_some() {
        existing.input_cost = incoming.input_cost;
    }
    if incoming.output_cost.is_some() {
        existing.output_cost = incoming.output_cost;
    }
    if incoming.supports_reasoning.is_some() {
        existing.supports_reasoning = incoming.supports_reasoning;
    }
    if incoming.supports_tools.is_some() {
        existing.supports_tools = incoming.supports_tools;
    }
    if incoming.supports_vision.is_some() {
        existing.supports_vision = incoming.supports_vision;
    }
    if incoming.family.is_some() {
        existing.family = incoming.family;
    }
}

fn apply_configured_model_capability_overrides(
    provider: &str,
    catalog: &mut BTreeMap<String, ModelCapabilities>,
) -> Result<()> {
    for (model, configured) in load_provider_model_capability_overrides(provider)? {
        catalog
            .entry(model)
            .and_modify(|existing| override_model_capabilities(existing, configured.clone()))
            .or_insert(configured);
    }
    Ok(())
}

impl ModelCapabilities {
    fn is_empty(&self) -> bool {
        self.context_window.is_none()
            && self.max_output_tokens.is_none()
            && self.input_cost.is_none()
            && self.output_cost.is_none()
            && self.supports_reasoning.is_none()
            && self.supports_tools.is_none()
            && self.supports_vision.is_none()
            && self.family.is_none()
    }
}

fn curated_provider_models(provider: ProviderKind) -> Vec<String> {
    let models: &[&str] = match provider {
        ProviderKind::Nous => &[
            "moonshotai/kimi-k2.6",
            "deepseek/deepseek-v4-pro",
            "deepseek/deepseek-v4-flash",
            "anthropic/claude-sonnet-4.5",
            "openai/gpt-5.3-codex",
            "google/gemini-3-pro-preview",
            "minimax/minimax-m2.7",
            "z-ai/glm-5.1",
        ],
        ProviderKind::OpenAi => &["gpt-4.1", "gpt-4.1-mini", "gpt-4o", "gpt-4o-mini"],
        ProviderKind::OpenAiCodex => &[
            "gpt-5.3-codex",
            "gpt-5.2-codex",
            "gpt-5.1-codex",
            "gpt-5.1-codex-max",
            "gpt-5.1-codex-mini",
            "gpt-5-codex",
        ],
        ProviderKind::DeepSeek => &["deepseek-v4-flash", "deepseek-v4-pro", "deepseek-chat"],
        ProviderKind::Anthropic => &[
            "claude-sonnet-4-5",
            "claude-opus-4-5",
            "claude-3-5-haiku-20241022",
        ],
        ProviderKind::OpenRouter => &["openai/gpt-4.1", "anthropic/claude-sonnet-4.5"],
        ProviderKind::Copilot | ProviderKind::CopilotAcp => &[
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5-mini",
            "gpt-5.3-codex",
            "gpt-5.2-codex",
            "gpt-4.1",
            "gpt-4o",
            "gpt-4o-mini",
            "claude-sonnet-4.6",
        ],
        ProviderKind::KimiCoding => &["kimi-k2-turbo-preview", "kimi-k2-0905-preview"],
        ProviderKind::KimiCodingCn => &["kimi-k2-turbo-preview", "kimi-k2-0905-preview"],
        ProviderKind::Zai => &["glm-4.6", "glm-4.5", "glm-4.5-air"],
        ProviderKind::Minimax | ProviderKind::MinimaxCn => &["minimax-m2", "MiniMax-M2"],
        ProviderKind::Alibaba => &["qwen-max", "qwen-plus", "qwen-turbo"],
        ProviderKind::Xai => &["grok-4", "grok-3"],
        ProviderKind::Nvidia => &["nvidia/llama-3.3-nemotron-super-49b-v1"],
        ProviderKind::Stepfun => &["step-3", "step-2-mini"],
        ProviderKind::Arcee => &[
            "trinity-large-thinking",
            "trinity-large-preview",
            "trinity-mini",
        ],
        ProviderKind::OpencodeZen => &["gpt-5-codex", "claude-sonnet-4-5"],
        ProviderKind::OpencodeGo => &["minimax-m2", "glm-4.6", "kimi-k2-turbo-preview"],
        _ => &[],
    };
    models.iter().map(|value| value.to_string()).collect()
}

fn curated_model_capabilities(provider: ProviderKind, model: &str) -> Option<ModelCapabilities> {
    match provider {
        ProviderKind::OpenAiCodex
            if curated_provider_models(provider)
                .iter()
                .any(|known| known.eq_ignore_ascii_case(model)) =>
        {
            Some(ModelCapabilities {
                context_window: Some(272_000),
                supports_reasoning: Some(true),
                supports_tools: Some(true),
                ..ModelCapabilities::default()
            })
        }
        _ => None,
    }
}

fn load_models_dev_json() -> Result<Value> {
    load_cached_or_fetch_models_dev_json()
}

fn load_cached_or_fetch_models_dev_json() -> Result<Value> {
    let cache_path = models_dev_cache_path()?;
    if let Ok(cached) = fs::read_to_string(&cache_path) {
        let value = serde_json::from_str(&cached).with_context(|| {
            format!(
                "failed to parse cached models.dev JSON: {}",
                cache_path.display()
            )
        })?;
        refresh_models_dev_cache_background();
        return Ok(value);
    }

    fetch_models_dev_json_from_network(&cache_path)
}

fn fetch_models_dev_json_from_network(cache_path: &PathBuf) -> Result<Value> {
    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("failed to build reqwest client for models.dev")?;
    let text = client
        .get(MODELS_DEV_URL)
        .send()
        .context("failed to request models.dev")?
        .error_for_status()
        .context("models.dev returned error")?
        .text()
        .context("failed to read models.dev response body")?;
    if let Some(parent) = cache_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&cache_path, &text);
    let value = serde_json::from_str(&text).context("failed to parse models.dev JSON")?;
    Ok(value)
}

fn duckagent_path(relative: &str) -> Result<PathBuf> {
    profiles::active_profile_path(relative)
}

fn models_dev_cache_path() -> Result<PathBuf> {
    duckagent_path(MODELS_DEV_CACHE_RELATIVE_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_aliases_resolve() {
        assert_eq!(
            ProviderKind::parse("deep-seek"),
            Some(ProviderKind::DeepSeek)
        );
        assert_eq!(
            ProviderKind::parse("codex"),
            Some(ProviderKind::OpenAiCodex)
        );
        assert_eq!(ProviderKind::parse("hf"), Some(ProviderKind::HuggingFace));
        assert_eq!(
            ProviderKind::parse("qwen-oauth"),
            Some(ProviderKind::QwenOauth)
        );
    }

    #[test]
    fn provider_defaults_match_expected_modes() {
        assert_eq!(
            ProviderKind::DeepSeek.default_api_mode(),
            ApiMode::ChatCompletions
        );
        assert_eq!(
            ProviderKind::Anthropic.default_api_mode(),
            ApiMode::AnthropicMessages
        );
        assert_eq!(
            ProviderKind::OpenAiCodex.default_api_mode(),
            ApiMode::CodexResponses
        );
        assert_eq!(
            ProviderKind::Minimax.default_api_mode(),
            ApiMode::AnthropicMessages
        );
    }

    #[test]
    fn opencode_api_mode_is_model_aware() {
        assert_eq!(
            resolve_provider_api_mode(
                ProviderKind::OpencodeZen,
                "gpt-5.3-codex",
                "https://opencode.ai/zen/v1",
                None
            ),
            ApiMode::CodexResponses
        );
        assert_eq!(
            resolve_provider_api_mode(
                ProviderKind::OpencodeZen,
                "claude-sonnet-4-5",
                "https://opencode.ai/zen/v1",
                None
            ),
            ApiMode::AnthropicMessages
        );
        assert_eq!(
            resolve_provider_api_mode(
                ProviderKind::OpencodeGo,
                "minimax-m2.7",
                "https://opencode.ai/zen/go/v1",
                None
            ),
            ApiMode::AnthropicMessages
        );
        assert_eq!(
            resolve_provider_api_mode(
                ProviderKind::OpencodeGo,
                "kimi-k2.5",
                "https://opencode.ai/zen/go/v1",
                None
            ),
            ApiMode::ChatCompletions
        );
    }

    #[test]
    fn opencode_api_mode_strips_provider_prefix_and_ignores_stale_mode() {
        assert_eq!(
            normalize_provider_model_id(ProviderKind::OpencodeZen, "opencode-zen/gpt-5.4"),
            "gpt-5.4"
        );
        assert_eq!(
            resolve_provider_api_mode(
                ProviderKind::OpencodeZen,
                "opencode-zen/gpt-5.4",
                "https://opencode.ai/zen/v1",
                Some(ApiMode::ChatCompletions),
            ),
            ApiMode::CodexResponses
        );
        assert_eq!(
            resolve_provider_api_mode(
                ProviderKind::OpencodeZen,
                "opencode-zen/claude-sonnet-4-6",
                "https://opencode.ai/zen/v1",
                Some(ApiMode::ChatCompletions),
            ),
            ApiMode::AnthropicMessages
        );
        assert_eq!(
            resolve_provider_api_mode(
                ProviderKind::OpencodeGo,
                "opencode-go/minimax-m2.5",
                "https://opencode.ai/zen/go/v1",
                Some(ApiMode::ChatCompletions),
            ),
            ApiMode::AnthropicMessages
        );
    }

    #[test]
    fn fixed_protocol_providers_ignore_stale_configured_api_mode() {
        assert_eq!(
            resolve_provider_api_mode(
                ProviderKind::OpenAiCodex,
                "gpt-5.3-codex",
                "https://chatgpt.com/backend-api/codex",
                Some(ApiMode::ChatCompletions),
            ),
            ApiMode::CodexResponses
        );
        assert_eq!(
            resolve_provider_api_mode(
                ProviderKind::Gemini,
                "gemini-2.5-pro",
                "https://generativelanguage.googleapis.com/v1beta",
                Some(ApiMode::ChatCompletions),
            ),
            ApiMode::GeminiNative
        );
    }

    #[test]
    fn provider_name_matches_aliases_but_not_other_providers() {
        assert!(provider_name_matches("codex", ProviderKind::OpenAiCodex));
        assert!(provider_name_matches(
            "openai-codex",
            ProviderKind::OpenAiCodex
        ));
        assert!(!provider_name_matches(
            "openrouter",
            ProviderKind::OpenAiCodex
        ));
    }

    #[test]
    fn kimi_coding_plan_key_uses_anthropic_coding_endpoint() {
        let base_url =
            kimi_coding_base_url_from_key(ProviderKind::KimiCoding, "sk-kimi-example").unwrap();
        assert_eq!(base_url, "https://api.kimi.com/coding");
        assert_eq!(
            resolve_provider_api_mode(ProviderKind::KimiCoding, "kimi-k2.6", &base_url, None),
            ApiMode::AnthropicMessages
        );
        assert_eq!(
            resolve_provider_api_mode(
                ProviderKind::KimiCoding,
                "kimi-k2.6",
                &base_url,
                Some(ApiMode::ChatCompletions),
            ),
            ApiMode::AnthropicMessages
        );
    }

    #[test]
    fn provider_models_url_normalizes_common_endpoints() -> Result<()> {
        assert_eq!(
            provider_models_urls("https://api.deepseek.com/v1")?,
            vec!["https://api.deepseek.com/v1/models".to_string()]
        );
        assert_eq!(
            provider_models_urls("https://api.example.com/v1/chat/completions")?,
            vec!["https://api.example.com/v1/models".to_string()]
        );
        assert_eq!(
            provider_models_urls("https://api.example.com")?,
            vec![
                "https://api.example.com/models".to_string(),
                "https://api.example.com/v1/models".to_string()
            ]
        );
        Ok(())
    }

    #[test]
    fn cache_paths_live_under_cache_directory() -> Result<()> {
        let models_dev_path = models_dev_cache_path()?
            .to_string_lossy()
            .replace('\\', "/");
        assert!(models_dev_path.ends_with("cache/models_dev_cache.json"));

        let runtime = RuntimeProvider {
            model_id: None,
            provider: ProviderKind::Custom,
            model: "test-model".to_string(),
            base_url: "https://example.com/v1".to_string(),
            api_key: String::new(),
            api_mode: ApiMode::ChatCompletions,
            source: "test".to_string(),
            account_id: None,
        };
        let provider_cache_path = provider_models_cache_path(&runtime)?
            .to_string_lossy()
            .replace('\\', "/");
        assert!(provider_cache_path.contains("cache/provider_models/"));
        Ok(())
    }

    #[test]
    fn provider_model_entry_extracts_openrouter_metadata() {
        let payload = serde_json::json!({
            "data": [{
                "id": "example/model",
                "context_length": 1000000,
                "pricing": {
                    "prompt": "0.0000004",
                    "completion": "0.0000024"
                },
                "top_provider": {
                    "max_completion_tokens": 65536
                },
                "supported_parameters": ["tools", "reasoning"]
            }]
        });
        let cache = extract_provider_models_cache(&payload);
        let capabilities = cache.capabilities.get("example/model").unwrap();
        assert_eq!(cache.models, vec!["example/model"]);
        assert_eq!(capabilities.context_window, Some(1_000_000));
        assert_eq!(capabilities.max_output_tokens, Some(65_536));
        assert!((capabilities.input_cost.unwrap() - 0.4).abs() < f64::EPSILON * 4.0);
        assert!((capabilities.output_cost.unwrap() - 2.4).abs() < f64::EPSILON * 4.0);
        assert_eq!(capabilities.supports_tools, Some(true));
        assert_eq!(capabilities.supports_reasoning, Some(true));
    }

    #[test]
    fn normal_provider_catalogs_use_models_dev_without_live_probe() {
        assert_eq!(
            model_catalog_strategy(ProviderKind::OpenRouter),
            ModelCatalogStrategy::ModelsDev
        );
        assert_eq!(
            model_catalog_strategy(ProviderKind::DeepSeek),
            ModelCatalogStrategy::ModelsDev
        );
        assert_eq!(
            model_catalog_strategy(ProviderKind::OpenAiCodex),
            ModelCatalogStrategy::IndependentProvider
        );
        assert_eq!(
            models_dev_provider_id("openai-codex"),
            None,
            "Codex OAuth uses ChatGPT Codex model metadata, not OpenAI API models.dev rows"
        );
    }

    #[test]
    fn copilot_models_dev_mapping_is_github_copilot() {
        assert_eq!(models_dev_provider_id("copilot"), Some("github-copilot"));
        assert_eq!(
            models_dev_provider_id("copilot-acp"),
            Some("github-copilot")
        );
    }

    #[test]
    fn codex_curated_catalog_has_provider_owned_context() {
        let catalog = curated_provider_catalog(ProviderKind::OpenAiCodex);
        let capabilities = catalog
            .get("gpt-5.3-codex")
            .expect("curated Codex model should exist");
        assert_eq!(capabilities.context_window, Some(272_000));
        assert_eq!(capabilities.supports_tools, Some(true));
        assert_eq!(capabilities.supports_reasoning, Some(true));
    }

    #[test]
    fn session_runtime_config_reports_empty_state() {
        assert!(SessionRuntimeConfig::default().is_empty());
        assert!(
            !SessionRuntimeConfig {
                provider: Some("deepseek".to_string()),
                ..SessionRuntimeConfig::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn runtime_context_window_falls_back_to_32k_for_unknown_model() {
        let runtime = RuntimeProvider {
            model_id: None,
            provider: ProviderKind::Custom,
            model: "definitely-unlisted-test-model".to_string(),
            base_url: "https://example.com/v1".to_string(),
            api_key: String::new(),
            api_mode: ApiMode::ChatCompletions,
            source: "test".to_string(),
            account_id: None,
        };
        assert_eq!(
            resolve_runtime_context_window(&runtime),
            UNKNOWN_CONTEXT_WINDOW_FALLBACK
        );
    }

    #[test]
    fn models_dev_new_shape_is_parsed() -> Result<()> {
        let payload = serde_json::json!({
            "deepseek": {
                "models": {
                    "deepseek-v4-flash": {
                        "family": "deepseek",
                        "reasoning": true,
                        "tool_call": true,
                        "attachment": false,
                        "limit": { "context": 1000000, "output": 32000 }
                    }
                }
            }
        });
        let capabilities = lookup_model_capabilities(&payload, "deepseek", "deepseek-v4-flash")?
            .expect("capabilities should be found");
        assert_eq!(capabilities.context_window, Some(1_000_000));
        assert_eq!(capabilities.supports_reasoning, Some(true));
        assert_eq!(capabilities.supports_tools, Some(true));
        Ok(())
    }

    #[test]
    fn models_dev_cost_is_parsed() {
        let payload = serde_json::json!({
            "cost": { "input": 0.29, "output": 2.86 },
            "limit": { "context": 128000, "output": 16384 }
        });
        let capabilities = capabilities_from_models_dev_entry(&payload);
        assert_eq!(capabilities.input_cost, Some(0.29));
        assert_eq!(capabilities.output_cost, Some(2.86));
    }
}
