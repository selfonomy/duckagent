use super::schedule::{compute_next_run_at, parse_rfc3339_utc};
use super::types::{
    CronJob, CronJobEvent, CronJobPatch, CronJobState, CronJobView, CronRunEvent, CronRunStatus,
};
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const JOBS_FILE_NAME: &str = "jobs.jsonl";
const RUNS_FILE_NAME: &str = "runs.jsonl";
const LOCK_FILE_NAME: &str = ".store.lock";
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_STALE_AFTER: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone)]
pub struct CronStore {
    dir: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct CronSnapshot {
    pub jobs: Vec<CronJobView>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CronRunStart {
    pub run_id: String,
    pub started_at: String,
}

impl CronStore {
    pub fn new_default() -> Result<Self> {
        Self::new(crate::profiles::active_profile_path("cron")?)
    }

    pub fn new(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create cron dir: {}", dir.display()))?;
        Ok(Self { dir })
    }

    pub fn snapshot(&self, now: DateTime<Utc>) -> Result<CronSnapshot> {
        let jobs = self.replay_jobs()?.into_values().collect::<Vec<_>>();
        let run_state = self.replay_run_state()?;
        let mut warnings = Vec::new();
        let mut views = Vec::new();
        for job in jobs {
            let mut state = run_state.get(&job.id).cloned().unwrap_or_default();
            match compute_next_run_at(&job, &state, now) {
                Ok(next) => {
                    state.next_run_at = next.map(|dt| dt.to_rfc3339());
                }
                Err(error) => {
                    warnings.push(format!(
                        "job `{}` schedule is invalid and will not run: {error:#}",
                        job.id
                    ));
                    state.last_error = Some(format!("schedule error: {error:#}"));
                }
            }
            views.push(CronJobView { job, state });
        }
        views.sort_by(|a, b| {
            a.state
                .next_run_at
                .cmp(&b.state.next_run_at)
                .then_with(|| a.job.name.cmp(&b.job.name))
                .then_with(|| a.job.id.cmp(&b.job.id))
        });
        Ok(CronSnapshot {
            jobs: views,
            warnings,
        })
    }

    pub fn create_job(&self, mut job: CronJob) -> Result<CronJob> {
        self.with_lock(|| {
            let now = now_rfc3339();
            if job.id.trim().is_empty() {
                job.id = new_id("job");
            }
            if job.name.trim().is_empty() {
                bail!("cron job name must be non-empty");
            }
            job.revision = 1;
            job.created_at = now.clone();
            job.updated_at = now.clone();
            let mut jobs = self.replay_jobs()?;
            if jobs.contains_key(&job.id) {
                bail!("cron job id already exists: {}", job.id);
            }
            validate_job(&job)?;
            let event = CronJobEvent::JobCreated {
                event_id: new_id("evt"),
                timestamp: now,
                job: job.clone(),
            };
            append_jsonl(&self.jobs_path(), &event)?;
            jobs.insert(job.id.clone(), job.clone());
            Ok(job)
        })
    }

    pub fn update_job(
        &self,
        job_id: &str,
        base_revision: Option<u64>,
        patch: CronJobPatch,
    ) -> Result<CronJob> {
        self.with_lock(|| {
            let mut jobs = self.replay_jobs()?;
            let mut job = jobs
                .remove(job_id)
                .ok_or_else(|| anyhow!("unknown cron job id: {job_id}"))?;
            if let Some(base) = base_revision {
                if job.revision != base {
                    bail!(
                        "cron job revision conflict for `{job_id}`: current={}, requested_base={base}",
                        job.revision
                    );
                }
            }
            apply_patch(&mut job, &patch);
            job.revision = job.revision.saturating_add(1);
            job.updated_at = now_rfc3339();
            validate_job(&job)?;
            let event = CronJobEvent::JobUpdated {
                event_id: new_id("evt"),
                timestamp: job.updated_at.clone(),
                job_id: job.id.clone(),
                base_revision: job.revision.saturating_sub(1),
                revision: job.revision,
                patch,
                job: job.clone(),
            };
            append_jsonl(&self.jobs_path(), &event)?;
            Ok(job)
        })
    }

    pub fn delete_job(&self, job_id: &str, base_revision: Option<u64>) -> Result<bool> {
        self.with_lock(|| {
            let jobs = self.replay_jobs()?;
            let Some(job) = jobs.get(job_id) else {
                return Ok(false);
            };
            if let Some(base) = base_revision {
                if job.revision != base {
                    bail!(
                        "cron job revision conflict for `{job_id}`: current={}, requested_base={base}",
                        job.revision
                    );
                }
            }
            let event = CronJobEvent::JobDeleted {
                event_id: new_id("evt"),
                timestamp: now_rfc3339(),
                job_id: job_id.to_string(),
                base_revision: job.revision,
                revision: job.revision.saturating_add(1),
            };
            append_jsonl(&self.jobs_path(), &event)?;
            Ok(true)
        })
    }

    pub fn start_run(&self, job_id: &str, scheduled_for: DateTime<Utc>) -> Result<CronRunStart> {
        self.with_lock(|| {
            let run = CronRunStart {
                run_id: new_id("run"),
                started_at: now_rfc3339(),
            };
            let event = CronRunEvent::RunStarted {
                event_id: new_id("evt"),
                run_id: run.run_id.clone(),
                job_id: job_id.to_string(),
                scheduled_for: scheduled_for.to_rfc3339(),
                started_at: run.started_at.clone(),
            };
            append_jsonl(&self.runs_path(), &event)?;
            Ok(run)
        })
    }

    pub fn finish_run(
        &self,
        run_id: &str,
        job_id: &str,
        status: CronRunStatus,
        session_id: Option<String>,
        error: Option<String>,
    ) -> Result<()> {
        self.with_lock(|| {
            let event = CronRunEvent::RunFinished {
                event_id: new_id("evt"),
                run_id: run_id.to_string(),
                job_id: job_id.to_string(),
                status,
                finished_at: now_rfc3339(),
                session_id,
                error,
            };
            append_jsonl(&self.runs_path(), &event)
        })
    }

    pub fn skip_run(&self, job_id: &str, scheduled_for: DateTime<Utc>, reason: &str) -> Result<()> {
        self.with_lock(|| {
            let event = CronRunEvent::RunSkipped {
                event_id: new_id("evt"),
                run_id: new_id("run"),
                job_id: job_id.to_string(),
                scheduled_for: scheduled_for.to_rfc3339(),
                skipped_at: now_rfc3339(),
                reason: reason.to_string(),
            };
            append_jsonl(&self.runs_path(), &event)
        })
    }

    fn replay_jobs(&self) -> Result<BTreeMap<String, CronJob>> {
        let mut jobs = BTreeMap::new();
        for event in read_jsonl_best_effort::<CronJobEvent>(&self.jobs_path())? {
            match event {
                CronJobEvent::JobCreated { job, .. } => {
                    jobs.insert(job.id.clone(), job);
                }
                CronJobEvent::JobUpdated { job, .. } => {
                    jobs.insert(job.id.clone(), job);
                }
                CronJobEvent::JobDeleted { job_id, .. } => {
                    jobs.remove(&job_id);
                }
            }
        }
        Ok(jobs)
    }

    fn replay_run_state(&self) -> Result<BTreeMap<String, CronJobState>> {
        let mut states = BTreeMap::<String, CronJobState>::new();
        for event in read_jsonl_best_effort::<CronRunEvent>(&self.runs_path())? {
            match event {
                CronRunEvent::RunStarted {
                    run_id,
                    job_id,
                    scheduled_for,
                    started_at,
                    ..
                } => {
                    let state = states.entry(job_id).or_default();
                    state.last_scheduled_for = Some(scheduled_for);
                    state.last_started_at = Some(started_at);
                    state.running_run_id = Some(run_id);
                }
                CronRunEvent::RunFinished {
                    run_id,
                    job_id,
                    status,
                    finished_at,
                    error,
                    ..
                } => {
                    let state = states.entry(job_id).or_default();
                    if state.running_run_id.as_deref() == Some(run_id.as_str()) {
                        state.running_run_id = None;
                    }
                    state.last_finished_at = Some(finished_at);
                    state.last_status = Some(status);
                    state.last_error = error;
                    if status == CronRunStatus::Ok || status == CronRunStatus::Skipped {
                        state.consecutive_failures = 0;
                    } else {
                        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                    }
                }
                CronRunEvent::RunSkipped {
                    job_id,
                    scheduled_for,
                    skipped_at,
                    reason,
                    ..
                } => {
                    let state = states.entry(job_id).or_default();
                    state.last_scheduled_for = Some(scheduled_for);
                    state.last_finished_at = Some(skipped_at);
                    state.last_status = Some(CronRunStatus::Skipped);
                    state.last_error = Some(reason);
                }
            }
        }
        Ok(states)
    }

    pub fn finish_expired_running_runs(
        &self,
        now: DateTime<Utc>,
        active_run_ids: &HashSet<String>,
    ) -> Result<()> {
        self.with_lock(|| {
            let jobs = self.replay_jobs()?;
            let states = self.replay_run_state()?;
            for (job_id, state) in states {
                let Some(run_id) = state.running_run_id else {
                    continue;
                };
                if active_run_ids.contains(&run_id) {
                    continue;
                }
                let Some(job) = jobs.get(&job_id) else {
                    continue;
                };
                let Some(started_at) = state.last_started_at.as_deref() else {
                    continue;
                };
                let started_at = parse_rfc3339_utc(started_at)?;
                let expires_at =
                    started_at + chrono::Duration::seconds(job.policy.timeout_seconds as i64);
                if expires_at > now {
                    continue;
                }
                let event = CronRunEvent::RunFinished {
                    event_id: new_id("evt"),
                    run_id,
                    job_id: job_id.clone(),
                    status: CronRunStatus::Error,
                    finished_at: now.to_rfc3339(),
                    session_id: None,
                    error: Some(format!(
                        "cron job timed out after {} seconds",
                        job.policy.timeout_seconds
                    )),
                };
                append_jsonl(&self.runs_path(), &event)?;
            }
            Ok(())
        })
    }

    fn jobs_path(&self) -> PathBuf {
        self.dir.join(JOBS_FILE_NAME)
    }

    fn runs_path(&self) -> PathBuf {
        self.dir.join(RUNS_FILE_NAME)
    }

    fn with_lock<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let _lock = StoreLock::acquire(self.dir.join(LOCK_FILE_NAME))?;
        f()
    }
}

fn validate_job(job: &CronJob) -> Result<()> {
    if job.id.trim().is_empty() {
        bail!("cron job id must be non-empty");
    }
    if job.name.trim().is_empty() {
        bail!("cron job name must be non-empty");
    }
    match &job.task {
        super::types::CronTask::AgentPrompt { prompt } if prompt.trim().is_empty() => {
            bail!("cron agent_prompt task requires non-empty prompt")
        }
        _ => {}
    }
    if job.policy.timeout_seconds == 0 {
        bail!("cron job timeout_seconds must be greater than zero");
    }
    let next = compute_next_run_at(job, &CronJobState::default(), Utc::now())?;
    if job.enabled && next.is_none() {
        bail!("cron job schedule has no next run");
    }
    Ok(())
}

fn apply_patch(job: &mut CronJob, patch: &CronJobPatch) {
    if let Some(name) = patch.name.as_ref() {
        job.name = name.trim().to_string();
    }
    if let Some(description) = patch.description.as_ref() {
        let description = description.trim();
        job.description = if description.trim().is_empty() {
            None
        } else {
            Some(description.to_string())
        };
    }
    if let Some(enabled) = patch.enabled {
        job.enabled = enabled;
    }
    if let Some(schedule) = patch.schedule.as_ref() {
        job.schedule = schedule.clone();
    }
    if let Some(task) = patch.task.as_ref() {
        job.task = match task {
            super::types::CronTask::AgentPrompt { prompt } => super::types::CronTask::AgentPrompt {
                prompt: prompt.trim().to_string(),
            },
        };
    }
    if let Some(target) = patch.target.as_ref() {
        job.target = target.clone();
    }
    if let Some(policy) = patch.policy.as_ref() {
        job.policy = policy.clone();
    }
}

fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory: {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open cron jsonl for append: {}", path.display()))?;
    writeln!(file, "{}", serde_json::to_string(value)?)
        .with_context(|| format!("failed to append cron jsonl: {}", path.display()))
}

fn read_jsonl_best_effort<T>(path: &Path) -> Result<Vec<T>>
where
    T: for<'de> serde::Deserialize<'de>,
{
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read cron jsonl: {}", path.display()));
        }
    };
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<T>(line) {
            Ok(value) => out.push(value),
            Err(error) => {
                eprintln!(
                    "duckagent cron ignored malformed line {} in {}: {error}",
                    index + 1,
                    path.display()
                );
            }
        }
    }
    Ok(out)
}

struct StoreLock {
    path: PathBuf,
}

impl StoreLock {
    fn acquire(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create cron lock dir: {}", parent.display()))?;
        }
        let start = Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{} {}", std::process::id(), Utc::now().to_rfc3339()).ok();
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    remove_stale_lock_if_needed(&path)?;
                    if start.elapsed() >= LOCK_TIMEOUT {
                        bail!("timed out waiting for cron store lock: {}", path.display());
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to create cron lock: {}", path.display())
                    });
                }
            }
        }
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn remove_stale_lock_if_needed(path: &Path) -> Result<()> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(());
    };
    let Ok(modified) = metadata.modified() else {
        return Ok(());
    };
    if modified.elapsed().unwrap_or_default() >= LOCK_STALE_AFTER {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::now_v7().simple())
}

pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

pub fn run_due_time(view: &CronJobView) -> Result<Option<DateTime<Utc>>> {
    view.state
        .next_run_at
        .as_deref()
        .map(parse_rfc3339_utc)
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::types::{CronJobPolicy, CronSchedule, CronTarget, CronTask};
    use tempfile::tempdir;

    fn test_job() -> CronJob {
        CronJob {
            id: String::new(),
            revision: 0,
            name: "reminder".to_string(),
            description: None,
            enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
            schedule: CronSchedule::Once {
                at: (Utc::now() + chrono::Duration::days(1)).to_rfc3339(),
            },
            task: CronTask::AgentPrompt {
                prompt: "buy milk".to_string(),
            },
            target: CronTarget::Session {
                session_id: "session".to_string(),
            },
            policy: Default::default(),
        }
    }

    #[test]
    fn create_update_delete_replays_from_append_only_events() -> Result<()> {
        let dir = tempdir()?;
        let store = CronStore::new(dir.path().to_path_buf())?;
        let job = store.create_job(test_job())?;
        assert_eq!(job.revision, 1);

        let updated = store.update_job(
            &job.id,
            Some(1),
            CronJobPatch {
                name: Some("new name".to_string()),
                ..Default::default()
            },
        )?;
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.name, "new name");

        let snapshot = store.snapshot(parse_rfc3339_utc("2026-06-01T00:01:00Z")?)?;
        assert_eq!(snapshot.jobs.len(), 1);
        assert_eq!(snapshot.jobs[0].job.name, "new name");

        assert!(store.delete_job(&job.id, Some(2))?);
        let snapshot = store.snapshot(Utc::now())?;
        assert!(snapshot.jobs.is_empty());
        Ok(())
    }

    #[test]
    fn update_revision_conflict_is_rejected() -> Result<()> {
        let dir = tempdir()?;
        let store = CronStore::new(dir.path().to_path_buf())?;
        let job = store.create_job(test_job())?;
        let err = store
            .update_job(
                &job.id,
                Some(999),
                CronJobPatch {
                    name: Some("bad".to_string()),
                    ..Default::default()
                },
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("revision conflict"));
        Ok(())
    }

    #[test]
    fn update_trims_user_visible_fields() -> Result<()> {
        let dir = tempdir()?;
        let store = CronStore::new(dir.path().to_path_buf())?;
        let job = store.create_job(test_job())?;

        let updated = store.update_job(
            &job.id,
            Some(1),
            CronJobPatch {
                name: Some("  new name  ".to_string()),
                description: Some("  description  ".to_string()),
                task: Some(CronTask::AgentPrompt {
                    prompt: "  do the thing  ".to_string(),
                }),
                ..Default::default()
            },
        )?;

        assert_eq!(updated.name, "new name");
        assert_eq!(updated.description.as_deref(), Some("description"));
        assert_eq!(
            updated.task,
            CronTask::AgentPrompt {
                prompt: "do the thing".to_string()
            }
        );
        Ok(())
    }

    #[test]
    fn zero_timeout_is_rejected() -> Result<()> {
        let dir = tempdir()?;
        let store = CronStore::new(dir.path().to_path_buf())?;
        let mut job = test_job();
        job.policy = CronJobPolicy {
            timeout_seconds: 0,
            ..Default::default()
        };

        let err = store.create_job(job).unwrap_err().to_string();
        assert!(err.contains("timeout_seconds"));
        Ok(())
    }

    #[test]
    fn skipped_run_does_not_hide_existing_running_run() -> Result<()> {
        let dir = tempdir()?;
        let store = CronStore::new(dir.path().to_path_buf())?;
        let job = store.create_job(test_job())?;
        let run = store.start_run(&job.id, parse_rfc3339_utc("2026-06-01T00:00:00Z")?)?;

        store.skip_run(
            &job.id,
            parse_rfc3339_utc("2026-06-01T00:05:00Z")?,
            "job is already running",
        )?;
        let snapshot = store.snapshot(parse_rfc3339_utc("2026-06-01T00:06:00Z")?)?;
        let state = &snapshot.jobs[0].state;
        assert_eq!(state.running_run_id.as_deref(), Some(run.run_id.as_str()));
        assert_eq!(state.last_status, Some(CronRunStatus::Skipped));
        assert_eq!(
            state.last_scheduled_for.as_deref(),
            Some("2026-06-01T00:05:00+00:00")
        );

        store.finish_run(&run.run_id, &job.id, CronRunStatus::Ok, None, None)?;
        let snapshot = store.snapshot(parse_rfc3339_utc("2026-06-01T00:06:00Z")?)?;
        let state = &snapshot.jobs[0].state;
        assert_eq!(state.running_run_id, None);
        assert_eq!(state.last_status, Some(CronRunStatus::Ok));
        Ok(())
    }

    #[test]
    fn expired_orphaned_run_is_finished_as_error() -> Result<()> {
        let dir = tempdir()?;
        let store = CronStore::new(dir.path().to_path_buf())?;
        let mut job = test_job();
        job.policy = CronJobPolicy {
            timeout_seconds: 1,
            ..Default::default()
        };
        let job = store.create_job(job)?;
        append_jsonl(
            &store.runs_path(),
            &CronRunEvent::RunStarted {
                event_id: "evt_started".to_string(),
                run_id: "run_orphan".to_string(),
                job_id: job.id.clone(),
                scheduled_for: "2026-06-01T00:00:00Z".to_string(),
                started_at: "2026-06-01T00:00:00Z".to_string(),
            },
        )?;

        store.finish_expired_running_runs(
            parse_rfc3339_utc("2026-06-01T00:00:02Z")?,
            &HashSet::new(),
        )?;

        let snapshot = store.snapshot(parse_rfc3339_utc("2026-06-01T00:00:03Z")?)?;
        let state = &snapshot.jobs[0].state;
        assert_eq!(state.running_run_id, None);
        assert_eq!(state.last_status, Some(CronRunStatus::Error));
        assert_eq!(state.consecutive_failures, 1);
        assert!(
            state
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("timed out")
        );
        Ok(())
    }

    #[test]
    fn active_running_run_is_not_expired_by_reconciliation() -> Result<()> {
        let dir = tempdir()?;
        let store = CronStore::new(dir.path().to_path_buf())?;
        let mut job = test_job();
        job.policy = CronJobPolicy {
            timeout_seconds: 1,
            ..Default::default()
        };
        let job = store.create_job(job)?;
        append_jsonl(
            &store.runs_path(),
            &CronRunEvent::RunStarted {
                event_id: "evt_started".to_string(),
                run_id: "run_active".to_string(),
                job_id: job.id.clone(),
                scheduled_for: "2026-06-01T00:00:00Z".to_string(),
                started_at: "2026-06-01T00:00:00Z".to_string(),
            },
        )?;
        let active = HashSet::from(["run_active".to_string()]);

        store.finish_expired_running_runs(parse_rfc3339_utc("2026-06-01T00:00:02Z")?, &active)?;

        let snapshot = store.snapshot(parse_rfc3339_utc("2026-06-01T00:00:03Z")?)?;
        let state = &snapshot.jobs[0].state;
        assert_eq!(state.running_run_id.as_deref(), Some("run_active"));
        assert_eq!(state.last_status, None);
        Ok(())
    }
}
