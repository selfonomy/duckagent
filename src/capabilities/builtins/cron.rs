use super::{BuiltinToolSpec, schema_value};
use crate::cron::store::{CronStore, new_id};
use crate::cron::types::{
    CronJob, CronJobPatch, CronJobPolicy, CronSchedule, CronTarget, CronTask,
};
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const CREATE_DESCRIPTION: &str = "Create a durable scheduled task. Use this for reminders and recurring automations. Convert natural language like `in five minutes` into a once schedule with an absolute RFC3339 timestamp; convert `every day at 8 AM` into a daily schedule with time `08:00` and timezone `local` unless the user named a timezone.";
pub const LIST_DESCRIPTION: &str =
    "List durable scheduled tasks, including next run time, revision, and recent run state.";
pub const UPDATE_DESCRIPTION: &str = "Update a scheduled task by appending a new revision. Pass base_revision from cron_list/cron_get when available to avoid overwriting newer changes.";
pub const DELETE_DESCRIPTION: &str =
    "Delete a scheduled task by appending a tombstone event. Existing JSONL records are preserved.";
pub const PAUSE_DESCRIPTION: &str = "Pause a scheduled task by disabling it.";
pub const RESUME_DESCRIPTION: &str = "Resume a paused scheduled task by enabling it.";
pub const GET_DESCRIPTION: &str = "Get one scheduled task by id.";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CronCreateArgs {
    /// Short human-readable job name.
    pub name: String,
    /// Optional longer description.
    #[serde(default)]
    pub description: Option<String>,
    /// Schedule definition. For one-off reminders, use kind=once and an absolute RFC3339 timestamp.
    pub schedule: CronSchedule,
    /// Agent task prompt to run when the schedule fires.
    pub prompt: String,
    /// Optional target. Omit to schedule in the current session.
    #[serde(default)]
    pub target: Option<CronTargetInput>,
    /// Optional execution policy. Defaults are production-safe: no overlap and a 30 minute timeout.
    #[serde(default)]
    pub policy: Option<CronJobPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CronTargetInput {
    CurrentSession,
    Session {
        session_id: String,
    },
    Gateway {
        channel: String,
        conversation_id: String,
        #[serde(default)]
        thread_id: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CronListArgs {
    /// Include disabled jobs. Defaults to true so management actions see paused jobs.
    #[serde(default = "default_true")]
    pub include_disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CronGetArgs {
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CronUpdateArgs {
    pub job_id: String,
    #[serde(default)]
    pub base_revision: Option<u64>,
    pub patch: CronUpdatePatchInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct CronUpdatePatchInput {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub schedule: Option<CronSchedule>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub target: Option<CronTargetInput>,
    #[serde(default)]
    pub policy: Option<CronJobPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CronJobIdArgs {
    pub job_id: String,
    #[serde(default)]
    pub base_revision: Option<u64>,
}

pub fn specs() -> Vec<BuiltinToolSpec> {
    vec![
        BuiltinToolSpec {
            name: "cron_create",
            description: CREATE_DESCRIPTION,
            input_schema: schema_value(schemars::schema_for!(CronCreateArgs)),
        },
        BuiltinToolSpec {
            name: "cron_list",
            description: LIST_DESCRIPTION,
            input_schema: schema_value(schemars::schema_for!(CronListArgs)),
        },
        BuiltinToolSpec {
            name: "cron_get",
            description: GET_DESCRIPTION,
            input_schema: schema_value(schemars::schema_for!(CronGetArgs)),
        },
        BuiltinToolSpec {
            name: "cron_update",
            description: UPDATE_DESCRIPTION,
            input_schema: schema_value(schemars::schema_for!(CronUpdateArgs)),
        },
        BuiltinToolSpec {
            name: "cron_delete",
            description: DELETE_DESCRIPTION,
            input_schema: schema_value(schemars::schema_for!(CronJobIdArgs)),
        },
        BuiltinToolSpec {
            name: "cron_pause",
            description: PAUSE_DESCRIPTION,
            input_schema: schema_value(schemars::schema_for!(CronJobIdArgs)),
        },
        BuiltinToolSpec {
            name: "cron_resume",
            description: RESUME_DESCRIPTION,
            input_schema: schema_value(schemars::schema_for!(CronJobIdArgs)),
        },
    ]
}

pub fn execute(args_capability: &str, args: Value, session_id: &str) -> Result<String> {
    match args_capability {
        "cron_create" => create(args, session_id),
        "cron_list" => list(args),
        "cron_get" => get(args),
        "cron_update" => update(args, session_id),
        "cron_delete" => delete(args),
        "cron_pause" => set_enabled(args, false),
        "cron_resume" => set_enabled(args, true),
        other => Ok(format!("Tool error: unknown cron capability `{other}`.")),
    }
}

fn create(args: Value, session_id: &str) -> Result<String> {
    let args: CronCreateArgs =
        serde_json::from_value(args).context("failed to parse cron_create args")?;
    let prompt = args.prompt.trim();
    if prompt.is_empty() {
        bail!("cron_create.prompt must be non-empty");
    }
    let now = crate::cron::store::now_rfc3339();
    let job = CronJob {
        id: new_id("job"),
        revision: 0,
        name: args.name.trim().to_string(),
        description: args
            .description
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        enabled: true,
        created_at: now.clone(),
        updated_at: now,
        schedule: args.schedule,
        task: CronTask::AgentPrompt {
            prompt: prompt.to_string(),
        },
        target: resolve_target(args.target, session_id)?,
        policy: args.policy.unwrap_or_default(),
    };
    let job = CronStore::new_default()?.create_job(job)?;
    crate::cron::service::wake_all();
    Ok(json!({"status":"created","job": job}).to_string())
}

fn list(args: Value) -> Result<String> {
    let args: CronListArgs =
        serde_json::from_value(args).context("failed to parse cron_list args")?;
    let mut snapshot = CronStore::new_default()?.snapshot(chrono::Utc::now())?;
    if !args.include_disabled {
        snapshot.jobs.retain(|view| view.job.enabled);
    }
    Ok(serde_json::to_string(&snapshot).context("failed to serialize cron_list output")?)
}

fn get(args: Value) -> Result<String> {
    let args: CronGetArgs =
        serde_json::from_value(args).context("failed to parse cron_get args")?;
    let snapshot = CronStore::new_default()?.snapshot(chrono::Utc::now())?;
    let Some(job) = snapshot
        .jobs
        .into_iter()
        .find(|view| view.job.id == args.job_id)
    else {
        return Ok(json!({"status":"not_found","job_id":args.job_id}).to_string());
    };
    Ok(serde_json::to_string(&job).context("failed to serialize cron_get output")?)
}

fn update(args: Value, session_id: &str) -> Result<String> {
    let args: CronUpdateArgs =
        serde_json::from_value(args).context("failed to parse cron_update args")?;
    let patch = CronJobPatch {
        name: args.patch.name,
        description: args.patch.description,
        enabled: args.patch.enabled,
        schedule: args.patch.schedule,
        task: args
            .patch
            .prompt
            .map(|prompt| CronTask::AgentPrompt { prompt }),
        target: args
            .patch
            .target
            .map(|target| resolve_target(Some(target), session_id))
            .transpose()?,
        policy: args.patch.policy,
    };
    let job = CronStore::new_default()?.update_job(&args.job_id, args.base_revision, patch)?;
    crate::cron::service::wake_all();
    Ok(json!({"status":"updated","job":job}).to_string())
}

fn delete(args: Value) -> Result<String> {
    let args: CronJobIdArgs =
        serde_json::from_value(args).context("failed to parse cron_delete args")?;
    let deleted = CronStore::new_default()?.delete_job(&args.job_id, args.base_revision)?;
    crate::cron::service::wake_all();
    Ok(
        json!({"status": if deleted {"deleted"} else {"not_found"},"job_id":args.job_id})
            .to_string(),
    )
}

fn set_enabled(args: Value, enabled: bool) -> Result<String> {
    let args: CronJobIdArgs =
        serde_json::from_value(args).context("failed to parse cron enabled args")?;
    let job = CronStore::new_default()?.update_job(
        &args.job_id,
        args.base_revision,
        CronJobPatch {
            enabled: Some(enabled),
            ..Default::default()
        },
    )?;
    crate::cron::service::wake_all();
    Ok(json!({"status": if enabled {"resumed"} else {"paused"},"job":job}).to_string())
}

fn resolve_target(target: Option<CronTargetInput>, current_session_id: &str) -> Result<CronTarget> {
    match target.unwrap_or(CronTargetInput::CurrentSession) {
        CronTargetInput::CurrentSession => Ok(CronTarget::Session {
            session_id: current_session_id.to_string(),
        }),
        CronTargetInput::Session { session_id } => {
            let session_id = session_id.trim();
            if session_id.is_empty() {
                bail!("cron target session_id must be non-empty");
            }
            Ok(CronTarget::Session {
                session_id: session_id.to_string(),
            })
        }
        CronTargetInput::Gateway {
            channel,
            conversation_id,
            thread_id,
        } => {
            let channel = channel.trim();
            let conversation_id = conversation_id.trim();
            if channel.is_empty() || conversation_id.is_empty() {
                bail!("cron gateway target requires non-empty channel and conversation_id");
            }
            Ok(CronTarget::Gateway {
                channel: channel.to_string(),
                conversation_id: conversation_id.to_string(),
                thread_id: thread_id
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
            })
        }
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_target_is_current_session() -> Result<()> {
        let target = resolve_target(None, "session_1")?;
        assert_eq!(
            target,
            CronTarget::Session {
                session_id: "session_1".to_string()
            }
        );
        Ok(())
    }

    #[test]
    fn gateway_target_is_trimmed_and_validated() -> Result<()> {
        let target = resolve_target(
            Some(CronTargetInput::Gateway {
                channel: " telegram ".to_string(),
                conversation_id: " chat ".to_string(),
                thread_id: Some(" thread ".to_string()),
            }),
            "session",
        )?;

        assert_eq!(
            target,
            CronTarget::Gateway {
                channel: "telegram".to_string(),
                conversation_id: "chat".to_string(),
                thread_id: Some("thread".to_string())
            }
        );
        Ok(())
    }

    #[test]
    fn cron_list_args_reject_unknown_fields() {
        let err = serde_json::from_value::<CronListArgs>(json!({"unexpected": true}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field"));
    }

    #[test]
    fn cron_create_args_reject_unknown_schedule_fields() {
        let err = serde_json::from_value::<CronCreateArgs>(json!({
            "name": "reminder",
            "schedule": {
                "kind": "once",
                "at": "2026-06-01T00:00:00Z",
                "surprise": true
            },
            "prompt": "buy milk"
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("surprise"));
    }
}
