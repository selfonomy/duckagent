use crate::model::{
    AssistantMessage, LanguageModelResponseContentType, Message, Messages, ToolCallInfo,
    ToolResultInfo,
};
use crate::profiles;
use crate::provider::SessionRuntimeConfig;
use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const DEFAULT_TITLE: &str = "Untitled session";
const META_FILE_NAME: &str = "meta.json";
const MESSAGES_FILE_NAME: &str = "messages.jsonl";
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct SessionManager {
    sessions_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLine {
    pub timestamp: String,
    #[serde(flatten)]
    pub entry: SessionEntry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum SessionEntry {
    SessionMeta(SessionMeta),
    ResponseItem(SessionMessage),
    ProcessEvent(ProcessEventPayload),
    UserTurn(UserTurnPayload),
    FileSnapshot(FileSnapshotPayload),
    Compacted(CompactedPayload),
    Rewound(RewoundPayload),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Main,
    Agent,
}

impl Default for SessionKind {
    fn default() -> Self {
        Self::Main
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub version: u32,
    #[serde(default)]
    pub kind: SessionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub root_id: String,
    #[serde(default = "default_created_by")]
    pub created_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_token_budget: Option<i64>,
    #[serde(default)]
    pub goal_tokens_used: i64,
    #[serde(default)]
    pub goal_time_used_seconds: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_created_at: Option<String>,
    #[serde(flatten, default)]
    pub runtime: SessionRuntimeConfig,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

impl GoalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::Blocked => "blocked",
            GoalStatus::UsageLimited => "usage_limited",
            GoalStatus::BudgetLimited => "budget_limited",
            GoalStatus::Complete => "complete",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value
            .trim()
            .replace('-', "_")
            .replace("usageLimited", "usage_limited")
            .replace("budgetLimited", "budget_limited")
            .to_ascii_lowercase()
            .as_str()
        {
            "active" | "running" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            "blocked" => Some(Self::Blocked),
            "usage_limited" | "usagelimited" => Some(Self::UsageLimited),
            "budget_limited" | "budgetlimited" => Some(Self::BudgetLimited),
            "complete" | "completed" => Some(Self::Complete),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            GoalStatus::Blocked
                | GoalStatus::UsageLimited
                | GoalStatus::BudgetLimited
                | GoalStatus::Complete
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionGoal {
    pub session_id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub token_budget: Option<i64>,
    pub tokens_used: i64,
    pub time_used_seconds: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct SessionScope(String);

impl Default for SessionScope {
    fn default() -> Self {
        Self::main()
    }
}

impl SessionScope {
    pub fn main() -> Self {
        Self("main".to_string())
    }

    pub fn shadow() -> Self {
        Self("shadow".to_string())
    }

    pub fn agent(agent_id: &str) -> Self {
        Self(format!("agent:{agent_id}"))
    }

    pub fn is_main(&self) -> bool {
        self.0 == "main"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentItem {
    InputText { text: String },
    OutputText { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionMessage {
    Message {
        id: String,
        #[serde(default)]
        scope: SessionScope,
        role: SessionRole,
        content: Vec<ContentItem>,
    },
    ToolCall {
        id: String,
        #[serde(default)]
        scope: SessionScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        #[serde(default)]
        scope: SessionScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        output: String,
    },
    ToolApprovalRequest {
        id: String,
        #[serde(default)]
        scope: SessionScope,
        call_id: String,
        tool_name: String,
        command: String,
        rule_hits: Vec<SessionRuleHit>,
        options: Vec<String>,
    },
    ToolApprovalDecision {
        id: String,
        #[serde(default)]
        scope: SessionScope,
        call_id: String,
        tool_name: String,
        command: String,
        decision: String,
        rule_hits: Vec<SessionRuleHit>,
        approved: bool,
    },
    Reasoning {
        id: String,
        #[serde(default)]
        scope: SessionScope,
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRuleHit {
    pub rule_id: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactedPayload {
    pub id: String,
    pub replacement_history: Vec<SessionMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserTurnPayload {
    pub id: String,
    pub message_id: String,
    pub raw_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSnapshotPayload {
    pub id: String,
    pub capability: String,
    pub path: String,
    pub existed_before: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<String>,
    pub existed_after: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256_after: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PendingFileSnapshot {
    id: String,
    capability: String,
    path: PathBuf,
    existed_before: bool,
    sha256_before: Option<String>,
    backup_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewoundPayload {
    pub id: String,
    pub target_index: usize,
    pub target_message_id: String,
    pub target_preview: String,
    pub replacement_history: Vec<SessionMessage>,
    #[serde(default)]
    pub restored_files: Vec<RewindRestoredFile>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindRestoredFile {
    pub path: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256_after: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindListItem {
    pub index: usize,
    pub message_id: String,
    pub preview: String,
}

#[derive(Debug, Clone)]
pub struct RewindResult {
    pub target: RewindListItem,
    pub restored_files: Vec<RewindRestoredFile>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessEventPayload {
    pub id: String,
    pub process_id: String,
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
    pub on_event: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactRequest {
    pub old_item_ids: Vec<String>,
    pub new_items: Vec<CompactNewItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompactNewItem {
    Message {
        role: SessionRole,
        content: Vec<ContentItem>,
    },
    ToolCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        name: String,
        input: Value,
    },
    ToolResult {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        output: String,
    },
}

impl SessionManager {
    pub fn new_default() -> Result<Self> {
        Self::new(profiles::active_profile_dir()?)
    }

    pub fn new(base_dir: PathBuf) -> Result<Self> {
        let sessions_dir = base_dir.join("sessions");
        fs::create_dir_all(&sessions_dir).with_context(|| {
            format!(
                "failed to create sessions directory: {}",
                sessions_dir.display()
            )
        })?;

        Ok(Self { sessions_dir })
    }

    pub fn create_session(&self, title: Option<&str>, system_prompt: &str) -> Result<String> {
        self.create_session_with_runtime(title, system_prompt, SessionRuntimeConfig::default())
    }

    pub fn create_session_with_runtime(
        &self,
        title: Option<&str>,
        system_prompt: &str,
        runtime: SessionRuntimeConfig,
    ) -> Result<String> {
        self.create_session_with_runtime_and_source(title, system_prompt, runtime, "user")
    }

    pub fn create_session_with_runtime_and_source(
        &self,
        title: Option<&str>,
        system_prompt: &str,
        runtime: SessionRuntimeConfig,
        created_by: &str,
    ) -> Result<String> {
        let session_id = new_uuid_string();
        let now = now_rfc3339();
        let created_by = created_by.trim();
        if created_by.is_empty() {
            bail!("created_by must be a non-empty string");
        }
        let meta = SessionMeta {
            id: session_id.clone(),
            title: title
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(DEFAULT_TITLE)
                .to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
            version: SCHEMA_VERSION,
            kind: SessionKind::Main,
            parent_id: None,
            root_id: session_id.clone(),
            created_by: created_by.to_string(),
            goal: None,
            status: None,
            goal_token_budget: None,
            goal_tokens_used: 0,
            goal_time_used_seconds: 0,
            goal_created_at: None,
            runtime,
        };

        fs::create_dir_all(self.session_dir_path(&session_id)).with_context(|| {
            format!("failed to create session directory for session {session_id}")
        })?;
        self.write_session_meta(&meta)?;

        let mut writer = Self::open_append(&self.messages_path(&session_id))?;
        let system_message = Self::new_text_message(SessionRole::System, system_prompt);
        Self::write_line(
            &mut writer,
            &SessionEntry::ResponseItem(system_message),
            &now_rfc3339(),
        )?;

        Ok(session_id)
    }

    pub fn fork_agent_session_from_model_messages(
        &self,
        parent_id: &str,
        title: Option<&str>,
        goal: &str,
        runtime: SessionRuntimeConfig,
        messages: &[Message],
        created_by: &str,
    ) -> Result<String> {
        self.ensure_session_exists(parent_id)?;
        let parent_meta = self.read_session_meta(parent_id)?;
        let session_id = new_uuid_string();
        let now = now_rfc3339();
        let normalized_goal = goal.trim();
        if normalized_goal.is_empty() {
            bail!("agent goal must be a non-empty string");
        }
        let normalized_created_by = created_by.trim();
        if normalized_created_by.is_empty() {
            bail!("agent created_by must be a non-empty string");
        }
        let root_id = if parent_meta.root_id.trim().is_empty() {
            parent_meta.id.clone()
        } else {
            parent_meta.root_id.clone()
        };
        let meta = SessionMeta {
            id: session_id.clone(),
            title: title
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(normalized_goal)
                .to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
            version: SCHEMA_VERSION,
            kind: SessionKind::Agent,
            parent_id: Some(parent_id.to_string()),
            root_id,
            created_by: normalized_created_by.to_string(),
            goal: Some(normalized_goal.to_string()),
            status: Some("running".to_string()),
            goal_token_budget: None,
            goal_tokens_used: 0,
            goal_time_used_seconds: 0,
            goal_created_at: Some(now.clone()),
            runtime,
        };

        fs::create_dir_all(self.session_dir_path(&session_id)).with_context(|| {
            format!("failed to create agent session directory for session {session_id}")
        })?;
        self.write_session_meta(&meta)?;

        let mut writer = Self::open_append(&self.messages_path(&session_id))?;
        for message in messages {
            for item in model_message_to_session_messages(message)? {
                Self::write_line(
                    &mut writer,
                    &SessionEntry::ResponseItem(item),
                    &now_rfc3339(),
                )?;
            }
        }

        Ok(session_id)
    }

    pub fn append_text_message(
        &self,
        session_id: &str,
        role: SessionRole,
        text: impl Into<String>,
    ) -> Result<SessionMessage> {
        let message = Self::new_text_message(role, text);
        self.append_message(session_id, &message)?;
        Ok(message)
    }

    pub fn append_text_message_with_scope(
        &self,
        session_id: &str,
        scope: SessionScope,
        role: SessionRole,
        text: impl Into<String>,
    ) -> Result<SessionMessage> {
        let message = Self::new_text_message_with_scope(scope, role, text);
        self.append_message(session_id, &message)?;
        Ok(message)
    }

    pub fn append_message(&self, session_id: &str, item: &SessionMessage) -> Result<()> {
        self.ensure_session_exists(session_id)?;
        self.append_entry(session_id, SessionEntry::ResponseItem(item.clone()))
    }

    pub fn compact_session(
        &self,
        session_id: &str,
        request: CompactRequest,
    ) -> Result<CompactedPayload> {
        self.ensure_session_exists(session_id)?;
        let visible = self.get_all_messages(session_id)?;
        self.validate_compact_request(&visible, &request)?;
        let new_items = request
            .new_items
            .into_iter()
            .map(materialize_new_item)
            .collect::<Vec<_>>();
        let replacement_history =
            build_replacement_history(&visible, &request.old_item_ids, &new_items)?;
        let compacted = CompactedPayload {
            id: new_uuid_string(),
            replacement_history,
        };
        self.append_entry(session_id, SessionEntry::Compacted(compacted.clone()))?;
        Ok(compacted)
    }

    pub fn append_user_turn_marker(
        &self,
        session_id: &str,
        message_id: &str,
        raw_text: impl Into<String>,
    ) -> Result<UserTurnPayload> {
        self.ensure_session_exists(session_id)?;
        let payload = UserTurnPayload {
            id: new_uuid_string(),
            message_id: message_id.to_string(),
            raw_text: raw_text.into(),
        };
        self.append_entry(session_id, SessionEntry::UserTurn(payload.clone()))?;
        Ok(payload)
    }

    pub fn capture_file_snapshot_before(
        &self,
        session_id: &str,
        capability: &str,
        path: &Path,
    ) -> Result<PendingFileSnapshot> {
        self.ensure_session_exists(session_id)?;
        let id = new_uuid_string();
        let path = path.to_path_buf();
        let (existed_before, sha256_before, backup_path) = match current_file_digest(&path)? {
            FileDigest::Missing => (false, None, None),
            FileDigest::File { sha256, bytes } => {
                let relative = Path::new("runtime")
                    .join("rewind")
                    .join("files")
                    .join(format!("{id}.bin"));
                let backup = self.session_dir_path(session_id).join(&relative);
                if let Some(parent) = backup.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!(
                            "failed to create rewind backup directory: {}",
                            parent.display()
                        )
                    })?;
                }
                fs::write(&backup, &bytes).with_context(|| {
                    format!("failed to write rewind backup: {}", backup.display())
                })?;
                (
                    true,
                    Some(sha256),
                    Some(relative.to_string_lossy().to_string()),
                )
            }
            FileDigest::Directory => {
                bail!(
                    "cannot create rewind file snapshot for directory: {}",
                    path.display()
                );
            }
        };
        Ok(PendingFileSnapshot {
            id,
            capability: capability.to_string(),
            path,
            existed_before,
            sha256_before,
            backup_path,
        })
    }

    pub fn append_file_snapshot_after(
        &self,
        session_id: &str,
        snapshot: PendingFileSnapshot,
    ) -> Result<FileSnapshotPayload> {
        self.ensure_session_exists(session_id)?;
        let (existed_after, sha256_after) = match current_file_digest(&snapshot.path)? {
            FileDigest::Missing => (false, None),
            FileDigest::File { sha256, .. } => (true, Some(sha256)),
            FileDigest::Directory => {
                bail!(
                    "cannot finalize rewind file snapshot for directory: {}",
                    snapshot.path.display()
                );
            }
        };
        let payload = FileSnapshotPayload {
            id: snapshot.id,
            capability: snapshot.capability,
            path: snapshot.path.display().to_string(),
            existed_before: snapshot.existed_before,
            sha256_before: snapshot.sha256_before,
            backup_path: snapshot.backup_path,
            existed_after,
            sha256_after,
        };
        self.append_entry(session_id, SessionEntry::FileSnapshot(payload.clone()))?;
        Ok(payload)
    }

    pub fn rewind_list(&self, session_id: &str) -> Result<Vec<RewindListItem>> {
        let lines = self.load_session_lines(session_id)?;
        let visible = replay_visible_history(&lines)?;
        Ok(build_rewind_items(&lines, &visible))
    }

    pub fn rewind_to_user_turn(&self, session_id: &str, index: usize) -> Result<RewindResult> {
        if index == 0 {
            bail!("rewind index must be >= 1");
        }
        let lines = self.load_session_lines(session_id)?;
        let visible = replay_visible_history(&lines)?;
        let items = build_rewind_items(&lines, &visible);
        let Some(target) = items.iter().find(|item| item.index == index).cloned() else {
            bail!("rewind index {index} is not in the latest /rewind list");
        };
        let Some(visible_target_index) = visible
            .iter()
            .position(|item| item.id() == target.message_id.as_str())
        else {
            bail!("rewind target is no longer visible: {}", target.message_id);
        };
        let replacement_history = visible[..visible_target_index].to_vec();

        let mut warnings = Vec::new();
        let restored_files = if let Some(line_index) =
            line_index_for_message(&lines, &target.message_id)
        {
            restore_file_snapshots_after(session_id, self, &lines, line_index, &mut warnings)
        } else {
            warnings.push(format!(
                "Skipped file restore: target message `{}` was not found as a direct session log line.",
                target.message_id
            ));
            Vec::new()
        };

        let payload = RewoundPayload {
            id: new_uuid_string(),
            target_index: target.index,
            target_message_id: target.message_id.clone(),
            target_preview: target.preview.clone(),
            replacement_history,
            restored_files: restored_files.clone(),
            warnings: warnings.clone(),
        };
        self.append_entry(session_id, SessionEntry::Rewound(payload))?;

        Ok(RewindResult {
            target,
            restored_files,
            warnings,
        })
    }

    pub fn update_title(&self, session_id: &str, title: &str) -> Result<()> {
        self.ensure_session_exists(session_id)?;
        let normalized = title.trim();
        if normalized.is_empty() {
            bail!("title must be a non-empty string");
        }

        let mut meta = self.read_session_meta(session_id)?;
        meta.title = normalized.to_string();
        meta.updated_at = now_rfc3339();
        self.write_session_meta(&meta)
    }

    pub fn update_runtime_config(
        &self,
        session_id: &str,
        runtime: SessionRuntimeConfig,
    ) -> Result<()> {
        self.ensure_session_exists(session_id)?;
        let mut meta = self.read_session_meta(session_id)?;
        meta.runtime = runtime;
        meta.updated_at = now_rfc3339();
        self.write_session_meta(&meta)
    }

    pub fn get_runtime_config(&self, session_id: &str) -> Result<SessionRuntimeConfig> {
        Ok(self.read_session_meta(session_id)?.runtime)
    }

    pub fn get_session_meta(&self, session_id: &str) -> Result<SessionMeta> {
        self.read_session_meta(session_id)
    }

    pub fn get_goal(&self, session_id: &str) -> Result<Option<SessionGoal>> {
        let meta = self.read_session_meta(session_id)?;
        Ok(session_goal_from_meta(&meta))
    }

    pub fn set_goal(
        &self,
        session_id: &str,
        objective: &str,
        token_budget: Option<i64>,
    ) -> Result<SessionGoal> {
        self.ensure_session_exists(session_id)?;
        let objective = objective.trim();
        validate_goal_objective(objective)?;
        validate_goal_budget(token_budget)?;
        let now = now_rfc3339();
        let mut meta = self.read_session_meta(session_id)?;
        meta.goal = Some(objective.to_string());
        meta.status = Some(GoalStatus::Active.as_str().to_string());
        meta.goal_token_budget = token_budget;
        meta.goal_tokens_used = 0;
        meta.goal_time_used_seconds = 0;
        meta.goal_created_at = Some(now.clone());
        meta.updated_at = now;
        self.write_session_meta(&meta)?;
        session_goal_from_meta(&meta).context("session goal missing after set")
    }

    pub fn update_goal_status(&self, session_id: &str, status: GoalStatus) -> Result<SessionGoal> {
        self.ensure_session_exists(session_id)?;
        let mut meta = self.read_session_meta(session_id)?;
        if meta.goal.as_deref().map(str::trim).unwrap_or("").is_empty() {
            bail!("cannot update goal because this session has no goal");
        }
        meta.status = Some(status.as_str().to_string());
        meta.updated_at = now_rfc3339();
        self.write_session_meta(&meta)?;
        session_goal_from_meta(&meta).context("session goal missing after status update")
    }

    pub fn clear_goal(&self, session_id: &str) -> Result<bool> {
        self.ensure_session_exists(session_id)?;
        let mut meta = self.read_session_meta(session_id)?;
        if meta.goal.is_none()
            && meta.goal_token_budget.is_none()
            && meta.goal_tokens_used == 0
            && meta.goal_time_used_seconds == 0
            && meta.goal_created_at.is_none()
        {
            return Ok(false);
        }
        meta.goal = None;
        meta.status = None;
        meta.goal_token_budget = None;
        meta.goal_tokens_used = 0;
        meta.goal_time_used_seconds = 0;
        meta.goal_created_at = None;
        meta.updated_at = now_rfc3339();
        self.write_session_meta(&meta)?;
        Ok(true)
    }

    pub fn add_goal_usage(
        &self,
        session_id: &str,
        token_delta: i64,
        time_delta_seconds: i64,
    ) -> Result<Option<SessionGoal>> {
        self.ensure_session_exists(session_id)?;
        let mut meta = self.read_session_meta(session_id)?;
        if meta.goal.as_deref().map(str::trim).unwrap_or("").is_empty() {
            return Ok(None);
        }
        let status = meta
            .status
            .as_deref()
            .and_then(GoalStatus::parse)
            .unwrap_or(GoalStatus::Active);
        meta.goal_tokens_used = meta.goal_tokens_used.saturating_add(token_delta.max(0));
        meta.goal_time_used_seconds = meta
            .goal_time_used_seconds
            .saturating_add(time_delta_seconds.max(0));
        if status == GoalStatus::Active
            && let Some(token_budget) = meta.goal_token_budget
            && token_budget > 0
            && meta.goal_tokens_used >= token_budget
        {
            meta.status = Some(GoalStatus::BudgetLimited.as_str().to_string());
        }
        meta.updated_at = now_rfc3339();
        self.write_session_meta(&meta)?;
        Ok(session_goal_from_meta(&meta))
    }

    pub fn list_main_session_metas(&self) -> Result<Vec<SessionMeta>> {
        let mut sessions = Vec::new();
        if !self.sessions_dir.exists() {
            return Ok(sessions);
        }
        for entry in fs::read_dir(&self.sessions_dir).with_context(|| {
            format!(
                "failed to read sessions directory: {}",
                self.sessions_dir.display()
            )
        })? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let session_id = entry.file_name().to_string_lossy().to_string();
            match self.read_session_meta(&session_id) {
                Ok(meta) if meta.kind == SessionKind::Main => sessions.push(meta),
                Ok(_) => {}
                Err(_) => {}
            }
        }
        sessions.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(sessions)
    }

    pub fn session_has_custom_title(&self, session_id: &str) -> bool {
        self.read_session_meta(session_id)
            .map(|meta| meta.title.trim() != DEFAULT_TITLE)
            .unwrap_or(false)
    }

    pub fn update_agent_status(&self, session_id: &str, status: &str) -> Result<()> {
        self.ensure_session_exists(session_id)?;
        let normalized = status.trim();
        if normalized.is_empty() {
            bail!("agent status must be a non-empty string");
        }
        let mut meta = self.read_session_meta(session_id)?;
        meta.status = Some(normalized.to_string());
        meta.updated_at = now_rfc3339();
        self.write_session_meta(&meta)
    }

    pub fn get_session_title(&self, session_id: &str) -> Result<String> {
        Ok(self.read_session_meta(session_id)?.title)
    }

    pub fn get_all_messages(&self, session_id: &str) -> Result<Vec<SessionMessage>> {
        let lines = self.load_session_lines(session_id)?;
        replay_visible_history(&lines)
    }

    pub fn get_messages_for_scope(
        &self,
        session_id: &str,
        scope: &SessionScope,
    ) -> Result<Vec<SessionMessage>> {
        let lines = self.load_session_lines(session_id)?;
        replay_scope_history(&lines, scope)
    }

    pub fn to_model_messages(&self, session_id: &str) -> Result<Messages> {
        let visible = self.get_all_messages(session_id)?;
        project_to_model_messages(&visible)
    }

    pub fn project_session_messages_to_model(messages: &[SessionMessage]) -> Result<Messages> {
        project_to_model_messages(messages)
    }

    pub fn to_openai_messages(&self, session_id: &str) -> Result<Messages> {
        self.to_model_messages(session_id)
    }

    pub fn to_model_messages_for_scope(
        &self,
        session_id: &str,
        scope: &SessionScope,
    ) -> Result<Messages> {
        let messages = self.get_messages_for_scope(session_id, scope)?;
        project_to_model_messages(&messages)
    }

    pub fn to_openai_messages_for_scope(
        &self,
        session_id: &str,
        scope: &SessionScope,
    ) -> Result<Messages> {
        self.to_model_messages_for_scope(session_id, scope)
    }

    pub fn handle_cli_compact(&self, session_id: &str, json: &str) -> Result<()> {
        let request: CompactRequest =
            serde_json::from_str(json).context("failed to parse compact json payload")?;
        self.compact_session(session_id, request)?;
        Ok(())
    }

    pub fn handle_cli_get_all_messages(&self, session_id: &str) -> Result<String> {
        let messages = self.get_all_messages(session_id)?;
        serde_json::to_string_pretty(&messages).context("failed to serialize messages as JSON")
    }

    pub fn append_process_event(&self, session_id: &str, event: ProcessEventPayload) -> Result<()> {
        self.ensure_session_exists(session_id)?;
        self.append_entry(session_id, SessionEntry::ProcessEvent(event))
    }

    pub fn session_dir_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(session_id)
    }

    pub fn runtime_dir_path(&self, session_id: &str) -> Result<PathBuf> {
        self.ensure_session_exists(session_id)?;
        let path = self.session_dir_path(session_id).join("runtime");
        fs::create_dir_all(&path).with_context(|| {
            format!(
                "failed to create session runtime directory: {}",
                path.display()
            )
        })?;
        Ok(path)
    }

    pub fn new_text_message(role: SessionRole, text: impl Into<String>) -> SessionMessage {
        Self::new_text_message_with_scope(SessionScope::main(), role, text)
    }

    pub fn new_text_message_with_scope(
        scope: SessionScope,
        role: SessionRole,
        text: impl Into<String>,
    ) -> SessionMessage {
        let content = match role {
            SessionRole::User => vec![ContentItem::InputText { text: text.into() }],
            SessionRole::Assistant | SessionRole::System => {
                vec![ContentItem::OutputText { text: text.into() }]
            }
        };
        SessionMessage::Message {
            id: new_uuid_string(),
            scope,
            role,
            content,
        }
    }

    pub fn new_tool_use_message(
        call_id: impl Into<String>,
        name: impl Into<String>,
        input: Value,
    ) -> SessionMessage {
        Self::new_tool_use_message_with_content(SessionScope::main(), call_id, None, name, input)
    }

    pub fn new_tool_use_message_with_scope(
        scope: SessionScope,
        call_id: impl Into<String>,
        name: impl Into<String>,
        input: Value,
    ) -> SessionMessage {
        Self::new_tool_use_message_with_content(scope, call_id, None, name, input)
    }

    pub fn new_tool_use_message_with_content(
        scope: SessionScope,
        call_id: impl Into<String>,
        content: Option<String>,
        name: impl Into<String>,
        input: Value,
    ) -> SessionMessage {
        let call_id = call_id.into();
        SessionMessage::ToolCall {
            id: new_uuid_string(),
            scope,
            call_id: Some(call_id),
            content,
            name: name.into(),
            input,
        }
    }

    pub fn new_tool_result_message(
        call_id: impl Into<String>,
        output: impl Into<String>,
    ) -> SessionMessage {
        Self::new_tool_result_message_with_scope(SessionScope::main(), call_id, output)
    }

    pub fn new_tool_result_message_with_scope(
        scope: SessionScope,
        call_id: impl Into<String>,
        output: impl Into<String>,
    ) -> SessionMessage {
        let call_id = call_id.into();
        SessionMessage::ToolResult {
            id: new_uuid_string(),
            scope,
            call_id: Some(call_id),
            output: output.into(),
        }
    }

    pub fn new_tool_approval_request_message(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        command: impl Into<String>,
        rule_hits: Vec<SessionRuleHit>,
        options: Vec<String>,
    ) -> SessionMessage {
        Self::new_tool_approval_request_message_with_scope(
            SessionScope::main(),
            call_id,
            tool_name,
            command,
            rule_hits,
            options,
        )
    }

    pub fn new_tool_approval_request_message_with_scope(
        scope: SessionScope,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        command: impl Into<String>,
        rule_hits: Vec<SessionRuleHit>,
        options: Vec<String>,
    ) -> SessionMessage {
        SessionMessage::ToolApprovalRequest {
            id: new_uuid_string(),
            scope,
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            command: command.into(),
            rule_hits,
            options,
        }
    }

    pub fn new_tool_approval_decision_message(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        command: impl Into<String>,
        decision: impl Into<String>,
        rule_hits: Vec<SessionRuleHit>,
        approved: bool,
    ) -> SessionMessage {
        Self::new_tool_approval_decision_message_with_scope(
            SessionScope::main(),
            call_id,
            tool_name,
            command,
            decision,
            rule_hits,
            approved,
        )
    }

    pub fn new_tool_approval_decision_message_with_scope(
        scope: SessionScope,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        command: impl Into<String>,
        decision: impl Into<String>,
        rule_hits: Vec<SessionRuleHit>,
        approved: bool,
    ) -> SessionMessage {
        SessionMessage::ToolApprovalDecision {
            id: new_uuid_string(),
            scope,
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            command: command.into(),
            decision: decision.into(),
            rule_hits,
            approved,
        }
    }

    pub fn new_reasoning_message(content: impl Into<String>) -> SessionMessage {
        Self::new_reasoning_message_with_scope(SessionScope::main(), content)
    }

    pub fn new_reasoning_message_with_scope(
        scope: SessionScope,
        content: impl Into<String>,
    ) -> SessionMessage {
        SessionMessage::Reasoning {
            id: new_uuid_string(),
            scope,
            content: content.into(),
        }
    }

    fn append_entry(&self, session_id: &str, entry: SessionEntry) -> Result<()> {
        let mut writer = Self::open_append(&self.messages_path(session_id))?;
        let now = now_rfc3339();
        Self::write_line(&mut writer, &entry, &now)?;
        self.touch_session_updated_at(session_id, &now)?;
        Ok(())
    }

    fn validate_compact_request(
        &self,
        visible: &[SessionMessage],
        request: &CompactRequest,
    ) -> Result<()> {
        if request.old_item_ids.is_empty() {
            bail!("old_item_ids must be a non-empty array");
        }
        if request.new_items.is_empty() {
            bail!("new_items must be a non-empty array");
        }

        let visible_ids: HashSet<&str> = visible.iter().map(SessionMessage::id).collect();
        let mut seen = HashSet::new();
        for id in &request.old_item_ids {
            if id.trim().is_empty() {
                bail!("old_item_ids must not contain empty strings");
            }
            if !seen.insert(id.clone()) {
                bail!("duplicate old_item_ids entry: {id}");
            }
            if !visible_ids.contains(id.as_str()) {
                bail!("item id is not currently visible: {id}");
            }
        }

        for item in &request.new_items {
            validate_new_item(item)?;
        }

        Ok(())
    }

    fn load_session_lines(&self, session_id: &str) -> Result<Vec<SessionLine>> {
        let path = self.messages_path_for_read(session_id);
        let file = fs::File::open(&path)
            .with_context(|| format!("failed to open session file: {}", path.display()))?;
        let reader = BufReader::new(file);
        let mut lines = Vec::new();

        for (line_no, line) in reader.lines().enumerate() {
            let line = line.with_context(|| {
                format!(
                    "failed to read session file line {}: {}",
                    line_no + 1,
                    path.display()
                )
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let parsed: SessionLine = serde_json::from_str(&line).with_context(|| {
                format!(
                    "failed to parse session file line {}: {}",
                    line_no + 1,
                    path.display()
                )
            })?;
            lines.push(parsed);
        }

        Ok(lines)
    }

    fn read_session_meta(&self, session_id: &str) -> Result<SessionMeta> {
        let meta_path = self.meta_path(session_id);
        if meta_path.exists() {
            let text = fs::read_to_string(&meta_path)
                .with_context(|| format!("failed to read session meta: {}", meta_path.display()))?;
            let meta: SessionMeta = serde_json::from_str(&text).with_context(|| {
                format!("failed to parse session meta: {}", meta_path.display())
            })?;
            return Ok(normalize_session_meta(meta));
        }

        let lines = self.load_session_lines(session_id)?;
        let latest = lines.iter().rev().find_map(|line| match &line.entry {
            SessionEntry::SessionMeta(meta) => Some(meta.clone()),
            _ => None,
        });
        match latest {
            Some(meta) => Ok(normalize_session_meta(meta)),
            None => bail!("session file missing leading session_meta: {session_id}"),
        }
    }

    fn ensure_session_exists(&self, session_id: &str) -> Result<()> {
        if !self.messages_path(session_id).exists()
            && !self.legacy_session_path(session_id).exists()
        {
            bail!("session not found: {session_id}");
        }
        Ok(())
    }

    fn messages_path(&self, session_id: &str) -> PathBuf {
        self.session_dir_path(session_id).join(MESSAGES_FILE_NAME)
    }

    fn meta_path(&self, session_id: &str) -> PathBuf {
        self.session_dir_path(session_id).join(META_FILE_NAME)
    }

    fn legacy_session_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{session_id}.jsonl"))
    }

    fn messages_path_for_read(&self, session_id: &str) -> PathBuf {
        let path = self.messages_path(session_id);
        if path.exists() {
            path
        } else {
            self.legacy_session_path(session_id)
        }
    }

    fn write_session_meta(&self, meta: &SessionMeta) -> Result<()> {
        fs::create_dir_all(self.session_dir_path(&meta.id))
            .with_context(|| format!("failed to create session directory for {}", meta.id))?;
        let path = self.meta_path(&meta.id);
        let serialized =
            serde_json::to_string_pretty(meta).context("failed to serialize session meta")?;
        fs::write(&path, serialized.as_bytes())
            .with_context(|| format!("failed to write session meta: {}", path.display()))
    }

    fn open_append(path: &Path) -> Result<fs::File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open append writer: {}", path.display()))
    }

    fn write_line(writer: &mut fs::File, entry: &SessionEntry, timestamp: &str) -> Result<()> {
        let line = SessionLine {
            timestamp: timestamp.to_string(),
            entry: entry.clone(),
        };
        let serialized =
            serde_json::to_string(&line).context("failed to serialize session line")?;
        writer
            .write_all(serialized.as_bytes())
            .context("failed to write session line")?;
        writer
            .write_all(b"\n")
            .context("failed to write session newline")?;
        Ok(())
    }

    fn touch_session_updated_at(&self, session_id: &str, updated_at: &str) -> Result<()> {
        let meta_path = self.meta_path(session_id);
        if !meta_path.exists() {
            return Ok(());
        }
        let mut meta = self.read_session_meta(session_id)?;
        meta.updated_at = updated_at.to_string();
        self.write_session_meta(&meta)
    }
}

impl SessionMessage {
    pub fn scope(&self) -> &SessionScope {
        match self {
            SessionMessage::Message { scope, .. }
            | SessionMessage::ToolCall { scope, .. }
            | SessionMessage::ToolResult { scope, .. }
            | SessionMessage::ToolApprovalRequest { scope, .. }
            | SessionMessage::ToolApprovalDecision { scope, .. }
            | SessionMessage::Reasoning { scope, .. } => scope,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            SessionMessage::Message { id, .. }
            | SessionMessage::ToolCall { id, .. }
            | SessionMessage::ToolResult { id, .. }
            | SessionMessage::ToolApprovalRequest { id, .. }
            | SessionMessage::ToolApprovalDecision { id, .. }
            | SessionMessage::Reasoning { id, .. } => id,
        }
    }

    pub fn text_preview(&self) -> Option<String> {
        match self {
            SessionMessage::Message { content, .. } => Some(join_content_text(content)),
            SessionMessage::ToolCall { .. }
            | SessionMessage::ToolResult { .. }
            | SessionMessage::ToolApprovalRequest { .. }
            | SessionMessage::ToolApprovalDecision { .. }
            | SessionMessage::Reasoning { .. } => None,
        }
    }
}

fn replay_visible_history(lines: &[SessionLine]) -> Result<Vec<SessionMessage>> {
    let mut visible = Vec::new();
    for line in lines {
        match &line.entry {
            SessionEntry::SessionMeta(_) => {}
            SessionEntry::ResponseItem(item) => {
                if item.scope().is_main() {
                    visible.push(item.clone());
                }
            }
            SessionEntry::ProcessEvent(event) => {
                visible.push(process_event_to_message(event));
            }
            SessionEntry::UserTurn(_) | SessionEntry::FileSnapshot(_) => {}
            SessionEntry::Compacted(compacted) => {
                visible = compacted.replacement_history.clone();
            }
            SessionEntry::Rewound(rewound) => {
                visible = rewound.replacement_history.clone();
            }
        }
    }
    Ok(visible)
}

fn replay_scope_history(
    lines: &[SessionLine],
    scope: &SessionScope,
) -> Result<Vec<SessionMessage>> {
    let mut messages = Vec::new();
    for line in lines {
        match &line.entry {
            SessionEntry::SessionMeta(_) => {}
            SessionEntry::ResponseItem(item) => {
                if item.scope() == scope {
                    messages.push(item.clone());
                }
            }
            SessionEntry::ProcessEvent(event) => {
                if scope.is_main() {
                    messages.push(process_event_to_message(event));
                }
            }
            SessionEntry::UserTurn(_)
            | SessionEntry::FileSnapshot(_)
            | SessionEntry::Compacted(_)
            | SessionEntry::Rewound(_) => {}
        }
    }
    Ok(messages)
}

fn process_event_to_message(event: &ProcessEventPayload) -> SessionMessage {
    let mut lines = vec![
        "[PROCESS EVENT]".to_string(),
        format!("process_id: {}", event.process_id),
        format!("event: {}", event.event),
    ];
    if let Some(exit_code) = event.exit_code {
        lines.push(format!("exit_code: {exit_code}"));
    }
    if let Some(output_tail) = event.output_tail.as_ref().filter(|value| !value.is_empty()) {
        lines.push(format!("output_tail: {output_tail}"));
    }
    lines.push(format!("on_event: {}", event.on_event));

    SessionMessage::Message {
        id: event.id.clone(),
        scope: SessionScope::main(),
        role: SessionRole::User,
        content: vec![ContentItem::InputText {
            text: lines.join("\n"),
        }],
    }
}

enum FileDigest {
    Missing,
    Directory,
    File { sha256: String, bytes: Vec<u8> },
}

fn current_file_digest(path: &Path) -> Result<FileDigest> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(FileDigest::Missing),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to stat file state: {}", path.display()));
        }
    };
    if metadata.is_dir() {
        return Ok(FileDigest::Directory);
    }
    let bytes =
        fs::read(path).with_context(|| format!("failed to read file state: {}", path.display()))?;
    Ok(FileDigest::File {
        sha256: sha256_hex(&bytes),
        bytes,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn build_rewind_items(lines: &[SessionLine], visible: &[SessionMessage]) -> Vec<RewindListItem> {
    let raw_by_message_id = raw_user_text_by_message_id(lines);
    let mut items = Vec::new();
    for item in visible {
        let SessionMessage::Message {
            id,
            scope,
            role,
            content,
        } = item
        else {
            continue;
        };
        if !scope.is_main() || role != &SessionRole::User {
            continue;
        }
        let text = join_content_text(content);
        if text.trim_start().starts_with("[PROCESS EVENT]") {
            continue;
        }
        let raw = raw_by_message_id
            .get(id)
            .cloned()
            .unwrap_or_else(|| extract_visible_user_text(&text));
        items.push(RewindListItem {
            index: items.len() + 1,
            message_id: id.clone(),
            preview: compact_rewind_preview(&raw, 80),
        });
    }
    items
}

fn raw_user_text_by_message_id(lines: &[SessionLine]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in lines {
        if let SessionEntry::UserTurn(payload) = &line.entry {
            map.insert(payload.message_id.clone(), payload.raw_text.clone());
        }
    }
    map
}

fn extract_visible_user_text(text: &str) -> String {
    for marker in ["\n[USER MESSAGE]\n", "[USER MESSAGE]\n"] {
        if let Some(index) = text.find(marker) {
            return text[index + marker.len()..].trim().to_string();
        }
    }
    if let Some(index) = text.find("\ntext:\n") {
        let rest = &text[index + "\ntext:\n".len()..];
        let end = rest.find("\n\n[User Attachment]").unwrap_or(rest.len());
        return rest[..end].trim().to_string();
    }
    if let Some(index) = text.rfind("\nmessage:\n") {
        return text[index + "\nmessage:\n".len()..].trim().to_string();
    }
    text.trim().to_string()
}

fn compact_rewind_preview(text: &str, max_chars: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let normalized = if one_line.is_empty() {
        "(empty user message)".to_string()
    } else {
        one_line
    };
    let mut out = normalized.chars().take(max_chars).collect::<String>();
    if normalized.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

fn line_index_for_message(lines: &[SessionLine], message_id: &str) -> Option<usize> {
    lines.iter().position(|line| {
        matches!(
            &line.entry,
            SessionEntry::ResponseItem(item) if item.id() == message_id
        )
    })
}

fn restore_file_snapshots_after(
    session_id: &str,
    manager: &SessionManager,
    lines: &[SessionLine],
    line_index: usize,
    warnings: &mut Vec<String>,
) -> Vec<RewindRestoredFile> {
    let snapshots = lines
        .iter()
        .skip(line_index + 1)
        .filter_map(|line| match &line.entry {
            SessionEntry::FileSnapshot(snapshot) => Some(snapshot.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut restored = Vec::new();
    for snapshot in snapshots.iter().rev() {
        if let Some(item) = restore_file_snapshot(session_id, manager, snapshot, warnings) {
            restored.push(item);
        }
    }
    restored
}

fn restore_file_snapshot(
    session_id: &str,
    manager: &SessionManager,
    snapshot: &FileSnapshotPayload,
    warnings: &mut Vec<String>,
) -> Option<RewindRestoredFile> {
    let path = PathBuf::from(&snapshot.path);
    let current = match current_file_digest(&path) {
        Ok(current) => current,
        Err(error) => {
            warnings.push(format!(
                "Skipped {}: failed to inspect current file state: {error:#}",
                snapshot.path
            ));
            return None;
        }
    };
    if !file_digest_matches_after(&current, snapshot) {
        warnings.push(format!(
            "Skipped {}: current file state {} does not match recorded after state {}.",
            snapshot.path,
            describe_file_digest(&current),
            describe_after_state(snapshot)
        ));
        return None;
    }

    if snapshot.existed_before {
        let Some(backup_path) = snapshot.backup_path.as_deref() else {
            warnings.push(format!(
                "Skipped {}: missing rewind backup path for existing-file restore.",
                snapshot.path
            ));
            return None;
        };
        let backup = match resolve_backup_path(manager, session_id, backup_path) {
            Ok(backup) => backup,
            Err(error) => {
                warnings.push(format!(
                    "Skipped {}: invalid rewind backup path `{backup_path}`: {error:#}",
                    snapshot.path
                ));
                return None;
            }
        };
        let bytes = match fs::read(&backup) {
            Ok(bytes) => bytes,
            Err(error) => {
                warnings.push(format!(
                    "Skipped {}: failed to read rewind backup {}: {error}",
                    snapshot.path,
                    backup.display()
                ));
                return None;
            }
        };
        let backup_sha256 = sha256_hex(&bytes);
        if snapshot.sha256_before.as_deref() != Some(backup_sha256.as_str()) {
            warnings.push(format!(
                "Skipped {}: rewind backup sha256 mismatch.",
                snapshot.path
            ));
            return None;
        }
        if let Some(parent) = path.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                warnings.push(format!(
                    "Skipped {}: failed to create parent directory {}: {error}",
                    snapshot.path,
                    parent.display()
                ));
                return None;
            }
        }
        if let Err(error) = fs::write(&path, &bytes) {
            warnings.push(format!(
                "Skipped {}: failed to restore file: {error}",
                snapshot.path
            ));
            return None;
        }
        return Some(RewindRestoredFile {
            path: snapshot.path.clone(),
            action: "restored".to_string(),
            sha256_before: snapshot.sha256_before.clone(),
            sha256_after: snapshot.sha256_after.clone(),
        });
    }

    if path.exists() {
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                warnings.push(format!(
                    "Skipped {}: expected file but current path is a directory.",
                    snapshot.path
                ));
                return None;
            }
            Ok(_) => {
                if let Err(error) = fs::remove_file(&path) {
                    warnings.push(format!(
                        "Skipped {}: failed to delete newly-created file: {error}",
                        snapshot.path
                    ));
                    return None;
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                warnings.push(format!(
                    "Skipped {}: failed to stat newly-created file: {error}",
                    snapshot.path
                ));
                return None;
            }
        }
    }
    Some(RewindRestoredFile {
        path: snapshot.path.clone(),
        action: "deleted".to_string(),
        sha256_before: snapshot.sha256_before.clone(),
        sha256_after: snapshot.sha256_after.clone(),
    })
}

fn file_digest_matches_after(current: &FileDigest, snapshot: &FileSnapshotPayload) -> bool {
    match current {
        FileDigest::Missing => !snapshot.existed_after && snapshot.sha256_after.is_none(),
        FileDigest::Directory => false,
        FileDigest::File { sha256, .. } => {
            snapshot.existed_after && snapshot.sha256_after.as_deref() == Some(sha256.as_str())
        }
    }
}

fn describe_file_digest(current: &FileDigest) -> String {
    match current {
        FileDigest::Missing => "missing".to_string(),
        FileDigest::Directory => "directory".to_string(),
        FileDigest::File { sha256, .. } => format!("file sha256={sha256}"),
    }
}

fn describe_after_state(snapshot: &FileSnapshotPayload) -> String {
    if snapshot.existed_after {
        format!(
            "file sha256={}",
            snapshot.sha256_after.as_deref().unwrap_or("<missing>")
        )
    } else {
        "missing".to_string()
    }
}

fn resolve_backup_path(
    manager: &SessionManager,
    session_id: &str,
    backup_path: &str,
) -> Result<PathBuf> {
    let relative = Path::new(backup_path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!("backup path must be a safe session-relative path");
    }
    Ok(manager.session_dir_path(session_id).join(relative))
}

fn model_message_to_session_messages(message: &Message) -> Result<Vec<SessionMessage>> {
    match message {
        Message::System(system) => Ok(vec![SessionManager::new_text_message(
            SessionRole::System,
            system.content.clone(),
        )]),
        Message::Developer(text) => Ok(vec![SessionManager::new_text_message(
            SessionRole::System,
            text.clone(),
        )]),
        Message::User(user) => Ok(vec![SessionManager::new_text_message(
            SessionRole::User,
            user.content.clone(),
        )]),
        Message::Assistant(assistant) => match &assistant.content {
            LanguageModelResponseContentType::Text(text) => {
                Ok(vec![SessionManager::new_text_message(
                    SessionRole::Assistant,
                    text.clone(),
                )])
            }
            LanguageModelResponseContentType::Reasoning { content } => {
                Ok(vec![SessionManager::new_reasoning_message(content.clone())])
            }
            LanguageModelResponseContentType::ToolCall(tool_call) => {
                Ok(vec![SessionManager::new_tool_use_message_with_content(
                    SessionScope::main(),
                    tool_call.tool.id.clone(),
                    tool_call.content.clone(),
                    tool_call.tool.name.clone(),
                    tool_call.input.clone(),
                )])
            }
        },
        Message::Tool(tool_result) => {
            let output = match &tool_result.output {
                Ok(Value::String(text)) => text.clone(),
                Ok(value) => serde_json::to_string(value)
                    .context("failed to serialize model tool result output")?,
                Err(err) => format!("Tool error: {err}"),
            };
            Ok(vec![SessionManager::new_tool_result_message(
                tool_result.tool.id.clone(),
                output,
            )])
        }
    }
}

fn materialize_new_item(item: CompactNewItem) -> SessionMessage {
    match item {
        CompactNewItem::Message { role, content } => SessionMessage::Message {
            id: new_uuid_string(),
            scope: SessionScope::main(),
            role,
            content,
        },
        CompactNewItem::ToolCall {
            call_id,
            name,
            input,
        } => SessionMessage::ToolCall {
            id: new_uuid_string(),
            scope: SessionScope::main(),
            call_id,
            content: None,
            name,
            input,
        },
        CompactNewItem::ToolResult { call_id, output } => SessionMessage::ToolResult {
            id: new_uuid_string(),
            scope: SessionScope::main(),
            call_id,
            output,
        },
    }
}

fn normalize_session_meta(mut meta: SessionMeta) -> SessionMeta {
    if meta.root_id.trim().is_empty() {
        meta.root_id = meta.id.clone();
    }
    if meta.created_by.trim().is_empty() {
        meta.created_by = default_created_by();
    }
    if meta.goal_tokens_used < 0 {
        meta.goal_tokens_used = 0;
    }
    if meta.goal_time_used_seconds < 0 {
        meta.goal_time_used_seconds = 0;
    }
    meta
}

fn session_goal_from_meta(meta: &SessionMeta) -> Option<SessionGoal> {
    let objective = meta.goal.as_deref()?.trim();
    if objective.is_empty() {
        return None;
    }
    let status = meta
        .status
        .as_deref()
        .and_then(GoalStatus::parse)
        .unwrap_or(GoalStatus::Active);
    Some(SessionGoal {
        session_id: meta.id.clone(),
        objective: objective.to_string(),
        status,
        token_budget: meta.goal_token_budget,
        tokens_used: meta.goal_tokens_used.max(0),
        time_used_seconds: meta.goal_time_used_seconds.max(0),
        created_at: meta
            .goal_created_at
            .clone()
            .unwrap_or_else(|| meta.created_at.clone()),
        updated_at: meta.updated_at.clone(),
    })
}

fn validate_goal_objective(objective: &str) -> Result<()> {
    if objective.trim().is_empty() {
        bail!("goal objective must be a non-empty string");
    }
    const MAX_GOAL_OBJECTIVE_CHARS: usize = 20_000;
    let actual = objective.chars().count();
    if actual > MAX_GOAL_OBJECTIVE_CHARS {
        bail!(
            "goal objective is too long: {actual} characters. Limit: {MAX_GOAL_OBJECTIVE_CHARS} characters"
        );
    }
    Ok(())
}

fn validate_goal_budget(token_budget: Option<i64>) -> Result<()> {
    if let Some(token_budget) = token_budget
        && token_budget <= 0
    {
        bail!("goal token budget must be positive when provided");
    }
    Ok(())
}

fn default_created_by() -> String {
    "user".to_string()
}

fn validate_new_item(item: &CompactNewItem) -> Result<()> {
    match item {
        CompactNewItem::Message { content, .. } => {
            if content.is_empty() {
                bail!("message content must be a non-empty array");
            }
            for content_item in content {
                match content_item {
                    ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                        if text.trim().is_empty() {
                            bail!("message text must be a non-empty string");
                        }
                    }
                }
            }
        }
        CompactNewItem::ToolCall { name, .. } => {
            if name.trim().is_empty() {
                bail!("tool_call.name must be a non-empty string");
            }
        }
        CompactNewItem::ToolResult { output, .. } => {
            if output.trim().is_empty() {
                bail!("tool_result.output must be a non-empty string");
            }
        }
    }
    Ok(())
}

fn build_replacement_history(
    visible: &[SessionMessage],
    old_item_ids: &[String],
    new_items: &[SessionMessage],
) -> Result<Vec<SessionMessage>> {
    let old_id_set: HashSet<&str> = old_item_ids.iter().map(String::as_str).collect();
    let mut last_old_pos = None;

    for (idx, item) in visible.iter().enumerate() {
        if old_id_set.contains(item.id()) {
            last_old_pos = Some(idx);
        }
    }

    let last_old_pos = last_old_pos.context("failed to locate insertion position for compact")?;
    let mut replacement = Vec::with_capacity(visible.len() - old_item_ids.len() + new_items.len());

    for (idx, item) in visible.iter().enumerate() {
        if old_id_set.contains(item.id()) {
            if idx == last_old_pos {
                replacement.extend(new_items.iter().cloned());
            }
            continue;
        }
        replacement.push(item.clone());
    }

    Ok(replacement)
}

fn project_to_model_messages(visible: &[SessionMessage]) -> Result<Messages> {
    let merged = merge_adjacent_messages(visible);
    let mut messages = Messages::new();
    let mut known_tool_calls = HashMap::<String, String>::new();
    let mut pending_tool_calls = VecDeque::<String>::new();

    for item in merged {
        match item {
            SessionMessage::Message { role, content, .. } => {
                let text = join_content_text(&content);
                if text.is_empty() {
                    continue;
                }
                match role {
                    SessionRole::System => messages.push(Message::System(text.into())),
                    SessionRole::User => messages.push(Message::User(text.into())),
                    SessionRole::Assistant => messages.push(Message::Assistant(text.into())),
                }
            }
            SessionMessage::Reasoning { content, .. } => {
                if content.trim().is_empty() {
                    continue;
                }
                messages.push(Message::Assistant(AssistantMessage::new(
                    LanguageModelResponseContentType::Reasoning { content },
                    None,
                )));
            }
            SessionMessage::ToolCall {
                id,
                call_id,
                content,
                name,
                input,
                ..
            } => {
                let resolved_call_id = call_id
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| format!("call_{id}"));
                let mut tool_call = ToolCallInfo::new(name.clone());
                tool_call.id(resolved_call_id.clone());
                tool_call.input(input);
                if let Some(content) = content.filter(|value| !value.trim().is_empty()) {
                    tool_call.content(content);
                }
                messages.push(Message::Assistant(AssistantMessage::new(
                    LanguageModelResponseContentType::ToolCall(tool_call),
                    None,
                )));
                known_tool_calls.insert(resolved_call_id.clone(), name);
                pending_tool_calls.push_back(resolved_call_id);
            }
            SessionMessage::ToolResult {
                call_id, output, ..
            } => {
                let resolved_call_id = call_id
                    .filter(|value| !value.trim().is_empty())
                    .or_else(|| pending_tool_calls.front().cloned());
                let Some(resolved_call_id) = resolved_call_id else {
                    continue;
                };
                let Some(tool_name) = known_tool_calls.get(&resolved_call_id).cloned() else {
                    continue;
                };
                if pending_tool_calls.front() == Some(&resolved_call_id) {
                    pending_tool_calls.pop_front();
                }
                let mut tool_result = ToolResultInfo::new(tool_name);
                tool_result.id(resolved_call_id);
                tool_result.output(Value::String(output));
                messages.push(Message::Tool(tool_result));
            }
            SessionMessage::ToolApprovalRequest { .. }
            | SessionMessage::ToolApprovalDecision { .. } => {}
        }
    }

    Ok(messages)
}

fn merge_adjacent_messages(items: &[SessionMessage]) -> Vec<SessionMessage> {
    let mut merged = Vec::new();

    for item in items {
        match (merged.last_mut(), item) {
            (
                Some(SessionMessage::Message {
                    role: last_role,
                    content: last_content,
                    ..
                }),
                SessionMessage::Message { role, content, .. },
            ) if last_role == role => {
                last_content.extend(content.clone());
            }
            _ => merged.push(item.clone()),
        }
    }

    merged
}

fn join_content_text(content: &[ContentItem]) -> String {
    let mut parts = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                parts.push(text.clone())
            }
        }
    }
    parts.join("\n\n")
}

fn new_uuid_string() -> String {
    Uuid::now_v7().to_string()
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_manager() -> Result<SessionManager> {
        let dir = tempdir().context("failed to create tempdir")?;
        SessionManager::new(dir.keep())
    }

    #[test]
    fn create_session_writes_meta_and_system_message() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        let visible = manager.get_all_messages(&session_id)?;
        let meta = manager.get_session_meta(&session_id)?;

        assert_eq!(visible.len(), 1);
        assert_eq!(meta.kind, SessionKind::Main);
        assert_eq!(meta.parent_id, None);
        assert_eq!(meta.root_id, session_id);
        assert_eq!(meta.created_by, "user");
        assert_eq!(
            visible[0],
            SessionMessage::Message {
                id: visible[0].id().to_string(),
                scope: SessionScope::main(),
                role: SessionRole::System,
                content: vec![ContentItem::OutputText {
                    text: "system prompt".to_string()
                }],
            }
        );
        Ok(())
    }

    #[test]
    fn goal_lifecycle_updates_session_meta() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;

        assert!(manager.get_goal(&session_id)?.is_none());

        let goal = manager.set_goal(&session_id, "ship goal support", Some(100))?;
        assert_eq!(goal.objective, "ship goal support");
        assert_eq!(goal.status, GoalStatus::Active);
        assert_eq!(goal.token_budget, Some(100));
        assert_eq!(goal.tokens_used, 0);

        let goal = manager.add_goal_usage(&session_id, 40, 3)?.expect("goal");
        assert_eq!(goal.tokens_used, 40);
        assert_eq!(goal.time_used_seconds, 3);
        assert_eq!(goal.status, GoalStatus::Active);

        let paused = manager.update_goal_status(&session_id, GoalStatus::Paused)?;
        assert_eq!(paused.status, GoalStatus::Paused);

        let resumed = manager.update_goal_status(&session_id, GoalStatus::Active)?;
        assert_eq!(resumed.status, GoalStatus::Active);

        let limited = manager.add_goal_usage(&session_id, 60, 2)?.expect("goal");
        assert_eq!(limited.status, GoalStatus::BudgetLimited);
        assert_eq!(limited.tokens_used, 100);

        assert!(manager.clear_goal(&session_id)?);
        assert!(manager.get_goal(&session_id)?.is_none());
        assert!(!manager.clear_goal(&session_id)?);
        Ok(())
    }

    #[test]
    fn fork_agent_session_uses_session_graph_meta_and_forked_messages() -> Result<()> {
        let manager = test_manager()?;
        let parent_id = manager.create_session(Some("parent"), "system prompt")?;
        let fork_messages = manager.to_model_messages(&parent_id)?;

        let agent_id = manager.fork_agent_session_from_model_messages(
            &parent_id,
            Some("Memory Review"),
            "review memory changes",
            SessionRuntimeConfig {
                provider: Some("test-provider".to_string()),
                ..SessionRuntimeConfig::default()
            },
            &fork_messages,
            "memory_review",
        )?;

        let meta = manager.get_session_meta(&agent_id)?;
        assert_eq!(meta.kind, SessionKind::Agent);
        assert_eq!(meta.parent_id.as_deref(), Some(parent_id.as_str()));
        assert_eq!(meta.root_id.as_str(), parent_id.as_str());
        assert_eq!(meta.created_by, "memory_review");
        assert_eq!(meta.goal.as_deref(), Some("review memory changes"));
        assert_eq!(meta.status.as_deref(), Some("running"));
        assert_eq!(meta.runtime.provider.as_deref(), Some("test-provider"));

        let child_messages = manager.get_all_messages(&agent_id)?;
        assert_eq!(child_messages.len(), 1);
        assert_eq!(manager.get_all_messages(&meta.root_id)?.len(), 1);
        Ok(())
    }

    #[test]
    fn compact_scattered_items_inserts_new_items_at_last_old_position() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        let msg1 = manager.append_text_message(&session_id, SessionRole::User, "one")?;
        let msg2 = manager.append_text_message(&session_id, SessionRole::Assistant, "two")?;
        let msg3 = manager.append_text_message(&session_id, SessionRole::User, "three")?;
        let msg4 = manager.append_text_message(&session_id, SessionRole::Assistant, "four")?;

        manager.compact_session(
            &session_id,
            CompactRequest {
                old_item_ids: vec![msg1.id().to_string(), msg3.id().to_string()],
                new_items: vec![CompactNewItem::Message {
                    role: SessionRole::Assistant,
                    content: vec![ContentItem::OutputText {
                        text: "summary".to_string(),
                    }],
                }],
            },
        )?;

        let visible = manager.get_all_messages(&session_id)?;
        let texts = visible
            .iter()
            .filter_map(SessionMessage::text_preview)
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec![
                "system prompt".to_string(),
                "two".to_string(),
                "summary".to_string(),
                "four".to_string()
            ]
        );
        assert_eq!(visible[1].id(), msg2.id());
        assert_eq!(visible[3].id(), msg4.id());
        Ok(())
    }

    #[test]
    fn compact_persists_replacement_history_for_restore() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        let msg1 = manager.append_text_message(&session_id, SessionRole::User, "one")?;
        let msg2 = manager.append_text_message(&session_id, SessionRole::Assistant, "two")?;

        let compacted = manager.compact_session(
            &session_id,
            CompactRequest {
                old_item_ids: vec![msg1.id().to_string(), msg2.id().to_string()],
                new_items: vec![CompactNewItem::Message {
                    role: SessionRole::Assistant,
                    content: vec![ContentItem::OutputText {
                        text: "summary".to_string(),
                    }],
                }],
            },
        )?;

        let lines = manager.load_session_lines(&session_id)?;
        let Some(SessionLine {
            entry: SessionEntry::Compacted(persisted),
            ..
        }) = lines.last()
        else {
            bail!("expected compacted line");
        };
        assert_eq!(persisted.id, compacted.id);
        assert_eq!(persisted.replacement_history, compacted.replacement_history);
        Ok(())
    }

    #[test]
    fn to_openai_messages_merges_adjacent_roles_and_drops_orphan_tool_results() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        manager.append_text_message(&session_id, SessionRole::User, "one")?;
        manager.append_text_message(&session_id, SessionRole::User, "two")?;
        manager.append_message(
            &session_id,
            &SessionMessage::ToolResult {
                id: new_uuid_string(),
                scope: SessionScope::main(),
                call_id: None,
                output: "orphan".to_string(),
            },
        )?;

        let messages = manager.to_openai_messages(&session_id)?;
        assert_eq!(messages.len(), 2);
        match &messages[1] {
            Message::User(user) => assert_eq!(user.content, "one\n\ntwo"),
            other => bail!("expected merged user message, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn compact_allows_missing_tool_call_ids_and_restore_pairs_latest_call() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        let base = manager.append_text_message(&session_id, SessionRole::User, "run shell")?;

        manager.compact_session(
            &session_id,
            CompactRequest {
                old_item_ids: vec![base.id().to_string()],
                new_items: vec![
                    CompactNewItem::ToolCall {
                        call_id: None,
                        name: "shell".to_string(),
                        input: serde_json::json!({ "command": "ls" }),
                    },
                    CompactNewItem::ToolResult {
                        call_id: None,
                        output: "a\nb".to_string(),
                    },
                ],
            },
        )?;

        let messages = manager.to_openai_messages(&session_id)?;
        assert_eq!(messages.len(), 3);
        assert!(matches!(messages[1], Message::Assistant(_)));
        assert!(matches!(messages[2], Message::Tool(_)));
        Ok(())
    }

    #[test]
    fn compact_rejects_empty_old_item_ids() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;

        let error = manager
            .compact_session(
                &session_id,
                CompactRequest {
                    old_item_ids: Vec::new(),
                    new_items: vec![CompactNewItem::Message {
                        role: SessionRole::Assistant,
                        content: vec![ContentItem::OutputText {
                            text: "summary".to_string(),
                        }],
                    }],
                },
            )
            .expect_err("expected compact validation failure");

        assert!(
            error
                .to_string()
                .contains("old_item_ids must be a non-empty array")
        );
        Ok(())
    }

    #[test]
    fn compact_rejects_empty_new_items() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        let base = manager.append_text_message(&session_id, SessionRole::User, "one")?;

        let error = manager
            .compact_session(
                &session_id,
                CompactRequest {
                    old_item_ids: vec![base.id().to_string()],
                    new_items: Vec::new(),
                },
            )
            .expect_err("expected compact validation failure");

        assert!(
            error
                .to_string()
                .contains("new_items must be a non-empty array")
        );
        Ok(())
    }

    #[test]
    fn compact_rejects_duplicate_old_item_ids() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        let base = manager.append_text_message(&session_id, SessionRole::User, "one")?;

        let error = manager
            .compact_session(
                &session_id,
                CompactRequest {
                    old_item_ids: vec![base.id().to_string(), base.id().to_string()],
                    new_items: vec![CompactNewItem::Message {
                        role: SessionRole::Assistant,
                        content: vec![ContentItem::OutputText {
                            text: "summary".to_string(),
                        }],
                    }],
                },
            )
            .expect_err("expected duplicate old id failure");

        assert!(error.to_string().contains("duplicate old_item_ids entry"));
        Ok(())
    }

    #[test]
    fn compact_rejects_unknown_old_item_ids() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;

        let error = manager
            .compact_session(
                &session_id,
                CompactRequest {
                    old_item_ids: vec!["missing".to_string()],
                    new_items: vec![CompactNewItem::Message {
                        role: SessionRole::Assistant,
                        content: vec![ContentItem::OutputText {
                            text: "summary".to_string(),
                        }],
                    }],
                },
            )
            .expect_err("expected missing id failure");

        assert!(
            error
                .to_string()
                .contains("item id is not currently visible")
        );
        Ok(())
    }

    #[test]
    fn compact_rejects_invalid_new_message_content() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        let base = manager.append_text_message(&session_id, SessionRole::User, "one")?;

        let error = manager
            .compact_session(
                &session_id,
                CompactRequest {
                    old_item_ids: vec![base.id().to_string()],
                    new_items: vec![CompactNewItem::Message {
                        role: SessionRole::Assistant,
                        content: vec![ContentItem::OutputText {
                            text: "   ".to_string(),
                        }],
                    }],
                },
            )
            .expect_err("expected invalid message content failure");

        assert!(
            error
                .to_string()
                .contains("message text must be a non-empty string")
        );
        Ok(())
    }

    #[test]
    fn compact_rejects_invalid_tool_call_name() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        let base = manager.append_text_message(&session_id, SessionRole::User, "one")?;

        let error = manager
            .compact_session(
                &session_id,
                CompactRequest {
                    old_item_ids: vec![base.id().to_string()],
                    new_items: vec![CompactNewItem::ToolCall {
                        call_id: None,
                        name: "   ".to_string(),
                        input: serde_json::json!({ "command": "ls" }),
                    }],
                },
            )
            .expect_err("expected invalid tool call failure");

        assert!(
            error
                .to_string()
                .contains("tool_call.name must be a non-empty string")
        );
        Ok(())
    }

    #[test]
    fn compact_rejects_invalid_tool_result_output() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        let base = manager.append_text_message(&session_id, SessionRole::User, "one")?;

        let error = manager
            .compact_session(
                &session_id,
                CompactRequest {
                    old_item_ids: vec![base.id().to_string()],
                    new_items: vec![CompactNewItem::ToolResult {
                        call_id: None,
                        output: "   ".to_string(),
                    }],
                },
            )
            .expect_err("expected invalid tool result failure");

        assert!(
            error
                .to_string()
                .contains("tool_result.output must be a non-empty string")
        );
        Ok(())
    }

    #[test]
    fn cli_compact_reports_json_parse_errors() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;

        let error = manager
            .handle_cli_compact(&session_id, "{bad json")
            .expect_err("expected CLI parse failure");

        assert!(
            error
                .to_string()
                .contains("failed to parse compact json payload")
        );
        Ok(())
    }

    #[test]
    fn to_openai_messages_merges_adjacent_assistant_messages() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        manager.append_text_message(&session_id, SessionRole::Assistant, "one")?;
        manager.append_text_message(&session_id, SessionRole::Assistant, "two")?;

        let messages = manager.to_openai_messages(&session_id)?;
        assert_eq!(messages.len(), 2);
        match &messages[1] {
            Message::Assistant(assistant) => match &assistant.content {
                LanguageModelResponseContentType::Text(text) => assert_eq!(text, "one\n\ntwo"),
                other => bail!("expected assistant text content, got {other:?}"),
            },
            other => bail!("expected assistant message, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn to_openai_messages_does_not_merge_across_tool_boundaries() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        manager.append_text_message(&session_id, SessionRole::Assistant, "before")?;
        manager.append_message(
            &session_id,
            &SessionManager::new_tool_use_message(
                "call-1",
                "shell",
                serde_json::json!({ "command": "ls" }),
            ),
        )?;
        manager.append_message(
            &session_id,
            &SessionManager::new_tool_result_message("call-1", "a\nb"),
        )?;
        manager.append_text_message(&session_id, SessionRole::Assistant, "after")?;

        let messages = manager.to_openai_messages(&session_id)?;
        assert_eq!(messages.len(), 5);
        assert!(matches!(messages[1], Message::Assistant(_)));
        assert!(matches!(messages[2], Message::Assistant(_)));
        assert!(matches!(messages[3], Message::Tool(_)));
        assert!(matches!(messages[4], Message::Assistant(_)));
        Ok(())
    }

    #[test]
    fn to_openai_messages_drops_tool_result_with_unknown_call_id() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        manager.append_message(
            &session_id,
            &SessionMessage::ToolResult {
                id: new_uuid_string(),
                scope: SessionScope::main(),
                call_id: Some("missing-call".to_string()),
                output: "orphan".to_string(),
            },
        )?;

        let messages = manager.to_openai_messages(&session_id)?;
        assert_eq!(messages.len(), 1);
        assert!(matches!(messages[0], Message::System(_)));
        Ok(())
    }

    #[test]
    fn to_openai_messages_keeps_tool_call_without_result() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        manager.append_message(
            &session_id,
            &SessionManager::new_tool_use_message(
                "call-1",
                "shell",
                serde_json::json!({ "command": "ls" }),
            ),
        )?;

        let messages = manager.to_openai_messages(&session_id)?;
        assert_eq!(messages.len(), 2);
        assert!(matches!(messages[1], Message::Assistant(_)));
        Ok(())
    }

    #[test]
    fn compacted_history_can_normalize_adjacent_roles_after_restore() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        let base = manager.append_text_message(&session_id, SessionRole::User, "seed")?;

        manager.compact_session(
            &session_id,
            CompactRequest {
                old_item_ids: vec![base.id().to_string()],
                new_items: vec![
                    CompactNewItem::Message {
                        role: SessionRole::User,
                        content: vec![ContentItem::InputText {
                            text: "one".to_string(),
                        }],
                    },
                    CompactNewItem::Message {
                        role: SessionRole::User,
                        content: vec![ContentItem::InputText {
                            text: "two".to_string(),
                        }],
                    },
                ],
            },
        )?;

        let messages = manager.to_openai_messages(&session_id)?;
        assert_eq!(messages.len(), 2);
        match &messages[1] {
            Message::User(user) => assert_eq!(user.content, "one\n\ntwo"),
            other => bail!("expected merged user message, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn multiple_compacts_can_chain_on_previous_compacted_history() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        let msg1 = manager.append_text_message(&session_id, SessionRole::User, "u1")?;
        let msg2 = manager.append_text_message(&session_id, SessionRole::Assistant, "a1")?;
        let msg3 = manager.append_text_message(&session_id, SessionRole::User, "u2")?;

        manager.compact_session(
            &session_id,
            CompactRequest {
                old_item_ids: vec![msg1.id().to_string(), msg2.id().to_string()],
                new_items: vec![CompactNewItem::Message {
                    role: SessionRole::Assistant,
                    content: vec![ContentItem::OutputText {
                        text: "summary-1".to_string(),
                    }],
                }],
            },
        )?;

        let first_visible = manager.get_all_messages(&session_id)?;
        assert_eq!(
            first_visible
                .iter()
                .filter_map(SessionMessage::text_preview)
                .collect::<Vec<_>>(),
            vec![
                "system prompt".to_string(),
                "summary-1".to_string(),
                "u2".to_string()
            ]
        );

        let compact_target_id = first_visible[1].id().to_string();
        manager.compact_session(
            &session_id,
            CompactRequest {
                old_item_ids: vec![compact_target_id, msg3.id().to_string()],
                new_items: vec![CompactNewItem::Message {
                    role: SessionRole::Assistant,
                    content: vec![ContentItem::OutputText {
                        text: "summary-2".to_string(),
                    }],
                }],
            },
        )?;

        let second_visible = manager.get_all_messages(&session_id)?;
        assert_eq!(
            second_visible
                .iter()
                .filter_map(SessionMessage::text_preview)
                .collect::<Vec<_>>(),
            vec!["system prompt".to_string(), "summary-2".to_string()]
        );
        Ok(())
    }

    #[test]
    fn multiple_compacts_preserve_new_messages_between_rounds() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        let msg1 = manager.append_text_message(&session_id, SessionRole::User, "first")?;
        let msg2 = manager.append_text_message(&session_id, SessionRole::Assistant, "reply")?;

        manager.compact_session(
            &session_id,
            CompactRequest {
                old_item_ids: vec![msg1.id().to_string(), msg2.id().to_string()],
                new_items: vec![CompactNewItem::Message {
                    role: SessionRole::Assistant,
                    content: vec![ContentItem::OutputText {
                        text: "summary-1".to_string(),
                    }],
                }],
            },
        )?;

        let msg3 = manager.append_text_message(&session_id, SessionRole::User, "followup")?;
        let msg4 = manager.append_text_message(&session_id, SessionRole::Assistant, "answer")?;

        let visible = manager.get_all_messages(&session_id)?;
        let compacted_id = visible[1].id().to_string();
        manager.compact_session(
            &session_id,
            CompactRequest {
                old_item_ids: vec![compacted_id, msg4.id().to_string()],
                new_items: vec![CompactNewItem::Message {
                    role: SessionRole::Assistant,
                    content: vec![ContentItem::OutputText {
                        text: "summary-2".to_string(),
                    }],
                }],
            },
        )?;

        let final_visible = manager.get_all_messages(&session_id)?;
        assert_eq!(
            final_visible
                .iter()
                .filter_map(SessionMessage::text_preview)
                .collect::<Vec<_>>(),
            vec![
                "system prompt".to_string(),
                "followup".to_string(),
                "summary-2".to_string()
            ]
        );
        assert_eq!(final_visible[1].id(), msg3.id());
        Ok(())
    }

    #[test]
    fn multiple_compacts_with_tools_restore_to_valid_model_messages() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        let msg1 = manager.append_text_message(&session_id, SessionRole::User, "inspect")?;
        let tool_call = SessionManager::new_tool_use_message(
            "call-1",
            "shell",
            serde_json::json!({ "command": "ls" }),
        );
        let tool_result = SessionManager::new_tool_result_message("call-1", "a\nb");
        manager.append_message(&session_id, &tool_call)?;
        manager.append_message(&session_id, &tool_result)?;
        let msg2 = manager.append_text_message(&session_id, SessionRole::Assistant, "done")?;

        manager.compact_session(
            &session_id,
            CompactRequest {
                old_item_ids: vec![
                    msg1.id().to_string(),
                    tool_call.id().to_string(),
                    tool_result.id().to_string(),
                    msg2.id().to_string(),
                ],
                new_items: vec![
                    CompactNewItem::ToolCall {
                        call_id: None,
                        name: "shell".to_string(),
                        input: serde_json::json!({ "command": "pwd" }),
                    },
                    CompactNewItem::ToolResult {
                        call_id: None,
                        output: "/tmp/demo".to_string(),
                    },
                    CompactNewItem::Message {
                        role: SessionRole::Assistant,
                        content: vec![ContentItem::OutputText {
                            text: "summary-1".to_string(),
                        }],
                    },
                ],
            },
        )?;

        let follow = manager.append_text_message(&session_id, SessionRole::User, "next")?;
        let visible_after_first = manager.get_all_messages(&session_id)?;
        let tool_call_id = visible_after_first[1].id().to_string();
        let tool_result_id = visible_after_first[2].id().to_string();

        manager.compact_session(
            &session_id,
            CompactRequest {
                old_item_ids: vec![tool_call_id, tool_result_id, follow.id().to_string()],
                new_items: vec![CompactNewItem::Message {
                    role: SessionRole::Assistant,
                    content: vec![ContentItem::OutputText {
                        text: "summary-2".to_string(),
                    }],
                }],
            },
        )?;

        let messages = manager.to_openai_messages(&session_id)?;
        assert_eq!(messages.len(), 2);
        assert!(matches!(messages[0], Message::System(_)));
        assert!(matches!(messages[1], Message::Assistant(_)));
        Ok(())
    }

    #[test]
    fn compact_cli_can_run_multiple_rounds_and_read_back_replaced_history() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        let msg1 = manager.append_text_message(&session_id, SessionRole::User, "one")?;
        let msg2 = manager.append_text_message(&session_id, SessionRole::Assistant, "two")?;

        let first_payload = serde_json::json!({
            "old_item_ids": [msg1.id(), msg2.id()],
            "new_items": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        { "type": "output_text", "text": "summary-1" }
                    ]
                }
            ]
        });
        manager.handle_cli_compact(&session_id, &first_payload.to_string())?;

        let visible = manager.get_all_messages(&session_id)?;
        let compacted_id = visible[1].id().to_string();
        let second_payload = serde_json::json!({
            "old_item_ids": [compacted_id],
            "new_items": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        { "type": "output_text", "text": "summary-2" }
                    ]
                }
            ]
        });
        manager.handle_cli_compact(&session_id, &second_payload.to_string())?;

        let output = manager.handle_cli_get_all_messages(&session_id)?;
        assert!(output.contains("summary-2"));
        assert!(!output.contains("summary-1"));
        Ok(())
    }

    #[test]
    fn update_title_updates_session_meta_file() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(None, "system prompt")?;
        manager.update_title(&session_id, "Generated session title")?;

        let meta = manager.read_session_meta(&session_id)?;
        assert_eq!(meta.title, "Generated session title");
        assert!(
            manager
                .session_dir_path(&session_id)
                .join(META_FILE_NAME)
                .exists()
        );
        assert!(
            manager
                .session_dir_path(&session_id)
                .join(MESSAGES_FILE_NAME)
                .exists()
        );
        Ok(())
    }

    #[test]
    fn update_title_rejects_blank_title() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(None, "system prompt")?;
        let err = manager
            .update_title(&session_id, "   ")
            .expect_err("expected empty title to fail");
        assert!(err.to_string().contains("title must be a non-empty string"));
        Ok(())
    }

    #[test]
    fn old_session_message_without_scope_defaults_to_main() -> Result<()> {
        let raw = r#"{"type":"message","id":"m1","role":"assistant","content":[{"type":"output_text","text":"hello"}]}"#;
        let parsed: SessionMessage = serde_json::from_str(raw)?;
        match parsed {
            SessionMessage::Message { scope, role, .. } => {
                assert_eq!(scope, SessionScope::main());
                assert_eq!(role, SessionRole::Assistant);
            }
            other => bail!("expected message variant, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn get_all_messages_ignores_auxiliary_items() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        manager.append_text_message(&session_id, SessionRole::User, "main user")?;
        manager.append_text_message_with_scope(
            &session_id,
            SessionScope::shadow(),
            SessionRole::Assistant,
            "shadow note",
        )?;

        let visible = manager.get_all_messages(&session_id)?;
        let texts = visible
            .iter()
            .filter_map(SessionMessage::text_preview)
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec!["system prompt".to_string(), "main user".to_string()]
        );
        Ok(())
    }

    #[test]
    fn scoped_auxiliary_messages_are_stored_but_not_projected_to_main() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system")?;
        let before = manager.to_model_messages(&session_id)?;
        let agent_scope = SessionScope::agent("memory_review_test");
        manager.append_text_message_with_scope(
            &session_id,
            agent_scope.clone(),
            SessionRole::User,
            "auxiliary user",
        )?;
        manager.append_message(
            &session_id,
            &SessionManager::new_tool_use_message_with_scope(
                agent_scope.clone(),
                "call_auxiliary",
                "call_capability",
                serde_json::json!({"capability": "read_file", "args": {"path": "a.txt"}}),
            ),
        )?;
        manager.append_message(
            &session_id,
            &SessionManager::new_tool_result_message_with_scope(
                agent_scope.clone(),
                "call_auxiliary",
                "auxiliary output",
            ),
        )?;

        assert_eq!(manager.to_model_messages(&session_id)?, before);
        let scoped = manager.get_messages_for_scope(&session_id, &agent_scope)?;
        assert_eq!(scoped.len(), 3);
        let scoped_model = manager.to_model_messages_for_scope(&session_id, &agent_scope)?;
        assert_eq!(scoped_model.len(), 3);
        Ok(())
    }

    #[test]
    fn to_openai_messages_ignores_auxiliary_items() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        manager.append_text_message(&session_id, SessionRole::User, "main user")?;
        manager.append_text_message_with_scope(
            &session_id,
            SessionScope::shadow(),
            SessionRole::Assistant,
            "shadow note",
        )?;
        manager.append_message(
            &session_id,
            &SessionManager::new_tool_use_message_with_scope(
                SessionScope::shadow(),
                "call-aux",
                "shell",
                serde_json::json!({ "command": "pwd" }),
            ),
        )?;

        let messages = manager.to_openai_messages(&session_id)?;
        assert_eq!(messages.len(), 2);
        assert!(matches!(messages[0], Message::System(_)));
        assert!(matches!(messages[1], Message::User(_)));
        Ok(())
    }

    #[test]
    fn auxiliary_items_are_persisted_in_raw_session_lines() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        manager.append_text_message_with_scope(
            &session_id,
            SessionScope::shadow(),
            SessionRole::Assistant,
            "shadow note",
        )?;

        let lines = manager.load_session_lines(&session_id)?;
        let found_aux = lines.iter().any(|line| {
            matches!(
                &line.entry,
                SessionEntry::ResponseItem(SessionMessage::Message { scope, .. })
                    if scope == &SessionScope::shadow()
            )
        });
        assert!(found_aux);
        Ok(())
    }

    #[test]
    fn compact_ignores_auxiliary_items() -> Result<()> {
        let manager = test_manager()?;
        let session_id = manager.create_session(Some("demo"), "system prompt")?;
        let user = manager.append_text_message(&session_id, SessionRole::User, "one")?;
        let reply = manager.append_text_message(&session_id, SessionRole::Assistant, "two")?;
        manager.append_text_message_with_scope(
            &session_id,
            SessionScope::shadow(),
            SessionRole::Assistant,
            "shadow note",
        )?;

        manager.compact_session(
            &session_id,
            CompactRequest {
                old_item_ids: vec![user.id().to_string(), reply.id().to_string()],
                new_items: vec![CompactNewItem::Message {
                    role: SessionRole::Assistant,
                    content: vec![ContentItem::OutputText {
                        text: "summary".to_string(),
                    }],
                }],
            },
        )?;

        let visible = manager.get_all_messages(&session_id)?;
        let texts = visible
            .iter()
            .filter_map(SessionMessage::text_preview)
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec!["system prompt".to_string(), "summary".to_string()]
        );

        let lines = manager.load_session_lines(&session_id)?;
        let found_aux = lines.iter().any(|line| {
            matches!(
                &line.entry,
                SessionEntry::ResponseItem(SessionMessage::Message { scope, content, .. })
                    if scope == &SessionScope::shadow()
                        && join_content_text(content) == "shadow note"
            )
        });
        assert!(found_aux);
        Ok(())
    }
}
