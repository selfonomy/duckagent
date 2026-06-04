use crate::profiles;
use crate::provider::ProviderKind;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const AUTH_FILE_NAME: &str = "auth.json";
const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const CODEX_AUTH_ISSUER: &str = "https://auth.openai.com";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_DEVICE_URL: &str = "https://auth.openai.com/codex/device";
const CODEX_DEVICE_CALLBACK_URL: &str = "https://auth.openai.com/deviceauth/callback";
const QWEN_BASE_URL: &str = "https://portal.qwen.ai/v1";
const GEMINI_CLOUDCODE_BASE_URL: &str = "https://cloudcode-pa.googleapis.com";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_GEMINI_CLIENT_ID_ENV: &str = "DUCKAGENT_GEMINI_CLIENT_ID";
const GOOGLE_GEMINI_CLIENT_SECRET_ENV: &str = "DUCKAGENT_GEMINI_CLIENT_SECRET";
// Google's public gemini-cli desktop OAuth client. Desktop OAuth clients use
// PKCE for security; these values are not confidential and can be overridden.
const GOOGLE_GEMINI_PUBLIC_CLIENT_ID_PROJECT: &str = "681255809395";
const GOOGLE_GEMINI_PUBLIC_CLIENT_ID_HASH: &str = "oo8ft2oprdrnp9e3aqf6av3hmdib135j";
const GOOGLE_GEMINI_PUBLIC_CLIENT_SECRET_SUFFIX: &str = "4uHgMPm-1o7Sk-geV6Cu5clXFsxl";
const NOUS_PORTAL_BASE_URL: &str = "https://portal.nousresearch.com";
const NOUS_INFERENCE_BASE_URL: &str = "https://inference-api.nousresearch.com/v1";
const NOUS_CLIENT_ID: &str = "hermes-cli";
const NOUS_SCOPE: &str = "inference:mint_agent_key";
const NOUS_AGENT_KEY_MIN_TTL_SECONDS: i64 = 30 * 60;
const QWEN_CLIENT_ID: &str = "f0304373b74a44d2b584a3fb70ca9e56";
const QWEN_TOKEN_URL: &str = "https://chat.qwen.ai/api/v1/oauth2/token";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthStore {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderCredentialEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_credentials: BTreeMap<String, ProviderCredentialEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub gateway: BTreeMap<String, GatewayCredentialEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub web: BTreeMap<String, WebCredentialEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderCredentialEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_key_expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portal_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GatewayCredentialEntry {
    pub channel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct WebCredentialEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProviderCredentials {
    pub api_key: Option<String>,
    pub token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    pub agent_key_expires_at: Option<i64>,
    pub project_id: Option<String>,
    pub source: String,
    pub source_path: Option<String>,
    pub ownership: Option<String>,
    pub base_url: Option<String>,
    pub portal_base_url: Option<String>,
    pub client_id: Option<String>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CodexDeviceLogin {
    pub verification_uri: String,
    pub user_code: String,
    device_auth_id: String,
    pub interval_seconds: u64,
}

#[derive(Debug, Clone)]
pub enum CodexDevicePoll {
    Pending,
    SlowDown,
    Complete(ProviderCredentials),
}

#[derive(Debug, Clone)]
pub struct NousDeviceLogin {
    pub verification_uri: String,
    pub user_code: String,
    pub interval_seconds: u64,
    expires_at: i64,
    portal_base_url: String,
    inference_base_url: String,
    client_id: String,
    scope: String,
    device_code: String,
}

#[derive(Debug, Clone)]
pub enum NousDevicePoll {
    Pending,
    SlowDown,
    Complete(ProviderCredentials),
}

impl ProviderCredentials {
    pub fn as_api_key(&self) -> String {
        self.api_key.clone().unwrap_or_else(|| self.token.clone())
    }
}

pub fn load_auth_store() -> Result<AuthStore> {
    let path = auth_store_path()?;
    if !path.exists() {
        return Ok(AuthStore::default());
    }
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read auth store: {}", path.display()))?;
    serde_json::from_str(&text).context("failed to parse active profile auth.json")
}

pub fn save_auth_store(store: &AuthStore) -> Result<()> {
    let path = auth_store_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create auth dir: {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(store).context("failed to serialize auth store")?;
    fs::write(&path, format!("{text}\n"))
        .with_context(|| format!("failed to write auth store: {}", path.display()))
}

pub fn save_provider_credentials(
    provider: ProviderKind,
    credentials: ProviderCredentials,
) -> Result<()> {
    let mut store = load_auth_store().unwrap_or_default();
    store.providers.insert(
        provider.as_str().to_string(),
        ProviderCredentialEntry {
            api_key: credentials
                .api_key
                .clone()
                .or_else(|| Some(credentials.token.clone())),
            token: Some(credentials.token),
            refresh_token: credentials.refresh_token,
            expires_at: credentials.expires_at,
            project_id: credentials.project_id,
            source: Some(credentials.source),
            source_path: credentials.source_path,
            ownership: credentials.ownership,
            base_url: credentials.base_url,
            agent_key_expires_at: credentials.agent_key_expires_at,
            portal_base_url: credentials.portal_base_url,
            client_id: credentials.client_id,
            scope: credentials.scope,
        },
    );
    save_auth_store(&store)
}

pub fn save_model_credentials(model_id: &str, credentials: ProviderCredentials) -> Result<()> {
    let mut store = load_auth_store().unwrap_or_default();
    store.model_credentials.insert(
        model_id.to_string(),
        ProviderCredentialEntry {
            api_key: credentials
                .api_key
                .clone()
                .or_else(|| Some(credentials.token.clone())),
            token: Some(credentials.token),
            refresh_token: credentials.refresh_token,
            expires_at: credentials.expires_at,
            project_id: credentials.project_id,
            source: Some(credentials.source),
            source_path: credentials.source_path,
            ownership: credentials.ownership,
            base_url: credentials.base_url,
            agent_key_expires_at: credentials.agent_key_expires_at,
            portal_base_url: credentials.portal_base_url,
            client_id: credentials.client_id,
            scope: credentials.scope,
        },
    );
    save_auth_store(&store)
}

pub fn save_gateway_credentials(id: &str, credentials: GatewayCredentialEntry) -> Result<()> {
    let mut store = load_auth_store().unwrap_or_default();
    store.gateway.insert(id.to_string(), credentials);
    save_auth_store(&store)
}

pub fn remove_gateway_credentials(id: &str) -> Result<()> {
    let mut store = load_auth_store().unwrap_or_default();
    store.gateway.remove(id);
    save_auth_store(&store)
}

pub fn save_web_credentials(provider: &str, api_key: String) -> Result<()> {
    let provider = provider.trim().to_ascii_lowercase();
    if provider.is_empty() {
        bail!("web provider name must be non-empty");
    }
    let now = Utc::now().to_rfc3339();
    let mut store = load_auth_store().unwrap_or_default();
    let created_at = store
        .web
        .get(&provider)
        .and_then(|entry| entry.created_at.clone())
        .or_else(|| Some(now.clone()));
    store.web.insert(
        provider,
        WebCredentialEntry {
            api_key: Some(api_key),
            token: None,
            source: Some("web_setup".to_string()),
            created_at,
            updated_at: Some(now),
        },
    );
    save_auth_store(&store)
}

pub fn resolve_web_api_key(provider: &str) -> Option<String> {
    let provider = provider.trim().to_ascii_lowercase();
    if provider.is_empty() {
        return None;
    }
    load_auth_store()
        .ok()
        .and_then(|store| {
            store
                .web
                .get(&provider)
                .and_then(|entry| entry.api_key.clone().or_else(|| entry.token.clone()))
        })
        .filter(|value| !value.trim().is_empty())
        .or_else(|| web_api_key_from_env(&provider))
}

fn web_api_key_from_env(provider: &str) -> Option<String> {
    let key = match provider {
        "exa" => "EXA_API_KEY",
        "firecrawl" => "FIRECRAWL_API_KEY",
        "tavily" => "TAVILY_API_KEY",
        "parallel" => "PARALLEL_API_KEY",
        _ => return None,
    };
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn remove_provider_credentials(provider: ProviderKind) -> Result<()> {
    let mut store = load_auth_store().unwrap_or_default();
    store.providers.remove(provider.as_str());
    save_auth_store(&store)
}

pub fn remove_model_credentials(model_id: &str) -> Result<()> {
    let mut store = load_auth_store().unwrap_or_default();
    store.model_credentials.remove(model_id);
    save_auth_store(&store)
}

pub fn resolve_provider_credentials(
    provider: ProviderKind,
    refresh: bool,
) -> Result<Option<ProviderCredentials>> {
    match provider {
        ProviderKind::Nous => resolve_nous_credentials(refresh).map(Some),
        ProviderKind::OpenAiCodex => resolve_codex_credentials(refresh).map(Some),
        ProviderKind::QwenOauth => resolve_qwen_credentials(refresh).map(Some),
        ProviderKind::GoogleGeminiCli => resolve_google_gemini_cli_credentials(refresh).map(Some),
        ProviderKind::CopilotAcp => Ok(Some(resolve_copilot_acp_credentials()?)),
        _ => Ok(None),
    }
}

pub fn codex_saved_credentials() -> Result<Option<ProviderCredentials>> {
    if let Some(saved) = load_saved_credentials(ProviderKind::OpenAiCodex)? {
        return Ok(Some(saved));
    }
    Ok(None)
}

pub fn codex_cli_credentials_if_usable() -> Result<Option<ProviderCredentials>> {
    let Some(credentials) = read_codex_cli_credentials()? else {
        return Ok(None);
    };
    if is_expiring(credentials.expires_at, 0) {
        return Ok(None);
    }
    Ok(Some(credentials))
}

pub fn copilot_acp_available() -> bool {
    let command = env::var("HERMES_COPILOT_ACP_COMMAND")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| env::var("COPILOT_CLI_PATH").ok())
        .unwrap_or_else(|| "copilot".to_string());
    Command::new(command)
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn resolve_codex_credentials(refresh: bool) -> Result<ProviderCredentials> {
    if let Some(credentials) = codex_saved_credentials()? {
        if !refresh || !is_expiring(credentials.expires_at, 120) {
            return Ok(credentials);
        }

        if is_codex_cli_shared(&credentials) {
            if let Some(cli_credentials) = codex_cli_credentials_if_usable()? {
                save_provider_credentials(ProviderKind::OpenAiCodex, cli_credentials.clone())?;
                return Ok(cli_credentials);
            }
        }

        let Some(refresh_token) = credentials.refresh_token.clone() else {
            remove_provider_credentials(ProviderKind::OpenAiCodex)?;
            bail!(
                "OpenAI Codex credentials expired and no refresh token is available. Removed saved Codex credentials from the active profile auth.json; rerun setup and sign in again."
            );
        };

        match refresh_openai_token(&refresh_token) {
            Ok(mut refreshed) => {
                refreshed.ownership = Some(codex_ownership(&credentials).to_string());
                refreshed.source_path = credentials.source_path.clone();
                if is_codex_cli_shared(&credentials) {
                    refreshed.source = "codex_cli_shared_refresh".to_string();
                }
                save_provider_credentials(ProviderKind::OpenAiCodex, refreshed.clone())?;
                return Ok(refreshed);
            }
            Err(error) => {
                remove_provider_credentials(ProviderKind::OpenAiCodex)?;
                bail!(
                    "OpenAI Codex credentials expired and refresh failed. Removed saved Codex credentials from the active profile auth.json; rerun setup and sign in again.\nCause: {error:#}"
                );
            }
        }
    }
    bail!("OpenAI Codex is not logged in. Start setup and complete device-code login.")
}

fn resolve_qwen_credentials(refresh: bool) -> Result<ProviderCredentials> {
    let path = qwen_cli_auth_path()?;
    let mut raw = read_json_file(&path).with_context(|| {
        format!(
            "Qwen CLI credentials not found. Run `qwen auth qwen-oauth` first. Expected: {}",
            path.display()
        )
    })?;
    let mut token = raw
        .get("access_token")
        .or_else(|| raw.get("accessToken"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut refresh_token = raw
        .get("refresh_token")
        .or_else(|| raw.get("refreshToken"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut expires_at = raw
        .get("expiry_date")
        .or_else(|| raw.get("expires_at"))
        .or_else(|| raw.get("expiresAt"))
        .and_then(|value| parse_expiry_value(Some(value)));

    if refresh && is_missing_or_expiring(expires_at, 120) {
        let refresh_value = refresh_token.clone().ok_or_else(|| {
            anyhow!("Qwen OAuth refresh token missing. Run `qwen auth qwen-oauth` first.")
        })?;
        let refreshed = refresh_qwen_token(&refresh_value)?;
        token = refreshed.token.clone();
        raw["access_token"] = Value::String(refreshed.token);
        if let Some(next_refresh_token) = refreshed.refresh_token {
            raw["refresh_token"] = Value::String(next_refresh_token.clone());
            refresh_token = Some(next_refresh_token);
        }
        if let Some(next_expires_at) = refreshed.expires_at {
            raw["expiry_date"] = Value::Number((next_expires_at * 1000).into());
            expires_at = Some(next_expires_at);
        }
        let _ = fs::write(&path, serde_json::to_string_pretty(&raw)?);
    }

    if token.trim().is_empty() {
        bail!("Qwen OAuth access token missing. Run `qwen auth qwen-oauth` first.");
    }

    Ok(ProviderCredentials {
        api_key: None,
        token,
        refresh_token,
        expires_at,
        agent_key_expires_at: None,
        project_id: None,
        source: path.display().to_string(),
        source_path: Some(path.display().to_string()),
        ownership: Some("qwen_cli_external".to_string()),
        base_url: Some(QWEN_BASE_URL.to_string()),
        portal_base_url: None,
        client_id: None,
        scope: None,
    })
}

fn resolve_google_gemini_cli_credentials(refresh: bool) -> Result<ProviderCredentials> {
    let path = google_oauth_path()?;
    let mut raw = read_json_file(&path).with_context(|| {
        format!(
            "Google Gemini CLI OAuth credentials not found. Run Gemini CLI login first. Expected: {}",
            path.display()
        )
    })?;
    let mut token = raw
        .get("access")
        .or_else(|| raw.get("access_token"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let refresh_packed = raw
        .get("refresh")
        .or_else(|| raw.get("refresh_token"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let (mut refresh_token, mut project_id) = parse_google_refresh_pack(&refresh_packed);
    let mut expires_at = raw
        .get("expires")
        .or_else(|| raw.get("expires_at"))
        .and_then(|value| parse_expiry_value(Some(value)));

    if refresh && is_missing_or_expiring(expires_at, 60) {
        if refresh_token.is_empty() {
            bail!("Google Gemini CLI OAuth refresh token missing. Run Gemini CLI login first.");
        }
        let refreshed = refresh_google_token(&refresh_token)?;
        token = refreshed.token.clone();
        raw["access"] = Value::String(refreshed.token);
        if let Some(next_refresh_token) = refreshed.refresh_token {
            raw["refresh"] = Value::String(match &project_id {
                Some(project_id) => format!("{next_refresh_token}|{project_id}"),
                None => next_refresh_token.clone(),
            });
            refresh_token = next_refresh_token;
        }
        if let Some(next_expires_at) = refreshed.expires_at {
            raw["expires"] = Value::Number((next_expires_at * 1000).into());
            expires_at = Some(next_expires_at);
        }
        let _ = fs::write(&path, serde_json::to_string_pretty(&raw)?);
    }

    if project_id.as_deref().is_none_or(str::is_empty) {
        project_id = google_project_id_from_env();
    }

    if token.trim().is_empty() {
        bail!("Google Gemini CLI OAuth access token missing. Run Gemini CLI login first.");
    }

    Ok(ProviderCredentials {
        api_key: None,
        token,
        refresh_token: Some(refresh_token).filter(|value| !value.is_empty()),
        expires_at,
        agent_key_expires_at: None,
        project_id,
        source: path.display().to_string(),
        source_path: Some(path.display().to_string()),
        ownership: Some("google_gemini_cli_external".to_string()),
        base_url: Some(GEMINI_CLOUDCODE_BASE_URL.to_string()),
        portal_base_url: None,
        client_id: None,
        scope: None,
    })
}

fn resolve_copilot_acp_credentials() -> Result<ProviderCredentials> {
    if !copilot_acp_available() {
        bail!(
            "Copilot ACP is unavailable. Install/login GitHub Copilot CLI and ensure `copilot --acp --stdio` works."
        );
    }
    Ok(ProviderCredentials {
        api_key: None,
        token: "copilot-acp".to_string(),
        refresh_token: None,
        expires_at: None,
        agent_key_expires_at: None,
        project_id: None,
        source: "copilot --acp --stdio".to_string(),
        source_path: None,
        ownership: Some("external_process".to_string()),
        base_url: Some("acp://copilot".to_string()),
        portal_base_url: None,
        client_id: None,
        scope: None,
    })
}

fn resolve_nous_credentials(refresh: bool) -> Result<ProviderCredentials> {
    let mut credentials = load_saved_credentials(ProviderKind::Nous)?
        .ok_or_else(|| anyhow!("Nous is not logged in. Start setup and complete Nous login."))?;
    let portal_base_url = credentials
        .portal_base_url
        .clone()
        .or_else(|| env::var("NOUS_PORTAL_BASE_URL").ok())
        .unwrap_or_else(|| NOUS_PORTAL_BASE_URL.to_string());
    let inference_base_url = credentials
        .base_url
        .clone()
        .or_else(|| env::var("NOUS_INFERENCE_BASE_URL").ok())
        .unwrap_or_else(|| NOUS_INFERENCE_BASE_URL.to_string());
    let client_id = credentials
        .client_id
        .clone()
        .unwrap_or_else(|| NOUS_CLIENT_ID.to_string());
    let scope = credentials
        .scope
        .clone()
        .unwrap_or_else(|| NOUS_SCOPE.to_string());

    if refresh && is_expiring(credentials.expires_at, 120) {
        let refresh_token = credentials.refresh_token.clone().ok_or_else(|| {
            anyhow!("Nous OAuth session expired and no refresh token is available. Rerun setup and sign in again.")
        })?;
        let refreshed = refresh_nous_access_token(&portal_base_url, &client_id, &refresh_token)?;
        credentials.token = refreshed.token;
        credentials.refresh_token = refreshed.refresh_token.or(Some(refresh_token));
        credentials.expires_at = refreshed.expires_at;
        credentials.source = "refresh".to_string();
        credentials.base_url = refreshed.base_url.or(Some(inference_base_url.clone()));
        credentials.portal_base_url = Some(portal_base_url.clone());
        credentials.client_id = Some(client_id.clone());
        credentials.scope = Some(scope.clone());
        // Nous refresh tokens can rotate. Persist immediately so a later mint
        // failure cannot strand us with a retired refresh token.
        save_provider_credentials(ProviderKind::Nous, credentials.clone())?;
    }

    if refresh && !nous_agent_key_is_usable(&credentials) {
        match mint_nous_agent_key(&portal_base_url, &credentials.token) {
            Ok(agent) => {
                credentials.api_key = Some(agent.api_key);
                credentials.agent_key_expires_at = agent.expires_at;
                credentials.base_url = agent.base_url.or(Some(inference_base_url));
                credentials.portal_base_url = Some(portal_base_url);
                credentials.client_id = Some(client_id);
                credentials.scope = Some(scope);
                credentials.source = "agent_key".to_string();
                save_provider_credentials(ProviderKind::Nous, credentials.clone())?;
            }
            Err(error) => {
                let message = error.to_string();
                if message.contains("invalid_token") || message.contains("invalid_grant") {
                    if let Some(refresh_token) = credentials.refresh_token.clone() {
                        let refreshed = refresh_nous_access_token(
                            &portal_base_url,
                            &client_id,
                            &refresh_token,
                        )?;
                        credentials.token = refreshed.token;
                        credentials.refresh_token = refreshed.refresh_token.or(Some(refresh_token));
                        credentials.expires_at = refreshed.expires_at;
                        credentials.base_url = refreshed.base_url.or(credentials.base_url.clone());
                        credentials.portal_base_url = Some(portal_base_url.clone());
                        credentials.client_id = Some(client_id.clone());
                        credentials.scope = Some(scope.clone());
                        save_provider_credentials(ProviderKind::Nous, credentials.clone())?;

                        let agent = mint_nous_agent_key(&portal_base_url, &credentials.token)?;
                        credentials.api_key = Some(agent.api_key);
                        credentials.agent_key_expires_at = agent.expires_at;
                        credentials.base_url = agent.base_url.or(credentials.base_url.clone());
                        credentials.source = "agent_key".to_string();
                        save_provider_credentials(ProviderKind::Nous, credentials.clone())?;
                        return Ok(credentials);
                    }
                }
                bail!(
                    "Failed to mint a Nous inference agent key. If this is a subscription issue, check {}/billing.\nCause: {error:#}",
                    portal_base_url.trim_end_matches('/')
                );
            }
        }
    }

    if credentials.as_api_key().trim().is_empty() {
        bail!("Nous login did not resolve an inference API key. Rerun setup and sign in again.");
    }
    Ok(credentials)
}

pub fn save_codex_cli_shared_credentials(credentials: ProviderCredentials) -> Result<()> {
    save_provider_credentials(ProviderKind::OpenAiCodex, credentials)
}

fn read_codex_cli_credentials() -> Result<Option<ProviderCredentials>> {
    let path = dirs::home_dir()
        .ok_or_else(|| anyhow!("failed to resolve home directory"))?
        .join(".codex/auth.json");
    if !path.exists() {
        return Ok(None);
    }
    let raw = read_json_file(&path)?;
    let token = raw
        .pointer("/tokens/access_token")
        .or_else(|| raw.get("access_token"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if token.trim().is_empty() {
        return Ok(None);
    }
    let refresh_token = raw
        .pointer("/tokens/refresh_token")
        .or_else(|| raw.get("refresh_token"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let credentials = ProviderCredentials {
        api_key: None,
        expires_at: jwt_expiry(&token),
        token,
        refresh_token,
        agent_key_expires_at: None,
        project_id: None,
        source: "codex_cli".to_string(),
        source_path: Some(path.display().to_string()),
        ownership: Some("codex_cli_shared".to_string()),
        base_url: Some(CODEX_BASE_URL.to_string()),
        portal_base_url: None,
        client_id: None,
        scope: None,
    };
    Ok(Some(credentials))
}

pub fn start_nous_device_code_login() -> Result<NousDeviceLogin> {
    let portal_base_url = env::var("NOUS_PORTAL_BASE_URL")
        .unwrap_or_else(|_| NOUS_PORTAL_BASE_URL.to_string())
        .trim_end_matches('/')
        .to_string();
    let inference_base_url = env::var("NOUS_INFERENCE_BASE_URL")
        .unwrap_or_else(|_| NOUS_INFERENCE_BASE_URL.to_string())
        .trim_end_matches('/')
        .to_string();
    let client_id = env::var("NOUS_CLIENT_ID").unwrap_or_else(|_| NOUS_CLIENT_ID.to_string());
    let scope = env::var("NOUS_SCOPE").unwrap_or_else(|_| NOUS_SCOPE.to_string());
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build Nous OAuth client")?;

    let mut device_form = vec![("client_id", client_id.as_str())];
    if !scope.trim().is_empty() {
        device_form.push(("scope", scope.as_str()));
    }
    let begin = post_form_json(
        &client,
        &format!("{portal_base_url}/api/oauth/device/code"),
        &device_form,
        "Nous device login start failed",
    )?;
    let verification_uri = begin
        .get("verification_uri_complete")
        .or_else(|| begin.get("verification_uri"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Nous device login response missing verification_uri"))?
        .to_string();
    let user_code = begin
        .get("user_code")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let device_code = begin
        .get("device_code")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Nous device login response missing device_code"))?
        .to_string();
    let expires_in = value_as_u64(begin.get("expires_in")).unwrap_or(600);
    let interval_seconds = value_as_u64(begin.get("interval"))
        .unwrap_or(1)
        .clamp(1, 30);

    Ok(NousDeviceLogin {
        verification_uri,
        user_code,
        interval_seconds,
        expires_at: now_seconds() + expires_in as i64,
        portal_base_url,
        inference_base_url,
        client_id,
        scope,
        device_code,
    })
}

pub fn poll_nous_device_code_login(login: &NousDeviceLogin) -> Result<NousDevicePoll> {
    if now_seconds() >= login.expires_at {
        bail!("Nous device login timed out");
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build Nous OAuth poll client")?;
    let response = client
        .post(format!("{}/api/oauth/token", login.portal_base_url))
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", login.client_id.as_str()),
            ("device_code", login.device_code.as_str()),
        ])
        .send()
        .context("failed to poll Nous device login")?;
    if response.status().is_success() {
        let token_data = response
            .json::<Value>()
            .context("failed to parse Nous token response")?;
        return Ok(NousDevicePoll::Complete(
            nous_credentials_from_token_response(login, &token_data)?,
        ));
    }

    let status = response.status();
    let body = response.text().unwrap_or_default();
    let error = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
    match error
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "authorization_pending" => Ok(NousDevicePoll::Pending),
        "slow_down" => Ok(NousDevicePoll::SlowDown),
        _ => bail!("Nous device login failed: HTTP {status}; body: {body}"),
    }
}

fn nous_credentials_from_token_response(
    login: &NousDeviceLogin,
    token_data: &Value,
) -> Result<ProviderCredentials> {
    let access_token = token_data
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Nous token response missing access_token"))?
        .to_string();
    let expires_in = token_data
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    let resolved_inference_base_url = token_data
        .get("inference_base_url")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&login.inference_base_url)
        .trim_end_matches('/')
        .to_string();
    let mut credentials = ProviderCredentials {
        api_key: None,
        token: access_token,
        refresh_token: token_data
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string),
        expires_at: Some(now_seconds() + expires_in),
        agent_key_expires_at: None,
        project_id: None,
        source: "device_code".to_string(),
        source_path: None,
        ownership: Some("duckagent_owned".to_string()),
        base_url: Some(resolved_inference_base_url),
        portal_base_url: Some(login.portal_base_url.clone()),
        client_id: Some(login.client_id.clone()),
        scope: Some(login.scope.clone()),
    };
    let agent = mint_nous_agent_key(&login.portal_base_url, &credentials.token)?;
    credentials.api_key = Some(agent.api_key);
    credentials.agent_key_expires_at = agent.expires_at;
    if let Some(base_url) = agent.base_url {
        credentials.base_url = Some(base_url);
    }
    Ok(credentials)
}

pub fn start_openai_codex_device_code_login() -> Result<CodexDeviceLogin> {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build OAuth client")?;
    // OpenCode/Codex headless auth is not the generic OAuth device-code flow.
    // The browser page is /codex/device, and the poll endpoint returns an
    // authorization_code + code_verifier that must be exchanged at /oauth/token.
    // Do not send users to /activate or expect access_token directly here.
    let begin: Value = client
        .post(format!(
            "{CODEX_AUTH_ISSUER}/api/accounts/deviceauth/usercode"
        ))
        .header("User-Agent", codex_user_agent())
        .json(&codex_usercode_request_body())
        .send()
        .context("failed to start OpenAI Codex device login")?
        .error_for_status()
        .context("OpenAI Codex device login start failed")?
        .json()
        .context("failed to parse OpenAI Codex device login response")?;
    let user_code = begin
        .get("user_code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let device_auth_id = begin
        .get("device_auth_id")
        .or_else(|| begin.get("device_code"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("OpenAI Codex device login response missing device_auth_id"))?;
    let interval_seconds = value_as_u64(begin.get("interval")).unwrap_or(5).max(1);
    Ok(CodexDeviceLogin {
        verification_uri: CODEX_DEVICE_URL.to_string(),
        user_code: user_code.to_string(),
        device_auth_id: device_auth_id.to_string(),
        interval_seconds,
    })
}

pub fn poll_openai_codex_device_code_login(login: &CodexDeviceLogin) -> Result<CodexDevicePoll> {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build OAuth client")?;
    let response = client
        .post(format!("{CODEX_AUTH_ISSUER}/api/accounts/deviceauth/token"))
        .header("User-Agent", codex_user_agent())
        .json(&codex_device_poll_request_body(login))
        .send()
        .context("failed to poll OpenAI Codex device login")?;
    if response.status().is_success() {
        let code_payload: Value = response
            .json()
            .context("failed to parse Codex authorization code response")?;
        let authorization_code = code_payload
            .get("authorization_code")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Codex device login did not return authorization_code"))?;
        let code_verifier = code_payload
            .get("code_verifier")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Codex device login did not return code_verifier"))?;
        let credentials = exchange_codex_authorization_code(authorization_code, code_verifier)?;
        save_provider_credentials(ProviderKind::OpenAiCodex, credentials.clone())?;
        return Ok(CodexDevicePoll::Complete(credentials));
    }

    let status = response.status();
    let body = response.text().unwrap_or_default();
    if status.as_u16() == 403 || status.as_u16() == 404 {
        return Ok(CodexDevicePoll::Pending);
    }
    let error = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
    match error
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "authorization_pending" => Ok(CodexDevicePoll::Pending),
        "slow_down" => Ok(CodexDevicePoll::SlowDown),
        _ => bail!("OpenAI Codex device login failed: HTTP {status}; body: {body}"),
    }
}

fn exchange_codex_authorization_code(
    authorization_code: &str,
    code_verifier: &str,
) -> Result<ProviderCredentials> {
    let payload: Value = Client::new()
        .post(format!("{CODEX_AUTH_ISSUER}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", authorization_code),
            ("redirect_uri", CODEX_DEVICE_CALLBACK_URL),
            ("client_id", CODEX_CLIENT_ID),
            ("code_verifier", code_verifier),
        ])
        .send()
        .context("failed to exchange Codex authorization code")?
        .error_for_status()
        .context("Codex authorization code exchange failed")?
        .json()
        .context("failed to parse Codex token exchange response")?;
    let token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Codex token exchange missing access_token"))?
        .to_string();
    Ok(ProviderCredentials {
        api_key: None,
        expires_at: token_expiry_from_payload(&payload, &token),
        token,
        refresh_token: payload
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string),
        agent_key_expires_at: None,
        project_id: codex_account_id_from_token_payload(&payload),
        source: "device_code".to_string(),
        source_path: None,
        ownership: Some("duckagent_owned".to_string()),
        base_url: Some(CODEX_BASE_URL.to_string()),
        portal_base_url: None,
        client_id: Some(CODEX_CLIENT_ID.to_string()),
        scope: None,
    })
}

fn refresh_openai_token(refresh_token: &str) -> Result<ProviderCredentials> {
    let payload: Value = Client::new()
        .post(format!("{CODEX_AUTH_ISSUER}/oauth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CODEX_CLIENT_ID),
        ])
        .send()
        .context("failed to refresh OpenAI token")?
        .error_for_status()
        .context("OpenAI token refresh failed")?
        .json()
        .context("failed to parse OpenAI token refresh")?;
    let token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("OpenAI token refresh missing access_token"))?
        .to_string();
    Ok(ProviderCredentials {
        api_key: None,
        expires_at: token_expiry_from_payload(&payload, &token),
        token,
        refresh_token: payload
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| Some(refresh_token.to_string())),
        agent_key_expires_at: None,
        project_id: codex_account_id_from_token_payload(&payload),
        source: "refresh".to_string(),
        source_path: None,
        ownership: Some("duckagent_owned".to_string()),
        base_url: Some(CODEX_BASE_URL.to_string()),
        portal_base_url: None,
        client_id: Some(CODEX_CLIENT_ID.to_string()),
        scope: None,
    })
}

fn refresh_qwen_token(refresh_token: &str) -> Result<ProviderCredentials> {
    let payload: Value = Client::new()
        .post(QWEN_TOKEN_URL)
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", QWEN_CLIENT_ID),
        ])
        .send()
        .context("failed to refresh Qwen token")?
        .error_for_status()
        .context("Qwen token refresh failed")?
        .json()
        .context("failed to parse Qwen token refresh")?;
    let token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Qwen token refresh missing access_token"))?
        .to_string();
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    Ok(ProviderCredentials {
        api_key: None,
        token,
        refresh_token: payload
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| Some(refresh_token.to_string())),
        expires_at: Some(now_seconds() + expires_in),
        agent_key_expires_at: None,
        project_id: None,
        source: "refresh".to_string(),
        source_path: None,
        ownership: None,
        base_url: Some(QWEN_BASE_URL.to_string()),
        portal_base_url: None,
        client_id: None,
        scope: None,
    })
}

fn refresh_google_token(refresh_token: &str) -> Result<ProviderCredentials> {
    let client_id = google_gemini_oauth_client_id();
    let client_secret = google_gemini_oauth_client_secret();
    let payload: Value = Client::new()
        .post(GOOGLE_TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
        ])
        .send()
        .context("failed to refresh Google token")?
        .error_for_status()
        .context("Google token refresh failed")?
        .json()
        .context("failed to parse Google token refresh")?;
    let token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Google token refresh missing access_token"))?
        .to_string();
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    Ok(ProviderCredentials {
        api_key: None,
        token,
        refresh_token: Some(refresh_token.to_string()),
        expires_at: Some(now_seconds() + expires_in),
        agent_key_expires_at: None,
        project_id: None,
        source: "refresh".to_string(),
        source_path: None,
        ownership: None,
        base_url: Some(GEMINI_CLOUDCODE_BASE_URL.to_string()),
        portal_base_url: None,
        client_id: Some(client_id),
        scope: None,
    })
}

fn google_gemini_oauth_client_id() -> String {
    env::var(GOOGLE_GEMINI_CLIENT_ID_ENV)
        .ok()
        .and_then(non_empty_string)
        .unwrap_or_else(google_gemini_public_client_id)
}

fn google_gemini_oauth_client_secret() -> String {
    env::var(GOOGLE_GEMINI_CLIENT_SECRET_ENV)
        .ok()
        .and_then(non_empty_string)
        .unwrap_or_else(google_gemini_public_client_secret)
}

fn google_gemini_public_client_id() -> String {
    format!(
        "{GOOGLE_GEMINI_PUBLIC_CLIENT_ID_PROJECT}-{GOOGLE_GEMINI_PUBLIC_CLIENT_ID_HASH}.apps.googleusercontent.com"
    )
}

fn google_gemini_public_client_secret() -> String {
    format!("GOCSPX-{GOOGLE_GEMINI_PUBLIC_CLIENT_SECRET_SUFFIX}")
}

#[derive(Debug, Clone)]
struct NousAgentKey {
    api_key: String,
    expires_at: Option<i64>,
    base_url: Option<String>,
}

fn refresh_nous_access_token(
    portal_base_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<ProviderCredentials> {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build Nous refresh client")?;
    let payload = post_form_json(
        &client,
        &format!("{}/api/oauth/token", portal_base_url.trim_end_matches('/')),
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
        ],
        "Nous token refresh failed",
    )?;
    let token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Nous refresh response missing access_token"))?
        .to_string();
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    Ok(ProviderCredentials {
        api_key: None,
        token,
        refresh_token: payload
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| Some(refresh_token.to_string())),
        expires_at: Some(now_seconds() + expires_in),
        agent_key_expires_at: None,
        project_id: None,
        source: "refresh".to_string(),
        source_path: None,
        ownership: Some("duckagent_owned".to_string()),
        base_url: payload
            .get("inference_base_url")
            .and_then(Value::as_str)
            .map(|value| value.trim_end_matches('/').to_string()),
        portal_base_url: Some(portal_base_url.trim_end_matches('/').to_string()),
        client_id: Some(client_id.to_string()),
        scope: Some(NOUS_SCOPE.to_string()),
    })
}

fn mint_nous_agent_key(portal_base_url: &str, access_token: &str) -> Result<NousAgentKey> {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build Nous agent-key client")?;
    let response = client
        .post(format!(
            "{}/api/oauth/agent-key",
            portal_base_url.trim_end_matches('/')
        ))
        .bearer_auth(access_token)
        .json(&json!({
            "min_ttl_seconds": NOUS_AGENT_KEY_MIN_TTL_SECONDS.max(60)
        }))
        .send()
        .context("failed to mint Nous inference agent key")?;
    let status = response.status();
    let body = response.text().unwrap_or_default();
    if !status.is_success() {
        if let Ok(error) = serde_json::from_str::<Value>(&body) {
            let code = error
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("server_error");
            let description = error
                .get("error_description")
                .or_else(|| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Agent key mint request failed");
            if code == "subscription_required" {
                bail!("Nous subscription required: {description}");
            }
            bail!("Nous agent key mint failed ({code}): {description}");
        }
        bail!("Nous agent key mint failed: HTTP {status}; body: {body}");
    }
    let payload: Value = serde_json::from_str(&body).context("failed to parse Nous agent key")?;
    let api_key = payload
        .get("api_key")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Nous agent-key response missing api_key"))?
        .to_string();
    Ok(NousAgentKey {
        api_key,
        expires_at: parse_expiry_value(payload.get("expires_at")).or_else(|| {
            payload
                .get("expires_in")
                .and_then(Value::as_i64)
                .map(|ttl| now_seconds() + ttl)
        }),
        base_url: payload
            .get("inference_base_url")
            .and_then(Value::as_str)
            .map(|value| value.trim_end_matches('/').to_string()),
    })
}

fn post_form_json(
    client: &Client,
    url: &str,
    form: &[(&str, &str)],
    context: &str,
) -> Result<Value> {
    let response = client
        .post(url)
        .form(form)
        .send()
        .with_context(|| format!("{context}: request failed"))?;
    let status = response.status();
    let body = response.text().unwrap_or_default();
    if !status.is_success() {
        if let Ok(error) = serde_json::from_str::<Value>(&body) {
            let code = error
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("server_error");
            let mut description = error
                .get("error_description")
                .or_else(|| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or(context)
                .to_string();
            if code == "invalid_grant" && description.to_ascii_lowercase().contains("reuse") {
                description = "Nous Portal detected refresh-token reuse and revoked this session. Rerun setup and sign in again.".to_string();
            }
            bail!("{context}: {code}: {description}");
        }
        bail!("{context}: HTTP {status}; body: {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("{context}: failed to parse JSON"))
}

fn nous_agent_key_is_usable(credentials: &ProviderCredentials) -> bool {
    credentials
        .api_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty())
        && !is_expiring(
            credentials.agent_key_expires_at,
            NOUS_AGENT_KEY_MIN_TTL_SECONDS,
        )
}

fn load_saved_credentials(provider: ProviderKind) -> Result<Option<ProviderCredentials>> {
    let store = load_auth_store().unwrap_or_default();
    let Some(entry) = store.providers.get(provider.as_str()) else {
        return Ok(None);
    };
    let token = entry
        .token
        .clone()
        .or_else(|| entry.api_key.clone())
        .unwrap_or_default();
    if token.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(ProviderCredentials {
        api_key: entry.api_key.clone(),
        token,
        refresh_token: entry.refresh_token.clone(),
        expires_at: entry.expires_at,
        agent_key_expires_at: entry.agent_key_expires_at,
        project_id: entry.project_id.clone(),
        source: entry
            .source
            .clone()
            .unwrap_or_else(|| "auth_store".to_string()),
        source_path: entry.source_path.clone(),
        ownership: entry.ownership.clone(),
        base_url: entry.base_url.clone(),
        portal_base_url: entry.portal_base_url.clone(),
        client_id: entry.client_id.clone(),
        scope: entry.scope.clone(),
    }))
}

fn is_codex_cli_shared(credentials: &ProviderCredentials) -> bool {
    codex_ownership(credentials) == "codex_cli_shared"
}

fn codex_ownership(credentials: &ProviderCredentials) -> &str {
    if let Some(ownership) = credentials.ownership.as_deref() {
        return ownership;
    }
    if credentials
        .source_path
        .as_deref()
        .or(Some(credentials.source.as_str()))
        .is_some_and(|source| source.contains(".codex/auth.json"))
    {
        "codex_cli_shared"
    } else {
        "duckagent_owned"
    }
}

fn qwen_cli_auth_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow!("failed to resolve home directory"))?
        .join(".qwen/oauth_creds.json"))
}

fn google_oauth_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("failed to resolve home directory"))?;
    let candidates = [
        profiles::active_profile_path("auth/google_oauth.json")?,
        home.join(".gemini/oauth_creds.json"),
    ];
    Ok(candidates
        .into_iter()
        .find(|path| path.exists())
        .unwrap_or_else(|| {
            profiles::active_profile_path("auth/google_oauth.json")
                .unwrap_or_else(|_| home.join(".duckagent/profiles/default/auth/google_oauth.json"))
        }))
}

fn google_project_id_from_env() -> Option<String> {
    [
        "HERMES_GEMINI_PROJECT_ID",
        "GOOGLE_CLOUD_PROJECT",
        "GOOGLE_CLOUD_PROJECT_ID",
    ]
    .into_iter()
    .find_map(|key| env::var(key).ok().filter(|value| !value.trim().is_empty()))
}

fn auth_store_path() -> Result<PathBuf> {
    profiles::active_profile_path(AUTH_FILE_NAME)
}

fn read_json_file(path: &PathBuf) -> Result<Value> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

fn codex_user_agent() -> String {
    format!("duckagent/{}", env!("CARGO_PKG_VERSION"))
}

fn codex_usercode_request_body() -> Value {
    json!({ "client_id": CODEX_CLIENT_ID })
}

fn codex_device_poll_request_body(login: &CodexDeviceLogin) -> Value {
    json!({
        "device_auth_id": login.device_auth_id,
        "user_code": login.user_code,
    })
}

fn parse_google_refresh_pack(value: &str) -> (String, Option<String>) {
    let mut parts = value.split('|');
    let refresh = parts.next().unwrap_or_default().to_string();
    let project = parts
        .next()
        .map(str::to_string)
        .filter(|value| !value.is_empty());
    (refresh, project)
}

fn jwt_expiry(token: &str) -> Option<i64> {
    jwt_payload_value(token)?.get("exp").and_then(Value::as_i64)
}

fn jwt_payload_value(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn token_expiry_from_payload(payload: &Value, token: &str) -> Option<i64> {
    parse_expiry_value(payload.get("expires_at"))
        .or_else(|| {
            payload
                .get("expires_in")
                .and_then(Value::as_i64)
                .map(|expires_in| now_seconds() + expires_in)
        })
        .or_else(|| jwt_expiry(token))
}

fn codex_account_id_from_token_payload(payload: &Value) -> Option<String> {
    ["id_token", "access_token"].into_iter().find_map(|field| {
        let token = payload.get(field).and_then(Value::as_str)?;
        let claims = jwt_payload_value(token)?;
        codex_account_id_from_claims(&claims)
    })
}

fn codex_account_id_from_claims(claims: &Value) -> Option<String> {
    claims
        .get("chatgpt_account_id")
        .and_then(Value::as_str)
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|auth| auth.get("chatgpt_account_id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            claims
                .get("organizations")
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(|item| item.get("id"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
}

fn normalize_expiry_seconds(value: i64) -> i64 {
    if value > 10_000_000_000 {
        value / 1000
    } else {
        value
    }
}

fn parse_expiry_value(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(number) = value.as_i64() {
        return Some(normalize_expiry_seconds(number));
    }
    let text = value.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(number) = text.parse::<i64>() {
        return Some(normalize_expiry_seconds(number));
    }
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|dt| dt.with_timezone(&Utc).timestamp())
}

fn value_as_u64(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
}

fn non_empty_string(value: String) -> Option<String> {
    let value = value.trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

fn is_expiring(expires_at: Option<i64>, skew_seconds: i64) -> bool {
    expires_at.is_some_and(|expires_at| expires_at <= now_seconds() + skew_seconds)
}

fn is_missing_or_expiring(expires_at: Option<i64>, skew_seconds: i64) -> bool {
    expires_at
        .map(|value| is_expiring(Some(value), skew_seconds))
        .unwrap_or(true)
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    static ENV_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct EnvRestore {
        key: &'static str,
        old_value: Option<String>,
    }

    impl EnvRestore {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                old_value: std::env::var(key).ok(),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            unsafe {
                match &self.old_value {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn google_refresh_pack_extracts_project() {
        let (refresh, project) = parse_google_refresh_pack("refresh-token|project-a|managed");
        assert_eq!(refresh, "refresh-token");
        assert_eq!(project.as_deref(), Some("project-a"));
    }

    #[test]
    fn google_gemini_oauth_client_uses_public_gemini_cli_default() {
        let _guard = ENV_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env test lock poisoned");
        let _client_id_env = EnvRestore::capture(GOOGLE_GEMINI_CLIENT_ID_ENV);
        let _client_secret_env = EnvRestore::capture(GOOGLE_GEMINI_CLIENT_SECRET_ENV);
        unsafe {
            std::env::remove_var(GOOGLE_GEMINI_CLIENT_ID_ENV);
            std::env::remove_var(GOOGLE_GEMINI_CLIENT_SECRET_ENV);
        }

        let client_id = google_gemini_oauth_client_id();
        assert!(client_id.starts_with("681255809395-"));
        assert!(client_id.ends_with(".apps.googleusercontent.com"));
        assert!(google_gemini_oauth_client_secret().starts_with("GOCSPX-"));
    }

    #[test]
    fn google_gemini_oauth_client_allows_env_override() {
        let _guard = ENV_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env test lock poisoned");
        let _client_id_env = EnvRestore::capture(GOOGLE_GEMINI_CLIENT_ID_ENV);
        let _client_secret_env = EnvRestore::capture(GOOGLE_GEMINI_CLIENT_SECRET_ENV);
        unsafe {
            std::env::set_var(
                GOOGLE_GEMINI_CLIENT_ID_ENV,
                "custom-client.apps.googleusercontent.com",
            );
            std::env::set_var(GOOGLE_GEMINI_CLIENT_SECRET_ENV, "custom-secret");
        }

        assert_eq!(
            google_gemini_oauth_client_id(),
            "custom-client.apps.googleusercontent.com"
        );
        assert_eq!(google_gemini_oauth_client_secret(), "custom-secret");
    }

    #[test]
    fn millisecond_expiry_is_normalized() {
        assert_eq!(normalize_expiry_seconds(1_700_000_000_000), 1_700_000_000);
        assert_eq!(normalize_expiry_seconds(1_700_000_000), 1_700_000_000);
    }

    #[test]
    fn codex_headless_flow_matches_opencode_device_page() {
        assert_eq!(CODEX_DEVICE_URL, "https://auth.openai.com/codex/device");
        assert_eq!(
            codex_usercode_request_body(),
            json!({ "client_id": CODEX_CLIENT_ID })
        );

        let login = CodexDeviceLogin {
            verification_uri: CODEX_DEVICE_URL.to_string(),
            user_code: "ABCD-EFGH".to_string(),
            device_auth_id: "device-auth-id".to_string(),
            interval_seconds: 5,
        };
        assert_eq!(
            codex_device_poll_request_body(&login),
            json!({
                "device_auth_id": "device-auth-id",
                "user_code": "ABCD-EFGH"
            })
        );
    }

    #[test]
    fn numeric_string_values_parse_as_u64() {
        assert_eq!(value_as_u64(Some(&Value::String("7".to_string()))), Some(7));
        assert_eq!(value_as_u64(Some(&json!(9))), Some(9));
    }

    #[test]
    fn codex_ownership_detects_legacy_codex_cli_imports() {
        let credentials = ProviderCredentials {
            api_key: None,
            token: "token".to_string(),
            refresh_token: None,
            expires_at: None,
            agent_key_expires_at: None,
            project_id: None,
            source: "/Users/test/.codex/auth.json".to_string(),
            source_path: None,
            ownership: None,
            base_url: None,
            portal_base_url: None,
            client_id: None,
            scope: None,
        };

        assert!(is_codex_cli_shared(&credentials));
    }

    #[test]
    fn explicit_codex_ownership_wins_over_source_path() {
        let credentials = ProviderCredentials {
            api_key: None,
            token: "token".to_string(),
            refresh_token: None,
            expires_at: None,
            agent_key_expires_at: None,
            project_id: None,
            source: "device_code".to_string(),
            source_path: Some("/Users/test/.codex/auth.json".to_string()),
            ownership: Some("duckagent_owned".to_string()),
            base_url: None,
            portal_base_url: None,
            client_id: None,
            scope: None,
        };

        assert!(!is_codex_cli_shared(&credentials));
    }

    #[test]
    fn provider_credentials_prefers_runtime_api_key() {
        let credentials = ProviderCredentials {
            api_key: Some("agent-key".to_string()),
            token: "oauth-access-token".to_string(),
            refresh_token: None,
            expires_at: None,
            agent_key_expires_at: None,
            project_id: None,
            source: "test".to_string(),
            source_path: None,
            ownership: None,
            base_url: None,
            portal_base_url: None,
            client_id: None,
            scope: None,
        };

        assert_eq!(credentials.as_api_key(), "agent-key");
    }

    #[test]
    fn rfc3339_expiry_is_parsed() {
        assert_eq!(
            parse_expiry_value(Some(&Value::String("2026-04-27T00:00:00Z".to_string()))),
            Some(1_777_248_000)
        );
    }
}
