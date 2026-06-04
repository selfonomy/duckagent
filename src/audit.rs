use crate::profiles;
use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct AuditEvent {
    pub id: String,
    pub timestamp: String,
    pub category: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub fields: Value,
}

impl AuditEvent {
    pub fn new(category: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            timestamp: Utc::now().to_rfc3339(),
            category: category.into(),
            action: action.into(),
            session_id: None,
            sandbox: None,
            target: None,
            outcome: "ok".to_string(),
            fields: Value::Null,
        }
    }
}

pub fn record(event: AuditEvent) {
    if audit_disabled() {
        return;
    }
    if let Err(error) = append_event(event) {
        eprintln!("duckagent audit write failed: {error:#}");
    }
}

fn append_event(event: AuditEvent) -> Result<()> {
    let dir = audit_dir()?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create audit directory: {}", dir.display()))?;
    let path = dir.join("events.ndjson");
    let line = serde_json::to_string(&event).context("failed to serialize audit event")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open audit file: {}", path.display()))?;
    writeln!(file, "{line}")
        .with_context(|| format!("failed to append audit file: {}", path.display()))
}

fn audit_dir() -> Result<PathBuf> {
    profiles::active_profile_path("audit")
}

fn audit_disabled() -> bool {
    #[cfg(test)]
    {
        if std::env::var("DUCKAGENT_AUDIT_TEST").ok().as_deref() != Some("1") {
            return true;
        }
    }
    std::env::var("DUCKAGENT_AUDIT")
        .map(|value| matches!(value.trim(), "0" | "false" | "FALSE" | "off" | "OFF"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_event_serializes_without_values() {
        let mut event = AuditEvent::new("tool", "call");
        event.target = Some("read_file".to_string());
        event.fields = serde_json::json!({"args": {"path": "src/main.rs"}});
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"category\":\"tool\""));
        assert!(json.contains("\"target\":\"read_file\""));
    }
}
