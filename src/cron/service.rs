use super::store::{CronStore, run_due_time};
use super::types::{CronJob, CronOverlapPolicy, CronRunStatus};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

const MAX_TIMER_DELAY: Duration = Duration::from_secs(60);
static GLOBAL_WAKE_HANDLES: OnceLock<Mutex<Vec<CronService>>> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct CronRunContext {
    pub run_id: String,
    pub job_id: String,
    pub scheduled_for: String,
}

#[derive(Debug, Clone)]
pub struct CronSubmitResult {
    pub session_id: Option<String>,
    pub completes_asynchronously: bool,
}

pub trait CronJobExecutor: Send + Sync + 'static {
    fn submit(&self, job: CronJob, context: CronRunContext) -> Result<CronSubmitResult>;
}

#[derive(Clone)]
pub struct CronService {
    inner: Arc<CronServiceInner>,
}

struct CronServiceInner {
    store: CronStore,
    state: Mutex<ServiceState>,
    wake: Condvar,
}

#[derive(Default)]
struct ServiceState {
    started: bool,
    running_jobs: HashSet<String>,
    running_runs: HashMap<String, RunningRun>,
}

#[derive(Debug, Clone)]
struct RunningRun {
    job_id: String,
    session_id: Option<String>,
}

impl CronService {
    pub fn new(store: CronStore) -> Self {
        Self {
            inner: Arc::new(CronServiceInner {
                store,
                state: Mutex::new(ServiceState::default()),
                wake: Condvar::new(),
            }),
        }
    }

    pub fn start(&self, executor: Arc<dyn CronJobExecutor>) {
        {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("cron service mutex poisoned");
            if state.started {
                self.inner.wake.notify_all();
                return;
            }
            state.started = true;
        }
        GLOBAL_WAKE_HANDLES
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .expect("cron wake handles mutex poisoned")
            .push(self.clone());
        let service = self.clone();
        thread::spawn(move || service.run_loop(executor));
    }

    pub fn wake(&self) {
        self.inner.wake.notify_all();
    }

    pub fn finish_run(
        &self,
        run_id: &str,
        job_id: &str,
        status: CronRunStatus,
        session_id: Option<String>,
        error: Option<String>,
    ) -> Result<()> {
        let known = {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("cron service mutex poisoned");
            let removed = state.running_runs.remove(run_id);
            if let Some(run) = removed.as_ref() {
                state.running_jobs.remove(&run.job_id);
            } else {
                state.running_jobs.remove(job_id);
            }
            removed
        };
        if known.is_none() {
            return Ok(());
        }
        let session_id = session_id.or_else(|| known.and_then(|run| run.session_id));
        self.inner
            .store
            .finish_run(run_id, job_id, status, session_id, error)?;
        self.wake();
        Ok(())
    }

    fn run_loop(&self, executor: Arc<dyn CronJobExecutor>) {
        loop {
            match self.tick(executor.clone()) {
                Ok(delay) => self.wait(delay),
                Err(error) => {
                    eprintln!("duckagent cron tick error: {error:#}");
                    self.wait(MAX_TIMER_DELAY);
                }
            }
        }
    }

    fn tick(&self, executor: Arc<dyn CronJobExecutor>) -> Result<Duration> {
        let now = Utc::now();
        let active_run_ids = self.active_run_ids();
        self.inner
            .store
            .finish_expired_running_runs(now, &active_run_ids)?;
        let snapshot = self.inner.store.snapshot(now)?;
        for warning in &snapshot.warnings {
            eprintln!("duckagent cron warning: {warning}");
        }

        let mut next_delay = MAX_TIMER_DELAY;
        for view in snapshot.jobs {
            let Some(next_run_at) = run_due_time(&view)? else {
                continue;
            };
            if next_run_at <= now {
                if view.job.policy.overlap == CronOverlapPolicy::Skip
                    && view
                        .state
                        .running_run_id
                        .as_ref()
                        .is_some_and(|run_id| !active_run_ids.contains(run_id))
                {
                    self.inner.store.skip_run(
                        &view.job.id,
                        next_run_at,
                        "previous run is still marked running",
                    )?;
                    continue;
                }
                self.run_due_job(view.job, next_run_at, executor.clone())?;
                continue;
            }
            let delay = duration_until(now, next_run_at);
            if delay < next_delay {
                next_delay = delay;
            }
        }
        Ok(next_delay.min(MAX_TIMER_DELAY))
    }

    fn run_due_job(
        &self,
        job: CronJob,
        scheduled_for: DateTime<Utc>,
        executor: Arc<dyn CronJobExecutor>,
    ) -> Result<()> {
        if !self.try_mark_job_running(&job) {
            self.inner
                .store
                .skip_run(&job.id, scheduled_for, "job is already running")?;
            return Ok(());
        }

        let store = self.inner.store.clone();
        let service = self.clone();
        thread::spawn(move || {
            let started = match store.start_run(&job.id, scheduled_for) {
                Ok(started) => started,
                Err(error) => {
                    eprintln!(
                        "duckagent cron failed to start run for {}: {error:#}",
                        job.id
                    );
                    service.clear_running_job(&job.id);
                    service.wake();
                    return;
                }
            };
            let context = CronRunContext {
                run_id: started.run_id.clone(),
                job_id: job.id.clone(),
                scheduled_for: scheduled_for.to_rfc3339(),
            };
            service.mark_running_run(context.run_id.clone(), job.id.clone(), None);
            service.spawn_timeout_watch(job.clone(), context.clone());
            match executor.submit(job.clone(), context.clone()) {
                Ok(result) if result.completes_asynchronously => {
                    service.set_running_run_session_id(&context.run_id, result.session_id.clone());
                }
                Ok(result) => {
                    if let Err(error) = service.record_run_finished(
                        &context.run_id,
                        &context.job_id,
                        CronRunStatus::Ok,
                        result.session_id,
                        None,
                    ) {
                        eprintln!("duckagent cron failed to finish run: {error:#}");
                    }
                }
                Err(error) => {
                    if let Err(finish_error) = service.record_run_finished(
                        &context.run_id,
                        &context.job_id,
                        CronRunStatus::Error,
                        None,
                        Some(format!("{error:#}")),
                    ) {
                        eprintln!("duckagent cron failed to record run error: {finish_error:#}");
                    }
                }
            }
        });
        Ok(())
    }

    fn record_run_finished(
        &self,
        run_id: &str,
        job_id: &str,
        status: CronRunStatus,
        session_id: Option<String>,
        error: Option<String>,
    ) -> Result<()> {
        let known = {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("cron service mutex poisoned");
            let removed = state.running_runs.remove(run_id);
            if let Some(run) = removed.as_ref() {
                state.running_jobs.remove(&run.job_id);
            } else {
                state.running_jobs.remove(job_id);
            }
            removed
        };
        let Some(known) = known else {
            self.wake();
            return Ok(());
        };
        let session_id = session_id.or(known.session_id);
        self.inner
            .store
            .finish_run(run_id, job_id, status, session_id, error)?;
        self.wake();
        Ok(())
    }

    fn set_running_run_session_id(&self, run_id: &str, session_id: Option<String>) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("cron service mutex poisoned");
        if let Some(run) = state.running_runs.get_mut(run_id) {
            run.session_id = session_id;
        }
    }

    fn try_mark_job_running(&self, job: &CronJob) -> bool {
        if job.policy.overlap == CronOverlapPolicy::Parallel {
            return true;
        }
        let mut state = self
            .inner
            .state
            .lock()
            .expect("cron service mutex poisoned");
        if state.running_jobs.contains(&job.id) {
            return false;
        }
        state.running_jobs.insert(job.id.clone());
        true
    }

    fn mark_running_run(&self, run_id: String, job_id: String, session_id: Option<String>) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("cron service mutex poisoned");
        state
            .running_runs
            .insert(run_id, RunningRun { job_id, session_id });
    }

    fn active_run_ids(&self) -> HashSet<String> {
        self.inner
            .state
            .lock()
            .expect("cron service mutex poisoned")
            .running_runs
            .keys()
            .cloned()
            .collect()
    }

    fn clear_running_job(&self, job_id: &str) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("cron service mutex poisoned");
        state.running_jobs.remove(job_id);
    }

    fn spawn_timeout_watch(&self, job: CronJob, context: CronRunContext) {
        let service = self.clone();
        thread::spawn(move || {
            let timeout = Duration::from_secs(job.policy.timeout_seconds.max(1));
            thread::sleep(timeout);
            let still_running = {
                let state = service
                    .inner
                    .state
                    .lock()
                    .expect("cron service mutex poisoned");
                state.running_runs.contains_key(&context.run_id)
            };
            if !still_running {
                return;
            }
            let _ = service.finish_run(
                &context.run_id,
                &context.job_id,
                CronRunStatus::Error,
                None,
                Some(format!(
                    "cron job timed out after {} seconds",
                    timeout.as_secs()
                )),
            );
        });
    }

    fn wait(&self, delay: Duration) {
        let guard = self
            .inner
            .state
            .lock()
            .expect("cron service mutex poisoned");
        let _ = self
            .inner
            .wake
            .wait_timeout(guard, delay.max(Duration::from_millis(50)));
    }
}

fn duration_until(now: DateTime<Utc>, target: DateTime<Utc>) -> Duration {
    target
        .signed_duration_since(now)
        .to_std()
        .unwrap_or_else(|_| Duration::from_millis(0))
}

pub fn build_scheduled_event_prompt(job: &CronJob, context: &CronRunContext) -> String {
    let prompt = match &job.task {
        super::types::CronTask::AgentPrompt { prompt } => prompt.trim(),
    };
    format!(
        "[Scheduled Task Event]\njob_id: {}\njob_name: {}\nrun_id: {}\nscheduled_for: {}\n\nTask:\n{}\n\nInstructions:\nThis event was triggered automatically by DuckAgent cron. Use the available capabilities as needed, then finish with a concise user-facing result.",
        job.id,
        job.name.trim(),
        context.run_id,
        context.scheduled_for,
        prompt
    )
}

pub fn wake_all() {
    if let Some(handles) = GLOBAL_WAKE_HANDLES.get() {
        for handle in handles
            .lock()
            .expect("cron wake handles mutex poisoned")
            .iter()
        {
            handle.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::types::{CronJobPatch, CronJobPolicy, CronSchedule, CronTarget, CronTask};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    #[derive(Default)]
    struct CountingExecutor {
        submitted: AtomicUsize,
    }

    impl CronJobExecutor for CountingExecutor {
        fn submit(&self, _job: CronJob, _context: CronRunContext) -> Result<CronSubmitResult> {
            self.submitted.fetch_add(1, Ordering::SeqCst);
            Ok(CronSubmitResult {
                session_id: Some("session".to_string()),
                completes_asynchronously: false,
            })
        }
    }

    fn test_job(schedule: CronSchedule) -> CronJob {
        CronJob {
            id: String::new(),
            revision: 0,
            name: "reminder".to_string(),
            description: None,
            enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
            schedule,
            task: CronTask::AgentPrompt {
                prompt: "buy milk".to_string(),
            },
            target: CronTarget::Session {
                session_id: "session".to_string(),
            },
            policy: CronJobPolicy::default(),
        }
    }

    fn wait_until(assertion: impl Fn() -> bool) {
        for _ in 0..100 {
            if assertion() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn scheduled_event_prompt_contains_run_context_and_trimmed_task() {
        let job = test_job(CronSchedule::Once {
            at: "2026-06-01T00:00:00Z".to_string(),
        });
        let context = CronRunContext {
            run_id: "run_1".to_string(),
            job_id: "job_1".to_string(),
            scheduled_for: "2026-06-01T00:00:00Z".to_string(),
        };

        let prompt = build_scheduled_event_prompt(
            &CronJob {
                id: "job_1".to_string(),
                task: CronTask::AgentPrompt {
                    prompt: "  remind the user  ".to_string(),
                },
                ..job
            },
            &context,
        );

        assert!(prompt.contains("[Scheduled Task Event]"));
        assert!(prompt.contains("job_id: job_1"));
        assert!(prompt.contains("run_id: run_1"));
        assert!(prompt.contains("Task:\nremind the user\n\nInstructions:"));
    }

    #[test]
    fn tick_runs_due_job_and_records_success() -> Result<()> {
        let dir = tempdir()?;
        let store = CronStore::new(dir.path().to_path_buf())?;
        let future = Utc::now() + chrono::Duration::days(1);
        let job = store.create_job(test_job(CronSchedule::Once {
            at: future.to_rfc3339(),
        }))?;
        let job = store.update_job(
            &job.id,
            Some(job.revision),
            CronJobPatch {
                schedule: Some(CronSchedule::Once {
                    at: job.created_at.clone(),
                }),
                ..Default::default()
            },
        )?;
        let service = CronService::new(store.clone());
        let executor = Arc::new(CountingExecutor::default());

        service.tick(executor.clone())?;
        wait_until(|| {
            executor.submitted.load(Ordering::SeqCst) == 1
                && store.snapshot(Utc::now()).ok().and_then(|snapshot| {
                    snapshot
                        .jobs
                        .into_iter()
                        .find(|view| view.job.id == job.id)
                        .and_then(|view| view.state.last_status)
                }) == Some(CronRunStatus::Ok)
        });

        let snapshot = store.snapshot(Utc::now())?;
        let state = &snapshot
            .jobs
            .iter()
            .find(|view| view.job.id == job.id)
            .expect("job still exists")
            .state;
        assert_eq!(state.last_status, Some(CronRunStatus::Ok));
        assert_eq!(state.running_run_id, None);
        Ok(())
    }

    #[test]
    fn tick_skips_due_run_when_previous_orphan_is_still_within_timeout() -> Result<()> {
        let dir = tempdir()?;
        let store = CronStore::new(dir.path().to_path_buf())?;
        let now = Utc::now();
        let mut job = test_job(CronSchedule::Interval {
            every_seconds: 60,
            anchor: None,
        });
        job.policy = CronJobPolicy {
            timeout_seconds: 3600,
            ..Default::default()
        };
        let job = store.create_job(job)?;
        store.start_run(&job.id, now - chrono::Duration::seconds(120))?;
        let service = CronService::new(store.clone());
        let executor = Arc::new(CountingExecutor::default());

        service.tick(executor.clone())?;

        assert_eq!(executor.submitted.load(Ordering::SeqCst), 0);
        let snapshot = store.snapshot(Utc::now())?;
        let state = &snapshot.jobs[0].state;
        assert_eq!(state.last_status, Some(CronRunStatus::Skipped));
        assert!(state.running_run_id.is_some());
        assert!(
            state
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("previous run")
        );
        Ok(())
    }
}
