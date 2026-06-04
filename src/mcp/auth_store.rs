use crate::profiles;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const MCP_AUTH_FILE_NAME: &str = "mcp-auth.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct McpAuthStore {
    #[serde(default)]
    pub servers: BTreeMap<String, McpAuthEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct McpAuthEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl McpAuthStore {
    pub fn load_active_profile() -> Result<Self> {
        Self::load_from_path(&mcp_auth_path()?)
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read MCP auth store: {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("failed to parse MCP auth store: {}", path.display()))
    }

    pub fn save_active_profile(&self) -> Result<()> {
        self.save_to_path(&mcp_auth_path()?)
    }

    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory: {}", parent.display()))?;
        }
        let text =
            serde_json::to_string_pretty(self).context("failed to serialize MCP auth store")?;
        fs::write(path, format!("{text}\n"))
            .with_context(|| format!("failed to write MCP auth store: {}", path.display()))
    }

    pub fn remove_server(&mut self, name: &str) -> bool {
        self.servers.remove(name).is_some()
    }
}

pub fn mcp_auth_path() -> Result<PathBuf> {
    profiles::active_profile_path(MCP_AUTH_FILE_NAME)
}

pub fn load_access_token(server_name: &str) -> Result<Option<String>> {
    Ok(McpAuthStore::load_active_profile()?
        .servers
        .get(server_name)
        .and_then(|entry| entry.access_token.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_server_reports_whether_entry_existed() {
        let mut store = McpAuthStore::default();
        store.servers.insert(
            "sentry".to_string(),
            McpAuthEntry {
                access_token: Some("token".to_string()),
                ..Default::default()
            },
        );
        assert!(store.remove_server("sentry"));
        assert!(!store.remove_server("sentry"));
    }
}
