use crate::auth::{
    ProviderCredentialEntry, ProviderCredentials, load_auth_store, remove_model_credentials,
    save_model_credentials,
};
use crate::mcp::config::DuckAgentConfig;
use crate::provider::{ApiMode, ProviderKind, RuntimeProvider, resolve_provider_api_mode};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashSet;
use url::Url;
use uuid::Uuid;

const ACTIVE_MODEL_ID_FIELD: &str = "active_model_id";
const SAVED_MODELS_FIELD: &str = "models";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedModel {
    pub model_id: String,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_mode: Option<ApiMode>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SavedModelInput {
    pub provider: ProviderKind,
    pub model: String,
    pub base_url: Option<String>,
    pub api_mode: Option<ApiMode>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SavedModelStore {
    pub active_model_id: Option<String>,
    pub models: Vec<SavedModel>,
}

#[derive(Debug, Clone)]
pub struct SavedModelListItem {
    pub saved_model: SavedModel,
    pub active: bool,
    pub endpoint: String,
    pub key_fingerprint: String,
}

pub fn load_saved_model_store() -> Result<SavedModelStore> {
    let config = DuckAgentConfig::load_active_profile()?;
    let active_model_id = config
        .raw()
        .get(ACTIVE_MODEL_ID_FIELD)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty());
    let models = match config.raw().get(SAVED_MODELS_FIELD) {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => serde_json::from_value(value.clone())
            .context("failed to parse active profile saved models")?,
    };
    Ok(SavedModelStore {
        active_model_id,
        models,
    })
}

pub fn list_saved_models() -> Result<Vec<SavedModelListItem>> {
    let store = load_saved_model_store()?;
    let auth = load_auth_store().unwrap_or_default();
    let active = store.active_model_id.as_deref();
    let mut items = store
        .models
        .into_iter()
        .map(|saved_model| {
            let key_fingerprint = auth
                .model_credentials
                .get(&saved_model.model_id)
                .and_then(entry_secret)
                .map(|secret| secret_fingerprint(&secret))
                .or_else(|| oauth_or_inherited_credential_label(&saved_model))
                .unwrap_or_else(|| "-".to_string());
            let endpoint = saved_model_endpoint_label(&saved_model);
            let active = active.is_some_and(|id| id == saved_model.model_id);
            SavedModelListItem {
                saved_model,
                active,
                endpoint,
                key_fingerprint,
            }
        })
        .collect::<Vec<_>>();
    items.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then_with(|| b.saved_model.last_used_at.cmp(&a.saved_model.last_used_at))
            .then_with(|| b.saved_model.created_at.cmp(&a.saved_model.created_at))
            .then_with(|| b.saved_model.model_id.cmp(&a.saved_model.model_id))
    });
    Ok(items)
}

pub fn add_saved_model(input: SavedModelInput) -> Result<SavedModel> {
    let mut store = load_saved_model_store()?;
    let now = now_rfc3339();
    let saved_model = SavedModel {
        model_id: Uuid::now_v7().to_string(),
        provider: input.provider.as_str().to_string(),
        model: input.model,
        base_url: input
            .base_url
            .or_else(|| input.provider.default_base_url().map(str::to_string)),
        api_mode: input.api_mode,
        created_at: now,
        last_used_at: None,
    };
    store.models.push(saved_model.clone());
    save_saved_model_store(&store)?;

    if let Some(api_key) = input.api_key.filter(|value| !value.trim().is_empty()) {
        save_model_credentials(
            &saved_model.model_id,
            ProviderCredentials {
                api_key: Some(api_key.clone()),
                token: api_key,
                refresh_token: None,
                expires_at: None,
                agent_key_expires_at: None,
                project_id: None,
                source: "model".to_string(),
                source_path: None,
                ownership: None,
                base_url: saved_model.base_url.clone(),
                portal_base_url: None,
                client_id: None,
                scope: None,
            },
        )?;
    }

    Ok(saved_model)
}

pub fn add_and_activate_saved_model(input: SavedModelInput) -> Result<RuntimeProvider> {
    let saved_model = add_saved_model(input)?;
    activate_saved_model(&saved_model.model_id)
}

pub fn activate_saved_model(model_id: &str) -> Result<RuntimeProvider> {
    let mut store = load_saved_model_store()?;
    let saved_model = store
        .models
        .iter()
        .find(|saved_model| saved_model.model_id == model_id)
        .cloned()
        .ok_or_else(|| anyhow!("saved model not found: {model_id}"))?;
    let runtime = resolve_saved_model_runtime(&saved_model)?;

    if let Some(saved_model) = store
        .models
        .iter_mut()
        .find(|saved_model| saved_model.model_id == model_id)
    {
        saved_model.last_used_at = Some(now_rfc3339());
    }
    store.active_model_id = Some(model_id.to_string());
    save_saved_model_store(&store)?;
    persist_active_runtime_fields(&runtime)?;
    Ok(runtime)
}

pub fn delete_saved_model(model_id: &str) -> Result<Option<RuntimeProvider>> {
    let mut store = load_saved_model_store()?;
    let deleted_was_active = store
        .active_model_id
        .as_deref()
        .is_some_and(|active| active == model_id);
    let before = store.models.len();
    store
        .models
        .retain(|saved_model| saved_model.model_id != model_id);
    if before == store.models.len() {
        bail!("saved model not found: {model_id}");
    }

    let mut replacement_runtime = None;
    if deleted_was_active {
        if let Some(replacement_id) = latest_created_saved_model_id(&store.models) {
            let replacement = store
                .models
                .iter()
                .find(|saved_model| saved_model.model_id == replacement_id)
                .cloned()
                .ok_or_else(|| anyhow!("replacement saved model not found: {replacement_id}"))?;
            let runtime = resolve_saved_model_runtime(&replacement)?;
            if let Some(saved_model) = store
                .models
                .iter_mut()
                .find(|saved_model| saved_model.model_id == replacement_id)
            {
                saved_model.last_used_at = Some(now_rfc3339());
            }
            store.active_model_id = Some(replacement_id);
            replacement_runtime = Some(runtime);
        } else {
            store.active_model_id = None;
        }
    }

    save_saved_model_store(&store)?;
    if let Some(runtime) = replacement_runtime.as_ref() {
        persist_active_runtime_fields(runtime)?;
    } else if deleted_was_active {
        clear_active_runtime_fields()?;
    }
    remove_model_credentials(model_id)?;
    Ok(replacement_runtime)
}

pub fn resolve_active_saved_model_runtime() -> Result<Option<RuntimeProvider>> {
    let store = load_saved_model_store()?;
    let Some(active_id) = store.active_model_id.as_deref() else {
        return Ok(None);
    };
    let saved_model = store
        .models
        .iter()
        .find(|saved_model| saved_model.model_id == active_id)
        .ok_or_else(|| anyhow!("active saved model not found: {active_id}"))?;
    resolve_saved_model_runtime(saved_model).map(Some)
}

pub fn resolve_saved_model_runtime_by_id(model_id: &str) -> Result<RuntimeProvider> {
    let store = load_saved_model_store()?;
    let saved_model = store
        .models
        .iter()
        .find(|saved_model| saved_model.model_id == model_id)
        .ok_or_else(|| anyhow!("saved model not found: {model_id}"))?;
    resolve_saved_model_runtime(saved_model)
}

pub fn request_candidate_runtimes(primary: &RuntimeProvider) -> Result<Vec<RuntimeProvider>> {
    let store = load_saved_model_store()?;
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    if let Some(active_id) = store.active_model_id.as_deref() {
        if let Some(runtime) = store
            .models
            .iter()
            .find(|saved_model| saved_model.model_id == active_id)
            .and_then(|saved_model| resolve_saved_model_runtime(saved_model).ok())
        {
            remember_runtime(&mut seen, &runtime);
            candidates.push(runtime);
        }
    }

    if candidates.is_empty() {
        remember_runtime(&mut seen, primary);
        candidates.push(primary.clone());
    }

    let mut saved_models = store.models;
    saved_models.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.model_id.cmp(&a.model_id))
    });
    for saved_model in saved_models {
        if seen.contains(&runtime_dedupe_key(
            Some(&saved_model.model_id),
            &saved_model.provider,
            &saved_model.model,
            saved_model.base_url.as_deref().unwrap_or_default(),
        )) {
            continue;
        }
        let Ok(runtime) = resolve_saved_model_runtime(&saved_model) else {
            continue;
        };
        if remember_runtime(&mut seen, &runtime) {
            candidates.push(runtime);
        }
    }

    Ok(candidates)
}

pub fn resolve_saved_model_runtime(saved_model: &SavedModel) -> Result<RuntimeProvider> {
    let provider = ProviderKind::parse(&saved_model.provider).with_context(|| {
        format!(
            "unsupported provider in saved model {}",
            saved_model.model_id
        )
    })?;
    let base_url = saved_model
        .base_url
        .clone()
        .or_else(|| provider.default_base_url().map(str::to_string))
        .ok_or_else(|| anyhow!("saved model {} is missing a base URL", saved_model.model_id))?;
    let api_mode = resolve_provider_api_mode(
        provider,
        &saved_model.model,
        &base_url,
        saved_model.api_mode,
    );
    let auth = load_auth_store().unwrap_or_default();
    let model_credentials = auth.model_credentials.get(&saved_model.model_id);
    let provider_credentials = auth.providers.get(provider.as_str());
    let resolved_credentials = crate::auth::resolve_provider_credentials(provider, true)
        .ok()
        .flatten();
    let api_key = model_credentials
        .and_then(entry_secret)
        .or_else(|| {
            resolved_credentials
                .as_ref()
                .map(ProviderCredentials::as_api_key)
        })
        .or_else(|| provider_credentials.and_then(entry_secret))
        .unwrap_or_default();

    let missing_secret = api_key.is_empty()
        && provider.requires_secret()
        && !matches!(provider, ProviderKind::Custom | ProviderKind::AzureFoundry);
    if missing_secret {
        bail!(
            "missing API key/token for saved model {}",
            saved_model.model_id
        );
    }

    Ok(RuntimeProvider {
        model_id: Some(saved_model.model_id.clone()),
        provider,
        model: saved_model.model.clone(),
        base_url,
        api_key,
        api_mode,
        source: "model".to_string(),
        account_id: None,
    })
}

fn remember_runtime(seen: &mut HashSet<String>, runtime: &RuntimeProvider) -> bool {
    seen.insert(runtime_dedupe_key(
        runtime.model_id.as_deref(),
        runtime.provider.as_str(),
        &runtime.model,
        &runtime.base_url,
    ))
}

fn runtime_dedupe_key(
    model_id: Option<&str>,
    provider: &str,
    model: &str,
    base_url: &str,
) -> String {
    model_id
        .filter(|id| !id.trim().is_empty())
        .map(|id| format!("id:{id}"))
        .unwrap_or_else(|| format!("runtime:{provider}:{model}:{base_url}"))
}

pub fn model_id_from_session(session_model_id: Option<&str>) -> Option<String> {
    session_model_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub fn saved_model_endpoint_label(saved_model: &SavedModel) -> String {
    let Some(base_url) = saved_model
        .base_url
        .as_deref()
        .filter(|value| !value.is_empty())
    else {
        return "default".to_string();
    };
    if let Ok(url) = Url::parse(base_url) {
        if let Some(host) = url.host_str() {
            return host.to_string();
        }
    }
    base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

fn latest_created_saved_model_id(models: &[SavedModel]) -> Option<String> {
    models
        .iter()
        .max_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.model_id.cmp(&b.model_id))
        })
        .map(|saved_model| saved_model.model_id.clone())
}

pub fn compact_timestamp(value: &str) -> String {
    value
        .strip_suffix("+00:00")
        .or_else(|| value.strip_suffix('Z'))
        .unwrap_or(value)
        .replace('T', " ")
}

fn save_saved_model_store(store: &SavedModelStore) -> Result<()> {
    let mut config = DuckAgentConfig::load_active_profile()?;
    set_store_fields(config.raw_mut(), store)?;
    config.save_active_profile()
}

fn set_store_fields(raw: &mut Map<String, Value>, store: &SavedModelStore) -> Result<()> {
    match store.active_model_id.as_deref() {
        Some(active) => {
            raw.insert(
                ACTIVE_MODEL_ID_FIELD.to_string(),
                Value::String(active.to_string()),
            );
        }
        None => {
            raw.remove(ACTIVE_MODEL_ID_FIELD);
        }
    }
    raw.insert(
        SAVED_MODELS_FIELD.to_string(),
        serde_json::to_value(&store.models).context("failed to serialize saved models")?,
    );
    Ok(())
}

fn persist_active_runtime_fields(runtime: &RuntimeProvider) -> Result<()> {
    let mut config = DuckAgentConfig::load_active_profile()?;
    let raw = config.raw_mut();
    if let Some(model_id) = runtime.model_id.as_deref() {
        raw.insert(
            ACTIVE_MODEL_ID_FIELD.to_string(),
            Value::String(model_id.to_string()),
        );
    }
    raw.insert(
        "provider".to_string(),
        Value::String(runtime.provider.as_str().to_string()),
    );
    raw.insert("model".to_string(), Value::String(runtime.model.clone()));
    raw.insert(
        "base_url".to_string(),
        Value::String(runtime.base_url.clone()),
    );
    raw.insert(
        "api_mode".to_string(),
        Value::String(runtime.api_mode.as_str().to_string()),
    );
    config.save_active_profile()
}

fn clear_active_runtime_fields() -> Result<()> {
    let mut config = DuckAgentConfig::load_active_profile()?;
    let raw = config.raw_mut();
    raw.remove(ACTIVE_MODEL_ID_FIELD);
    raw.remove("provider");
    raw.remove("model");
    raw.remove("base_url");
    raw.remove("api_mode");
    config.save_active_profile()
}

fn oauth_or_inherited_credential_label(saved_model: &SavedModel) -> Option<String> {
    let provider = ProviderKind::parse(&saved_model.provider)?;
    if matches!(
        provider,
        ProviderKind::OpenAiCodex
            | ProviderKind::Nous
            | ProviderKind::QwenOauth
            | ProviderKind::GoogleGeminiCli
            | ProviderKind::Bedrock
            | ProviderKind::CopilotAcp
    ) {
        return Some("oauth".to_string());
    }
    load_auth_store()
        .ok()
        .and_then(|auth| auth.providers.get(provider.as_str()).and_then(entry_secret))
        .map(|secret| secret_fingerprint(&secret))
}

fn entry_secret(entry: &ProviderCredentialEntry) -> Option<String> {
    entry
        .api_key
        .clone()
        .or_else(|| entry.token.clone())
        .filter(|value| !value.trim().is_empty())
}

fn secret_fingerprint(secret: &str) -> String {
    let trimmed = secret.trim();
    if trimmed.is_empty() {
        return "-".to_string();
    }
    let tail = trimmed
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("...{tail}")
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_fingerprint_uses_only_tail() {
        assert_eq!(secret_fingerprint("sk-abcdef"), "...cdef");
        assert_eq!(secret_fingerprint("abc"), "...abc");
    }

    #[test]
    fn endpoint_label_prefers_host() {
        let saved_model = SavedModel {
            model_id: "m1".to_string(),
            provider: "deepseek".to_string(),
            model: "deepseek-chat".to_string(),
            base_url: Some("https://api.deepseek.com/v1".to_string()),
            api_mode: None,
            created_at: "2026-05-17T00:00:00Z".to_string(),
            last_used_at: None,
        };
        assert_eq!(saved_model_endpoint_label(&saved_model), "api.deepseek.com");
    }

    #[test]
    fn session_model_id_ignores_blank() {
        assert_eq!(model_id_from_session(Some("  ")), None);
        assert_eq!(model_id_from_session(Some("m1")), Some("m1".to_string()));
    }

    #[test]
    fn saved_model_serializes_model_id() {
        let value = serde_json::to_value(SavedModel {
            model_id: "m1".to_string(),
            provider: "deepseek".to_string(),
            model: "deepseek-chat".to_string(),
            base_url: None,
            api_mode: None,
            created_at: "2026-05-17T00:00:00Z".to_string(),
            last_used_at: None,
        })
        .unwrap();
        assert_eq!(value["model_id"], "m1");
        assert!(value.get("id").is_none());
    }

    #[test]
    fn latest_created_saved_model_uses_created_at() {
        let models = vec![
            SavedModel {
                model_id: "older".to_string(),
                provider: "deepseek".to_string(),
                model: "deepseek-chat".to_string(),
                base_url: None,
                api_mode: None,
                created_at: "2026-05-17T00:00:00Z".to_string(),
                last_used_at: Some("2026-05-18T00:00:00Z".to_string()),
            },
            SavedModel {
                model_id: "newer".to_string(),
                provider: "deepseek".to_string(),
                model: "deepseek-chat".to_string(),
                base_url: None,
                api_mode: None,
                created_at: "2026-05-17T01:00:00Z".to_string(),
                last_used_at: None,
            },
        ];
        assert_eq!(
            latest_created_saved_model_id(&models),
            Some("newer".to_string())
        );
    }

    #[test]
    fn runtime_dedupe_prefers_model_id_identity() {
        assert_eq!(
            runtime_dedupe_key(Some("m1"), "deepseek", "a", "https://one"),
            runtime_dedupe_key(Some("m1"), "openai", "b", "https://two")
        );
        assert_ne!(
            runtime_dedupe_key(None, "deepseek", "a", "https://one"),
            runtime_dedupe_key(None, "deepseek", "a", "https://two")
        );
    }
}
