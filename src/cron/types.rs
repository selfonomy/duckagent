use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct CronJob {
    pub id: String,
    pub revision: u64,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
    pub schedule: CronSchedule,
    pub task: CronTask,
    pub target: CronTarget,
    #[serde(default)]
    pub policy: CronJobPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CronSchedule {
    Once {
        /// RFC3339 timestamp. Example: 2026-06-01T08:00:00+08:00.
        at: String,
    },
    Interval {
        every_seconds: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<String>,
    },
    Daily {
        /// HH:MM or HH:MM:SS in the selected timezone.
        time: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    },
    Weekly {
        weekdays: Vec<CronWeekday>,
        /// HH:MM or HH:MM:SS in the selected timezone.
        time: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    },
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum CronWeekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CronTask {
    AgentPrompt { prompt: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CronTarget {
    Session {
        session_id: String,
    },
    Gateway {
        channel: String,
        conversation_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CronJobPolicy {
    #[serde(default)]
    pub overlap: CronOverlapPolicy,
    #[serde(default)]
    pub missed_run: CronMissedRunPolicy,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
}

impl Default for CronJobPolicy {
    fn default() -> Self {
        Self {
            overlap: CronOverlapPolicy::Skip,
            missed_run: CronMissedRunPolicy::RunOnce,
            timeout_seconds: default_timeout_seconds(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CronOverlapPolicy {
    #[default]
    Skip,
    Parallel,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CronMissedRunPolicy {
    Skip,
    #[default]
    RunOnce,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
pub struct CronJobPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<CronSchedule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<CronTask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<CronTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<CronJobPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct CronJobView {
    pub job: CronJob,
    pub state: CronJobState,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
pub struct CronJobState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scheduled_for: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<CronRunStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub consecutive_failures: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_run_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CronRunStatus {
    Ok,
    Error,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CronJobEvent {
    JobCreated {
        event_id: String,
        timestamp: String,
        job: CronJob,
    },
    JobUpdated {
        event_id: String,
        timestamp: String,
        job_id: String,
        base_revision: u64,
        revision: u64,
        patch: CronJobPatch,
        job: CronJob,
    },
    JobDeleted {
        event_id: String,
        timestamp: String,
        job_id: String,
        base_revision: u64,
        revision: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CronRunEvent {
    RunStarted {
        event_id: String,
        run_id: String,
        job_id: String,
        scheduled_for: String,
        started_at: String,
    },
    RunFinished {
        event_id: String,
        run_id: String,
        job_id: String,
        status: CronRunStatus,
        finished_at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    RunSkipped {
        event_id: String,
        run_id: String,
        job_id: String,
        scheduled_for: String,
        skipped_at: String,
        reason: String,
    },
}

fn default_timeout_seconds() -> u64 {
    30 * 60
}
