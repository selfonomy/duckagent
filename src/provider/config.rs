use super::{ApiMode, ModelCapabilities, RuntimeProvider};
use crate::mcp::config::DuckAgentConfig;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalRuntimeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_mode: Option<ApiMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalProviderConfig {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, ModelCapabilities>,
}

pub fn load_global_runtime_config() -> Result<GlobalRuntimeConfig> {
    let config = DuckAgentConfig::load_active_profile()?;
    serde_json::from_value(serde_json::Value::Object(config.raw().clone()))
        .context("failed to parse active profile config runtime fields")
}

pub fn save_global_runtime_config(runtime: &RuntimeProvider) -> Result<()> {
    let mut config = DuckAgentConfig::load_active_profile()?;
    match runtime.model_id.as_deref() {
        Some(model_id) => {
            config.raw_mut().insert(
                "active_model_id".to_string(),
                serde_json::Value::String(model_id.to_string()),
            );
        }
        None => {
            config.raw_mut().remove("active_model_id");
        }
    }
    let runtime_config = GlobalRuntimeConfig {
        provider: Some(runtime.provider.as_str().to_string()),
        model: Some(runtime.model.clone()),
        base_url: Some(runtime.base_url.clone()),
        api_mode: Some(runtime.api_mode),
    };
    let serialized =
        serde_json::to_value(runtime_config).context("failed to serialize runtime config")?;
    if let serde_json::Value::Object(fields) = serialized {
        for (key, value) in fields {
            config.raw_mut().insert(key, value);
        }
    }
    config.save_active_profile()
}

pub fn load_provider_configs() -> Result<BTreeMap<String, GlobalProviderConfig>> {
    let config = DuckAgentConfig::load_active_profile()?;
    match config.raw().get("providers") {
        None | Some(serde_json::Value::Null) => Ok(BTreeMap::new()),
        Some(value) => serde_json::from_value(value.clone())
            .context("failed to parse active profile config providers"),
    }
}

pub fn load_model_capabilities_override(
    provider: &str,
    model: &str,
) -> Result<Option<ModelCapabilities>> {
    let providers = load_provider_configs()?;
    Ok(providers
        .get(provider)
        .and_then(|provider_config| provider_config.models.get(model))
        .cloned())
}

pub fn load_provider_model_capability_overrides(
    provider: &str,
) -> Result<BTreeMap<String, ModelCapabilities>> {
    let providers = load_provider_configs()?;
    Ok(providers
        .get(provider)
        .map(|provider_config| provider_config.models.clone())
        .unwrap_or_default())
}

pub fn save_model_capabilities_override(
    provider: &str,
    model: &str,
    capabilities: ModelCapabilities,
) -> Result<()> {
    let mut config = DuckAgentConfig::load_active_profile()?;
    set_model_capabilities_override(&mut config, provider, model, capabilities)?;
    config.save_active_profile()
}

fn set_model_capabilities_override(
    config: &mut DuckAgentConfig,
    provider: &str,
    model: &str,
    capabilities: ModelCapabilities,
) -> Result<()> {
    let providers = object_field(config.raw_mut(), "providers")?;
    let provider_config = object_entry(providers, provider)?;
    let models = object_field(provider_config, "models")?;
    models.insert(
        model.to_string(),
        serde_json::to_value(capabilities)
            .context("failed to serialize model capability override")?,
    );
    Ok(())
}

fn object_field<'a>(
    object: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>> {
    if !object.contains_key(key) {
        object.insert(key.to_string(), Value::Object(Map::new()));
    }
    object
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("active profile config field `{key}` must be an object"))
}

fn object_entry<'a>(
    object: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>> {
    if key.trim().is_empty() {
        bail!("provider/model capability override key must not be empty");
    }
    if !object.contains_key(key) {
        object.insert(key.to_string(), Value::Object(Map::new()));
    }
    object
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("active profile config providers.{key} must be an object"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn set_model_override_preserves_provider_unknown_fields() -> Result<()> {
        let mut config = DuckAgentConfig::from_str(
            r#"{
                "theme": "dark",
                "providers": {
                    "custom": {
                        "base_url": "https://example.com/v1",
                        "models": {
                            "old-model": { "context_window": 64000 }
                        }
                    }
                }
            }"#,
        )?;

        set_model_capabilities_override(
            &mut config,
            "custom",
            "new-model",
            ModelCapabilities {
                context_window: Some(32_000),
                ..ModelCapabilities::default()
            },
        )?;

        assert_eq!(config.raw().get("theme"), Some(&json!("dark")));
        assert_eq!(
            config.raw()["providers"]["custom"]["base_url"],
            json!("https://example.com/v1")
        );
        assert_eq!(
            config.raw()["providers"]["custom"]["models"]["old-model"]["context_window"],
            json!(64_000)
        );
        assert_eq!(
            config.raw()["providers"]["custom"]["models"]["new-model"]["context_window"],
            json!(32_000)
        );
        Ok(())
    }
}
