use crate::profiles;
use crate::sandbox::config::SandboxConfig;
use crate::web::WebConfig;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum McpTransportKind {
    Stdio,
    Http,
    Sse,
}

impl McpTransportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
            Self::Sse => "sse",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct McpServerConfig {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<McpTransportKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

impl McpServerConfig {
    pub fn effective_transport(&self) -> Result<McpTransportKind> {
        if let Some(transport) = self.transport {
            return Ok(transport);
        }
        if self
            .command
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Ok(McpTransportKind::Stdio);
        }
        if self
            .url
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Ok(McpTransportKind::Http);
        }
        bail!("MCP server config must contain either command or url")
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn timeout_ms(&self) -> u64 {
        self.timeout.unwrap_or(10_000)
    }
}

#[derive(Debug, Clone, Default)]
pub struct DuckAgentConfig {
    raw: Map<String, Value>,
}

impl DuckAgentConfig {
    pub fn load_global() -> Result<Self> {
        Self::load_from_path(&global_config_path()?)
    }

    pub fn load_active_profile() -> Result<Self> {
        Ok(Self {
            raw: profiles::load_active_profile_config()?,
        })
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read config: {}", path.display()))?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        Self::from_str(&text).with_context(|| format!("failed to parse config: {}", path.display()))
    }

    pub fn from_str(text: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(text)?;
        match value {
            Value::Object(raw) => Ok(Self { raw }),
            _ => bail!("global config must be a JSON object"),
        }
    }

    pub fn save_global(&self) -> Result<()> {
        self.save_to_path(&global_config_path()?)
    }

    pub fn save_active_profile(&self) -> Result<()> {
        profiles::save_active_profile_config(&self.raw)
    }

    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        write_json_file(path, &Value::Object(self.raw.clone()))
    }

    pub fn raw(&self) -> &Map<String, Value> {
        &self.raw
    }

    pub fn raw_mut(&mut self) -> &mut Map<String, Value> {
        &mut self.raw
    }

    pub fn mcp_servers(&self) -> Result<BTreeMap<String, McpServerConfig>> {
        match self.raw.get("mcpServers") {
            None | Some(Value::Null) => Ok(BTreeMap::new()),
            Some(value) => {
                serde_json::from_value(value.clone()).context("failed to parse mcpServers")
            }
        }
    }

    pub fn set_mcp_servers(&mut self, servers: BTreeMap<String, McpServerConfig>) -> Result<()> {
        self.raw.insert(
            "mcpServers".to_string(),
            serde_json::to_value(servers).context("failed to serialize mcpServers")?,
        );
        Ok(())
    }

    pub fn sandbox_config(&self) -> Result<SandboxConfig> {
        match self.raw.get("sandbox") {
            None | Some(Value::Null) => Ok(SandboxConfig::default()),
            Some(value) => {
                serde_json::from_value(value.clone()).context("failed to parse sandbox config")
            }
        }
    }

    pub fn set_sandbox_config(&mut self, sandbox: SandboxConfig) -> Result<()> {
        self.raw.insert(
            "sandbox".to_string(),
            serde_json::to_value(sandbox).context("failed to serialize sandbox config")?,
        );
        Ok(())
    }

    pub fn web_config(&self) -> Result<WebConfig> {
        match self.raw.get("web") {
            None | Some(Value::Null) => Ok(WebConfig::default()),
            Some(value) => {
                serde_json::from_value(value.clone()).context("failed to parse web config")
            }
        }
    }

    pub fn set_web_config(&mut self, web: WebConfig) -> Result<()> {
        self.raw.insert(
            "web".to_string(),
            serde_json::to_value(web).context("failed to serialize web config")?,
        );
        Ok(())
    }
}

pub fn load_mcp_servers() -> Result<BTreeMap<String, McpServerConfig>> {
    DuckAgentConfig::load_active_profile()?.mcp_servers()
}

pub fn save_mcp_servers(servers: BTreeMap<String, McpServerConfig>) -> Result<()> {
    let mut config = DuckAgentConfig::load_active_profile()?;
    config.set_mcp_servers(servers)?;
    config.save_active_profile()
}

pub fn global_config_path() -> Result<PathBuf> {
    profiles::root_config_path()
}

pub fn write_json_file(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory: {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(value).context("failed to serialize JSON file")?;
    fs::write(path, format!("{text}\n"))
        .with_context(|| format!("failed to write JSON file: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_claude_style_stdio_server_and_defaults() -> Result<()> {
        let config = DuckAgentConfig::from_str(
            r#"{
              "mcpServers": {
                "playwright": {
                  "command": "npx",
                  "args": ["@playwright/mcp@latest"]
                }
              }
            }"#,
        )?;
        let servers = config.mcp_servers()?;
        let server = servers.get("playwright").unwrap();
        assert_eq!(server.effective_transport()?, McpTransportKind::Stdio);
        assert!(server.is_enabled());
        assert_eq!(server.timeout_ms(), 10_000);
        Ok(())
    }

    #[test]
    fn preserves_unknown_fields_when_setting_mcp_servers() -> Result<()> {
        let mut config = DuckAgentConfig::from_str(
            r#"{
              "provider": "openai",
              "unknown": {"keep": true}
            }"#,
        )?;
        let mut servers = BTreeMap::new();
        servers.insert(
            "context7".to_string(),
            McpServerConfig {
                transport: Some(McpTransportKind::Http),
                url: Some("https://mcp.context7.com/mcp".to_string()),
                oauth: Some(json!(true)),
                ..Default::default()
            },
        );
        config.set_mcp_servers(servers)?;
        assert_eq!(config.raw()["provider"], json!("openai"));
        assert_eq!(config.raw()["unknown"]["keep"], json!(true));
        assert!(config.raw().contains_key("mcpServers"));
        Ok(())
    }

    #[test]
    fn preserves_mcp_and_unknown_fields_when_setting_sandbox() -> Result<()> {
        let mut config = DuckAgentConfig::from_str(
            r#"{
              "mcpServers": {
                "docs": {"type": "http", "url": "https://example.com/mcp"}
              },
              "unknown": {"keep": true}
            }"#,
        )?;
        let mut sandbox = SandboxConfig::default();
        sandbox.preset = "danger".to_string();
        config.set_sandbox_config(sandbox)?;
        assert!(config.raw().contains_key("mcpServers"));
        assert_eq!(config.raw()["unknown"]["keep"], json!(true));
        assert_eq!(config.raw()["sandbox"]["preset"], json!("danger"));
        Ok(())
    }

    #[test]
    fn missing_or_empty_sandbox_config_resolves_to_workspace() -> Result<()> {
        for text in [r#"{}"#, r#"{"sandbox": {}}"#] {
            let config = DuckAgentConfig::from_str(text)?;
            let mut sandbox = config.sandbox_config()?;
            sandbox.ensure_builtin_defaults();
            let resolved = sandbox.resolve(None)?;
            assert_eq!(resolved.name, "workspace");
        }
        Ok(())
    }
}
