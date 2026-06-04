use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const PAIRING_CODE_TTL_SECONDS: i64 = 60 * 60;
const PAIRING_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

#[derive(Clone)]
pub(crate) struct GatewayPairingStore {
    dir: PathBuf,
    lock: Arc<Mutex<()>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PairingPendingRecord {
    pub channel: String,
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_label: Option<String>,
    pub code: String,
    pub created_at: i64,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PairingApprovedRecord {
    pub channel: String,
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_label: Option<String>,
    pub approved_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PairingNotice {
    pub code: String,
    pub expires_at: i64,
    pub reused_existing: bool,
}

impl GatewayPairingStore {
    pub(crate) fn new(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create gateway pairing dir: {}", dir.display()))?;
        Ok(Self {
            dir,
            lock: Arc::new(Mutex::new(())),
        })
    }

    pub(crate) fn is_approved(&self, channel: &str, user_id: &str) -> bool {
        let _guard = self.lock.lock().expect("gateway pairing mutex poisoned");
        let path = self.approved_path(channel);
        read_json_vec::<PairingApprovedRecord>(&path)
            .map(|records| {
                records
                    .into_iter()
                    .any(|record| record.channel == channel && record.user_id == user_id)
            })
            .unwrap_or(false)
    }

    pub(crate) fn ensure_pending_code(
        &self,
        channel: &str,
        user_id: &str,
        user_label: Option<String>,
    ) -> Result<PairingNotice> {
        let _guard = self.lock.lock().expect("gateway pairing mutex poisoned");
        let now = now_ts();
        let path = self.pending_path(channel);
        let mut records = read_json_vec::<PairingPendingRecord>(&path)?
            .into_iter()
            .filter(|record| record.expires_at > now)
            .collect::<Vec<_>>();
        if let Some(existing) = records
            .iter()
            .find(|record| record.user_id == user_id)
            .cloned()
        {
            write_json_vec(&path, &records)?;
            return Ok(PairingNotice {
                code: existing.code,
                expires_at: existing.expires_at,
                reused_existing: true,
            });
        }
        let code = generate_pairing_code(channel, user_id);
        let record = PairingPendingRecord {
            channel: channel.to_string(),
            user_id: user_id.to_string(),
            user_label,
            code: code.clone(),
            created_at: now,
            expires_at: now + PAIRING_CODE_TTL_SECONDS,
        };
        records.push(record);
        write_json_vec(&path, &records)?;
        Ok(PairingNotice {
            code,
            expires_at: now + PAIRING_CODE_TTL_SECONDS,
            reused_existing: false,
        })
    }

    pub(crate) fn approve_code(
        &self,
        code: &str,
        channel: Option<&str>,
    ) -> Result<Option<PairingApprovedRecord>> {
        let _guard = self.lock.lock().expect("gateway pairing mutex poisoned");
        let normalized_code = normalize_code(code);
        let now = now_ts();
        for path in self.pending_paths(channel)? {
            let mut records = read_json_vec::<PairingPendingRecord>(&path)?
                .into_iter()
                .filter(|record| record.expires_at > now)
                .collect::<Vec<_>>();
            let Some(index) = records
                .iter()
                .position(|record| normalize_code(&record.code) == normalized_code)
            else {
                write_json_vec(&path, &records)?;
                continue;
            };
            let pending = records.remove(index);
            write_json_vec(&path, &records)?;
            let approved = PairingApprovedRecord {
                channel: pending.channel,
                user_id: pending.user_id,
                user_label: pending.user_label,
                approved_at: now,
            };
            let approved_path = self.approved_path(&approved.channel);
            let mut approved_records = read_json_vec::<PairingApprovedRecord>(&approved_path)?;
            if !approved_records.iter().any(|record| {
                record.channel == approved.channel && record.user_id == approved.user_id
            }) {
                approved_records.push(approved.clone());
                write_json_vec(&approved_path, &approved_records)?;
            }
            return Ok(Some(approved));
        }
        Ok(None)
    }

    pub(crate) fn approve_user(
        &self,
        channel: &str,
        user_id: &str,
        user_label: Option<String>,
    ) -> Result<PairingApprovedRecord> {
        let _guard = self.lock.lock().expect("gateway pairing mutex poisoned");
        let now = now_ts();
        let approved = PairingApprovedRecord {
            channel: channel.to_string(),
            user_id: user_id.to_string(),
            user_label,
            approved_at: now,
        };
        let approved_path = self.approved_path(channel);
        let mut approved_records = read_json_vec::<PairingApprovedRecord>(&approved_path)?;
        if !approved_records
            .iter()
            .any(|record| record.channel == approved.channel && record.user_id == approved.user_id)
        {
            approved_records.push(approved.clone());
            write_json_vec(&approved_path, &approved_records)?;
        }
        Ok(approved)
    }

    pub(crate) fn revoke(
        &self,
        user_id: &str,
        channel: Option<&str>,
    ) -> Result<Vec<PairingApprovedRecord>> {
        let _guard = self.lock.lock().expect("gateway pairing mutex poisoned");
        let mut removed = Vec::new();
        for path in self.approved_paths(channel)? {
            let mut records = read_json_vec::<PairingApprovedRecord>(&path)?;
            let before = records.len();
            records.retain(|record| {
                let keep = record.user_id != user_id;
                if !keep {
                    removed.push(record.clone());
                }
                keep
            });
            if records.len() != before {
                write_json_vec(&path, &records)?;
            }
        }
        Ok(removed)
    }

    pub(crate) fn list_pending(&self, channel: Option<&str>) -> Result<Vec<PairingPendingRecord>> {
        let _guard = self.lock.lock().expect("gateway pairing mutex poisoned");
        let now = now_ts();
        let mut out = Vec::new();
        for path in self.pending_paths(channel)? {
            let records = read_json_vec::<PairingPendingRecord>(&path)?
                .into_iter()
                .filter(|record| record.expires_at > now)
                .collect::<Vec<_>>();
            write_json_vec(&path, &records)?;
            out.extend(records);
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }

    pub(crate) fn list_approved(
        &self,
        channel: Option<&str>,
    ) -> Result<Vec<PairingApprovedRecord>> {
        let _guard = self.lock.lock().expect("gateway pairing mutex poisoned");
        let mut out = Vec::new();
        for path in self.approved_paths(channel)? {
            out.extend(read_json_vec::<PairingApprovedRecord>(&path)?);
        }
        out.sort_by(|a, b| a.approved_at.cmp(&b.approved_at));
        Ok(out)
    }

    fn pending_path(&self, channel: &str) -> PathBuf {
        self.dir
            .join(format!("{}--pending.json", safe_segment(channel)))
    }

    fn approved_path(&self, channel: &str) -> PathBuf {
        self.dir
            .join(format!("{}--approved.json", safe_segment(channel)))
    }

    fn pending_paths(&self, channel: Option<&str>) -> Result<Vec<PathBuf>> {
        self.matching_paths(channel, "--pending.json")
    }

    fn approved_paths(&self, channel: Option<&str>) -> Result<Vec<PathBuf>> {
        self.matching_paths(channel, "--approved.json")
    }

    fn matching_paths(&self, channel: Option<&str>, suffix: &str) -> Result<Vec<PathBuf>> {
        if let Some(channel) = channel {
            let path = if suffix == "--pending.json" {
                self.pending_path(channel)
            } else {
                self.approved_path(channel)
            };
            return Ok(vec![path]);
        }
        let mut out = Vec::new();
        if !self.dir.exists() {
            return Ok(out);
        }
        let channel = channel.map(safe_segment);
        for entry in fs::read_dir(&self.dir)
            .with_context(|| format!("failed to read pairing dir: {}", self.dir.display()))?
        {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if !name.ends_with(suffix) {
                continue;
            }
            if let Some(channel) = channel.as_deref() {
                if !name.starts_with(&format!("{channel}--")) {
                    continue;
                }
            }
            out.push(path);
        }
        Ok(out)
    }
}

fn read_json_vec<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read pairing file: {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&text)
        .with_context(|| format!("failed to parse pairing file: {}", path.display()))
}

fn write_json_vec<T: Serialize>(path: &Path, records: &[T]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create pairing dir: {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(records)?;
    fs::write(path, text)
        .with_context(|| format!("failed to write pairing file: {}", path.display()))
}

fn generate_pairing_code(channel: &str, user_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(channel.as_bytes());
    hasher.update(b"\0");
    hasher.update(user_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(now_ts().to_le_bytes());
    hasher.update(Uuid::now_v7().as_bytes());
    let digest = hasher.finalize();
    let mut code = String::new();
    for byte in digest.iter().take(8) {
        let index = (*byte as usize) % PAIRING_CODE_ALPHABET.len();
        code.push(PAIRING_CODE_ALPHABET[index] as char);
    }
    format!("{}-{}", &code[..4], &code[4..])
}

fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_uppercase())
        .collect()
}

fn safe_segment(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "empty".to_string()
    } else {
        out
    }
}

pub(crate) fn format_pairing_time(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|value| value.to_rfc3339())
        .unwrap_or_else(|| timestamp.to_string())
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn pairing_code_can_be_approved() -> Result<()> {
        let dir = TempDir::new()?;
        let store = GatewayPairingStore::new(dir.path().join("pairing"))?;
        let notice = store.ensure_pending_code("telegram", "u1", None)?;

        assert!(!store.is_approved("telegram", "u1"));
        let approved = store
            .approve_code(&notice.code, None)?
            .expect("code should approve");
        assert_eq!(approved.user_id, "u1");
        assert!(store.is_approved("telegram", "u1"));
        Ok(())
    }

    #[test]
    fn pairing_reuses_existing_pending_code() -> Result<()> {
        let dir = TempDir::new()?;
        let store = GatewayPairingStore::new(dir.path().join("pairing"))?;
        let first = store.ensure_pending_code("slack", "U1", None)?;
        let second = store.ensure_pending_code("slack", "U1", None)?;

        assert_eq!(first.code, second.code);
        assert!(second.reused_existing);
        Ok(())
    }

    #[test]
    fn pairing_can_preapprove_user_without_code() -> Result<()> {
        let dir = TempDir::new()?;
        let store = GatewayPairingStore::new(dir.path().join("pairing"))?;

        assert!(!store.is_approved("lark", "ou_scanner"));
        let approved = store.approve_user("lark", "ou_scanner", Some("QR scanner".to_string()))?;
        assert_eq!(approved.user_id, "ou_scanner");
        assert!(store.is_approved("lark", "ou_scanner"));

        let approved_again = store.approve_user("lark", "ou_scanner", None)?;
        assert_eq!(approved_again.user_id, "ou_scanner");
        assert_eq!(store.list_approved(Some("lark"))?.len(), 1);
        Ok(())
    }
}
