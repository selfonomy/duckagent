use crate::approval::ApprovalProvider;
use crate::sandbox::config::resolve_sandbox;
use crate::sandbox::runner::{bypass_sandbox_for_tests, sandbox_command_with_target};
use crate::session::{ProcessEventPayload, SessionEntry, SessionLine, SessionManager};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::Args;
use portable_pty::{Child, ChildKiller, CommandBuilder, NativePtySystem, PtySize, PtySystem};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[cfg(windows)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

const DEFAULT_READ_LIMIT: usize = 12_000;
const EVENT_TAIL_LIMIT: usize = 4_000;
const START_OBSERVE_MS: u64 = 5_000;

#[derive(Debug, Clone, Args)]
pub struct ProcessSupervisorCommand {
    #[arg(long)]
    pub process_id: String,
    #[arg(long)]
    pub session_id: String,
    #[arg(long)]
    pub command: String,
    #[arg(long)]
    pub cwd: PathBuf,
    #[arg(long)]
    pub state_path: PathBuf,
    #[arg(long)]
    pub log_path: PathBuf,
    #[arg(long)]
    pub messages_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProcessState {
    process_id: String,
    session_id: String,
    command: String,
    cwd: String,
    background: bool,
    pty: bool,
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    supervisor_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pgid: Option<i32>,
    started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watch: Option<ProcessWatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProcessWatch {
    event: String,
    on_event: String,
    registered_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    triggered_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProcessActionEnvelope {
    action: String,
}

#[derive(Debug, Deserialize)]
struct ProcessStartArgs {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    pty: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ProcessIdArgs {
    process_id: String,
}

#[derive(Debug, Deserialize)]
struct ProcessReadArgs {
    process_id: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    cursor: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    query: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProcessWatchArgs {
    process_id: String,
    event: String,
    on_event: String,
}

#[derive(Debug, Deserialize)]
struct ProcessWriteArgs {
    process_id: String,
    data: String,
}

#[derive(Clone)]
struct LivePty {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellInvocation {
    program: String,
    args: Vec<String>,
}

impl ShellInvocation {
    fn args_with_command(&self, command: &str) -> Vec<String> {
        let mut args = self.args.clone();
        args.push(command.to_string());
        args
    }
}

static LIVE_PTYS: OnceLock<Mutex<HashMap<String, LivePty>>> = OnceLock::new();

pub fn execute_process_action(
    session_manager: &SessionManager,
    session_id: &str,
    args: Value,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<String> {
    let envelope: ProcessActionEnvelope =
        serde_json::from_value(args.clone()).context("failed to parse process args")?;
    match envelope.action.trim() {
        "start" => start_process(session_manager, session_id, args, approval_provider),
        "read" => read_process(session_manager, session_id, args),
        "list" => list_processes(session_manager, session_id),
        "stop" => stop_process(session_manager, session_id, args),
        "watch" => watch_process(session_manager, session_id, args),
        "write" => write_process(args),
        action if action.is_empty() => Ok(
            "Tool error: process args.action must be one of start, read, list, stop, watch, write."
                .to_string(),
        ),
        action => Ok(format!(
            "Tool error: unknown process action `{action}`. Use start, read, list, stop, watch, or write."
        )),
    }
}

pub fn run_process_supervisor(command: ProcessSupervisorCommand) -> Result<()> {
    let now = now_rfc3339();
    let mut state = read_state(&command.state_path).unwrap_or_else(|_| ProcessState {
        process_id: command.process_id.clone(),
        session_id: command.session_id.clone(),
        command: command.command.clone(),
        cwd: command.cwd.to_string_lossy().to_string(),
        background: true,
        pty: false,
        status: "starting".to_string(),
        supervisor_pid: None,
        pid: None,
        pgid: None,
        started_at: now.clone(),
        ended_at: None,
        exit_code: None,
        watch: None,
    });
    state.supervisor_pid = Some(std::process::id());

    append_log_line(
        &command.log_path,
        &format!("[duckagent] starting process {}\n", command.process_id),
    )?;

    let sandbox = resolve_sandbox().context("failed to resolve sandbox for process supervisor")?;

    let shell = default_shell_invocation();
    let args = shell.args_with_command(&command.command);
    let mut inherited_proxy_env = inherited_proxy_environment();
    let mut child_command = sandbox_command_with_target(
        &sandbox,
        Some(&command.cwd),
        std::mem::take(&mut inherited_proxy_env),
        &shell.program,
        &args,
        None,
    )?;
    child_command
        .current_dir(&command.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        child_command.process_group(0);
    }

    match child_command.spawn() {
        Ok(mut child) => {
            let child_id = child.id();
            let stdout_logger = child
                .stdout
                .take()
                .map(|stdout| spawn_pipe_logger(command.log_path.clone(), stdout));
            let stderr_logger = child
                .stderr
                .take()
                .map(|stderr| spawn_pipe_logger(command.log_path.clone(), stderr));
            state.status = "running".to_string();
            state.pid = Some(child_id);
            state.pgid = Some(child_id as i32);
            write_state(&command.state_path, &state)?;

            let status = child
                .wait()
                .context("failed waiting for supervised process")?;
            wait_for_pipe_logger(stdout_logger, "stdout", &command.log_path);
            wait_for_pipe_logger(stderr_logger, "stderr", &command.log_path);
            state.status = "exited".to_string();
            state.exit_code = Some(status.code().unwrap_or(-1));
            state.ended_at = Some(now_rfc3339());
            write_state(&command.state_path, &state)?;
            append_log_line(
                &command.log_path,
                &format!(
                    "\n[duckagent] process exited with code {}\n",
                    state.exit_code.unwrap_or(-1)
                ),
            )?;
            trigger_exit_event_if_needed(
                &mut state,
                &command.state_path,
                &command.messages_path,
                &command.log_path,
            )?;
        }
        Err(err) => {
            state.status = "failed".to_string();
            state.exit_code = Some(-1);
            state.ended_at = Some(now_rfc3339());
            write_state(&command.state_path, &state)?;
            append_log_line(
                &command.log_path,
                &format!("[duckagent] failed to start process: {err:#}\n"),
            )?;
            trigger_exit_event_if_needed(
                &mut state,
                &command.state_path,
                &command.messages_path,
                &command.log_path,
            )?;
        }
    }

    Ok(())
}

fn spawn_pipe_logger<R>(log_path: PathBuf, mut reader: R) -> thread::JoinHandle<Result<()>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            let read = reader.read(&mut buffer).with_context(|| {
                format!("failed to read child process pipe: {}", log_path.display())
            })?;
            if read == 0 {
                return Ok(());
            }
            append_log_bytes(&log_path, &buffer[..read])?;
        }
    })
}

fn wait_for_pipe_logger(
    handle: Option<thread::JoinHandle<Result<()>>>,
    label: &str,
    log_path: &Path,
) {
    let Some(handle) = handle else {
        return;
    };
    match handle.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = append_log_line(
                log_path,
                &format!("[duckagent] failed to capture child {label}: {error:#}\n"),
            );
        }
        Err(_) => {
            let _ = append_log_line(
                log_path,
                &format!("[duckagent] child {label} logger panicked\n"),
            );
        }
    }
}

fn start_process(
    session_manager: &SessionManager,
    session_id: &str,
    args: Value,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<String> {
    let input: ProcessStartArgs =
        serde_json::from_value(args).context("failed to parse process_start args")?;
    if input.command.trim().is_empty() {
        bail!("process_start command must be non-empty");
    }
    let cwd = resolve_cwd(input.cwd.as_deref())?;
    let sandbox = resolve_sandbox().context("failed to resolve sandbox for process_start")?;
    let process_id = new_process_id();
    let process_dir = process_dir(session_manager, session_id)?;
    let state_path = process_dir.join(format!("{process_id}.json"));
    let log_path = process_dir.join(format!("{process_id}.log"));
    let messages_path = session_manager
        .session_dir_path(session_id)
        .join("messages.jsonl");
    let now = now_rfc3339();
    let pty = input.pty.unwrap_or(false);
    let state = ProcessState {
        process_id: process_id.clone(),
        session_id: session_id.to_string(),
        command: input.command.clone(),
        cwd: cwd.to_string_lossy().to_string(),
        background: true,
        pty,
        status: "starting".to_string(),
        supervisor_pid: None,
        pid: None,
        pgid: None,
        started_at: now,
        ended_at: None,
        exit_code: None,
        watch: None,
    };
    write_state(&state_path, &state)?;

    if pty {
        return start_pty_process(
            process_id,
            session_id.to_string(),
            input.command,
            cwd,
            state_path,
            log_path,
            messages_path,
            state,
            approval_provider,
        );
    }

    let mut supervisor =
        Command::new(std::env::current_exe().context("failed to get current executable")?);
    let parent_explicit_env = parent_explicit_env_for_sandbox(&sandbox, approval_provider.clone())?;
    supervisor
        .arg("--sandbox")
        .arg(&sandbox.name)
        .arg("__process-supervisor")
        .arg("--process-id")
        .arg(&process_id)
        .arg("--session-id")
        .arg(session_id)
        .arg("--command")
        .arg(&input.command)
        .arg("--cwd")
        .arg(&cwd)
        .arg("--state-path")
        .arg(&state_path)
        .arg("--log-path")
        .arg(&log_path)
        .arg("--messages-path")
        .arg(&messages_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(marker) = crate::sandbox::runner::explicit_env_keys_marker(&parent_explicit_env) {
        supervisor.env(crate::sandbox::runner::EXPLICIT_ENV_KEYS_ENV, marker);
    }
    supervisor.envs(parent_explicit_env);
    #[cfg(unix)]
    {
        supervisor.process_group(0);
    }
    let _child = supervisor
        .spawn()
        .context("failed to start process supervisor")?;

    process_start_response_after_wait(&state_path, &log_path, &state, START_OBSERVE_MS)
}

fn start_pty_process(
    process_id: String,
    _session_id: String,
    command: String,
    cwd: PathBuf,
    state_path: PathBuf,
    log_path: PathBuf,
    messages_path: PathBuf,
    mut state: ProcessState,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<String> {
    append_log_line(
        &log_path,
        &format!("[duckagent] starting PTY process {process_id}\n"),
    )?;

    let pty_system = NativePtySystem::default();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open PTY")?;
    let sandbox = resolve_sandbox().context("failed to resolve sandbox for pty process")?;
    let mut cmd = shell_command_builder(&sandbox, &command, &cwd, approval_provider)?;
    cmd.cwd(&cwd);
    let child = pair
        .slave
        .spawn_command(cmd)
        .context("failed to spawn PTY command")?;
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .context("failed to clone PTY reader")?;
    let writer = pair
        .master
        .take_writer()
        .context("failed to take PTY writer")?;
    let killer = child.clone_killer();
    let pid = child.process_id();

    state.status = "running".to_string();
    state.pid = pid;
    state.pgid = pid.map(|value| value as i32);
    write_state(&state_path, &state)?;

    live_pty_registry()
        .lock()
        .expect("live PTY registry mutex poisoned")
        .insert(
            process_id.clone(),
            LivePty {
                writer: Arc::new(Mutex::new(writer)),
                killer: Arc::new(Mutex::new(killer)),
            },
        );

    let cursor = file_len(&log_path);
    spawn_pty_reader(
        process_id.clone(),
        state_path,
        log_path,
        messages_path,
        reader,
        child,
    );

    Ok(serde_json::to_string(&json!({
        "process_id": process_id,
        "status": "running",
        "pty": true,
        "exit_code": null,
        "output": "",
        "cursor": cursor,
        "truncated": false
    }))?)
}

fn process_start_response_after_wait(
    state_path: &Path,
    log_path: &Path,
    initial_state: &ProcessState,
    wait_ms: u64,
) -> Result<String> {
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    let mut state = initial_state.clone();
    loop {
        if let Ok(updated) = read_state(state_path) {
            state = updated;
        }
        if matches!(state.status.as_str(), "exited" | "failed") || Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let log_len = file_len(log_path);
    let (output, truncated) = read_log_tail_limited(log_path, log_len, DEFAULT_READ_LIMIT)?;
    Ok(serde_json::to_string(&json!({
        "process_id": state.process_id,
        "status": state.status,
        "exit_code": state.exit_code,
        "output": output,
        "cursor": log_len,
        "truncated": truncated
    }))?)
}

fn spawn_pty_reader(
    process_id: String,
    state_path: PathBuf,
    log_path: PathBuf,
    messages_path: PathBuf,
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn Child + Send + Sync>,
) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read_count) => {
                    let _ = append_log_bytes(&log_path, &buffer[..read_count]);
                }
                Err(_) => break,
            }
        }

        let exit_code = child
            .wait()
            .map(|status| status.exit_code() as i32)
            .unwrap_or(-1);
        let mut state = read_state(&state_path).unwrap_or_else(|_| ProcessState {
            process_id: process_id.clone(),
            session_id: String::new(),
            command: String::new(),
            cwd: String::new(),
            background: true,
            pty: true,
            status: "exited".to_string(),
            supervisor_pid: None,
            pid: child.process_id(),
            pgid: child.process_id().map(|value| value as i32),
            started_at: now_rfc3339(),
            ended_at: None,
            exit_code: None,
            watch: None,
        });
        state.status = "exited".to_string();
        state.exit_code = Some(exit_code);
        state.ended_at = Some(now_rfc3339());
        let _ = write_state(&state_path, &state);
        let _ = append_log_line(
            &log_path,
            &format!("\n[duckagent] PTY process exited with code {exit_code}\n"),
        );
        let _ = trigger_exit_event_if_needed(&mut state, &state_path, &messages_path, &log_path);
        live_pty_registry()
            .lock()
            .expect("live PTY registry mutex poisoned")
            .remove(&process_id);
    });
}

fn read_process(session_manager: &SessionManager, session_id: &str, args: Value) -> Result<String> {
    let input: ProcessReadArgs =
        serde_json::from_value(args).context("failed to parse process_read args")?;
    validate_process_id(&input.process_id)?;
    let state_path = state_path(session_manager, session_id, &input.process_id)?;
    let state = read_state(&state_path)?;
    let log_path = log_path(session_manager, session_id, &input.process_id)?;
    let log_len = file_len(&log_path);
    let limit = input.limit.unwrap_or(DEFAULT_READ_LIMIT).max(1);
    let mode = input.mode.as_deref().unwrap_or("tail").trim();
    let (output, truncated) = match mode {
        "tail" => read_log_tail_limited(&log_path, log_len, limit)?,
        "since" => read_log_since(&log_path, log_len, input.cursor.unwrap_or(0), limit)?,
        "full" => read_log_full_limited(&log_path, log_len, limit)?,
        "search" => search_log_file(&log_path, input.query.as_deref().unwrap_or_default(), limit)?,
        other => {
            return Ok(format!(
                "Tool error: unknown process_read mode `{other}`. Use tail, since, full, or search."
            ));
        }
    };

    Ok(serde_json::to_string(&json!({
        "process_id": state.process_id,
        "status": state.status,
        "exit_code": state.exit_code,
        "cursor": log_len,
        "output": output,
        "truncated": truncated
    }))?)
}

fn write_process(args: Value) -> Result<String> {
    let input: ProcessWriteArgs =
        serde_json::from_value(args).context("failed to parse process_write args")?;
    validate_process_id(&input.process_id)?;
    write_live_pty(&input.process_id, input.data.as_bytes())
}

fn write_live_pty(process_id: &str, bytes: &[u8]) -> Result<String> {
    let live = live_pty_registry()
        .lock()
        .expect("live PTY registry mutex poisoned")
        .get(process_id)
        .cloned();
    let Some(live) = live else {
        return Ok(format!(
            "Tool error: process_write requires a live pty process in the current duckagent runtime: {process_id}"
        ));
    };
    live.writer
        .lock()
        .expect("live PTY writer mutex poisoned")
        .write_all(bytes)
        .with_context(|| format!("failed to write to PTY process {process_id}"))?;
    Ok(serde_json::to_string(&json!({
        "process_id": process_id,
        "bytes_written": bytes.len()
    }))?)
}

fn stop_live_pty(process_id: &str) -> bool {
    let live = live_pty_registry()
        .lock()
        .expect("live PTY registry mutex poisoned")
        .get(process_id)
        .cloned();
    let Some(live) = live else {
        return false;
    };
    live.killer
        .lock()
        .expect("live PTY killer mutex poisoned")
        .kill()
        .is_ok()
}

fn list_processes(session_manager: &SessionManager, session_id: &str) -> Result<String> {
    let dir = process_dir(session_manager, session_id)?;
    let mut processes = Vec::new();
    for entry in fs::read_dir(&dir)
        .with_context(|| format!("failed to read process directory: {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        if let Ok(state) = read_state(&path) {
            processes.push(json!({
                "process_id": state.process_id,
                "status": state.status,
                "command": state.command,
                "cwd": state.cwd,
                "pty": state.pty,
                "pid": state.pid,
                "exit_code": state.exit_code,
                "started_at": state.started_at,
                "ended_at": state.ended_at,
                "watched": state.watch.as_ref().is_some_and(|watch| watch.triggered_at.is_none())
            }));
        }
    }
    Ok(serde_json::to_string(&json!({ "processes": processes }))?)
}

fn stop_process(session_manager: &SessionManager, session_id: &str, args: Value) -> Result<String> {
    let input: ProcessIdArgs =
        serde_json::from_value(args).context("failed to parse process_stop args")?;
    validate_process_id(&input.process_id)?;
    let state_path = state_path(session_manager, session_id, &input.process_id)?;
    let mut state = read_state(&state_path)?;
    if matches!(state.status.as_str(), "exited" | "failed") {
        return Ok(serde_json::to_string(&json!({
            "process_id": state.process_id,
            "status": state.status,
            "exit_code": state.exit_code
        }))?);
    }

    if stop_live_pty(&input.process_id) {
        // The PTY reader will update final status when the child exits.
    } else if let Some(pgid) = state.pgid {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(format!("-{pgid}"))
            .status();
    } else if let Some(pid) = state.pid.or(state.supervisor_pid) {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
    }
    state.status = "stopping".to_string();
    write_state(&state_path, &state)?;
    Ok(serde_json::to_string(&json!({
        "process_id": state.process_id,
        "status": state.status
    }))?)
}

fn watch_process(
    session_manager: &SessionManager,
    session_id: &str,
    args: Value,
) -> Result<String> {
    let input: ProcessWatchArgs =
        serde_json::from_value(args).context("failed to parse process_watch args")?;
    validate_process_id(&input.process_id)?;
    if input.event.trim() != "exit" {
        return Ok("Tool error: process_watch currently supports only event=\"exit\".".to_string());
    }
    if input.on_event.trim().is_empty() {
        bail!("process_watch on_event must be non-empty");
    }

    let state_path = state_path(session_manager, session_id, &input.process_id)?;
    let log_path = log_path(session_manager, session_id, &input.process_id)?;
    let messages_path = session_manager
        .session_dir_path(session_id)
        .join("messages.jsonl");
    let mut state = read_state(&state_path)?;
    state.watch = Some(ProcessWatch {
        event: "exit".to_string(),
        on_event: input.on_event,
        registered_at: now_rfc3339(),
        triggered_at: None,
    });

    if matches!(state.status.as_str(), "exited" | "failed") {
        trigger_exit_event_if_needed(&mut state, &state_path, &messages_path, &log_path)?;
        Ok(serde_json::to_string(&json!({
            "process_id": state.process_id,
            "watch": "already_triggered",
            "status": state.status,
            "exit_code": state.exit_code
        }))?)
    } else {
        write_state(&state_path, &state)?;
        Ok(serde_json::to_string(&json!({
            "process_id": state.process_id,
            "watch": "registered",
            "event": "exit"
        }))?)
    }
}

fn trigger_exit_event_if_needed(
    state: &mut ProcessState,
    state_path: &Path,
    messages_path: &Path,
    log_path: &Path,
) -> Result<()> {
    let Some(watch) = state.watch.as_mut() else {
        return Ok(());
    };
    if watch.event != "exit" || watch.triggered_at.is_some() {
        return Ok(());
    }

    let output_tail = read_log_tail(log_path, EVENT_TAIL_LIMIT).unwrap_or_default();
    let payload = ProcessEventPayload {
        id: new_uuid_string(),
        process_id: state.process_id.clone(),
        event: "exit".to_string(),
        exit_code: state.exit_code,
        output_tail: if output_tail.is_empty() {
            None
        } else {
            Some(output_tail)
        },
        on_event: watch.on_event.clone(),
    };
    append_process_event_line(messages_path, payload)?;
    watch.triggered_at = Some(now_rfc3339());
    write_state(state_path, state)
}

fn append_process_event_line(messages_path: &Path, payload: ProcessEventPayload) -> Result<()> {
    if let Some(parent) = messages_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create messages directory: {}", parent.display())
        })?;
    }
    let line = SessionLine {
        timestamp: now_rfc3339(),
        entry: SessionEntry::ProcessEvent(payload),
    };
    let serialized = serde_json::to_string(&line).context("failed to serialize process event")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(messages_path)
        .with_context(|| {
            format!(
                "failed to append process event: {}",
                messages_path.display()
            )
        })?;
    file.write_all(serialized.as_bytes())
        .context("failed to write process event")?;
    file.write_all(b"\n")
        .context("failed to write process event newline")?;
    Ok(())
}

fn process_dir(session_manager: &SessionManager, session_id: &str) -> Result<PathBuf> {
    let dir = session_manager
        .runtime_dir_path(session_id)?
        .join("processes");
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create process directory: {}", dir.display()))?;
    Ok(dir)
}

fn state_path(
    session_manager: &SessionManager,
    session_id: &str,
    process_id: &str,
) -> Result<PathBuf> {
    validate_process_id(process_id)?;
    Ok(process_dir(session_manager, session_id)?.join(format!("{process_id}.json")))
}

fn log_path(
    session_manager: &SessionManager,
    session_id: &str,
    process_id: &str,
) -> Result<PathBuf> {
    validate_process_id(process_id)?;
    Ok(process_dir(session_manager, session_id)?.join(format!("{process_id}.log")))
}

fn read_state(path: &Path) -> Result<ProcessState> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read process state: {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("failed to parse process state: {}", path.display()))
}

fn write_state(path: &Path, state: &ProcessState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create process state directory: {}",
                parent.display()
            )
        })?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let serialized =
        serde_json::to_vec_pretty(state).context("failed to serialize process state")?;
    fs::write(&tmp_path, serialized).with_context(|| {
        format!(
            "failed to write process state temp file: {}",
            tmp_path.display()
        )
    })?;
    fs::rename(&tmp_path, path)
        .with_context(|| format!("failed to replace process state: {}", path.display()))
}

fn append_log_line(path: &Path, line: &str) -> Result<()> {
    append_log_bytes(path, line.as_bytes())
}

fn append_log_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create process log directory: {}",
                parent.display()
            )
        })?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open process log: {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("failed to append process log: {}", path.display()))
}

fn live_pty_registry() -> &'static Mutex<HashMap<String, LivePty>> {
    LIVE_PTYS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn shell_command_builder(
    sandbox: &crate::sandbox::config::ResolvedSandbox,
    command: &str,
    cwd: &Path,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<CommandBuilder> {
    if bypass_sandbox_for_tests(sandbox) {
        let shell = default_shell_invocation();
        let args = shell.args_with_command(command);
        let mut cmd = CommandBuilder::new(shell.program);
        for arg in args {
            cmd.arg(arg);
        }
        return Ok(cmd);
    }

    let exe = std::env::current_exe().context("failed to get current executable")?;
    let proxy_env = parent_explicit_env_for_sandbox(sandbox, approval_provider)?;
    let mut cmd = CommandBuilder::new(exe);
    cmd.arg("--sandbox");
    cmd.arg(&sandbox.name);
    cmd.arg("__sandbox-run");
    cmd.arg("--cwd");
    cmd.arg(cwd);
    for (key, value) in proxy_env {
        cmd.arg("--env");
        cmd.arg(format!("{key}={value}"));
    }
    cmd.arg("--");
    let shell = default_shell_invocation();
    let args = shell.args_with_command(command);
    cmd.arg(shell.program);
    for arg in args {
        cmd.arg(arg);
    }
    Ok(cmd)
}

#[cfg(windows)]
fn default_shell_invocation() -> ShellInvocation {
    let program = std::env::var_os("COMSPEC")
        .and_then(non_empty_os_string)
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "cmd.exe".to_string());
    ShellInvocation {
        program,
        args: vec!["/d".to_string(), "/C".to_string()],
    }
}

#[cfg(windows)]
fn non_empty_os_string(value: OsString) -> Option<OsString> {
    (!value.to_string_lossy().trim().is_empty()).then_some(value)
}

#[cfg(not(windows))]
fn default_shell_invocation() -> ShellInvocation {
    let mut candidates = Vec::new();
    if let Some(shell) = compatible_shell_from_env() {
        candidates.push(shell);
    }
    for name in ["zsh", "bash", "sh"] {
        if let Some(path) = find_executable_in_path(name) {
            candidates.push(path);
        }
    }
    candidates.extend(
        [
            "/bin/zsh",
            "/usr/bin/zsh",
            "/bin/bash",
            "/usr/bin/bash",
            "/bin/sh",
            "/usr/bin/sh",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    unix_shell_invocation_from_candidates(candidates)
}

#[cfg(not(windows))]
fn compatible_shell_from_env() -> Option<PathBuf> {
    let value = std::env::var_os("SHELL")?;
    let shell = PathBuf::from(value);
    if !is_supported_unix_shell(&shell) {
        return None;
    }
    if shell.is_absolute() {
        is_executable_file(&shell).then_some(shell)
    } else {
        shell.to_str().and_then(find_executable_in_path)
    }
}

#[cfg(not(windows))]
fn unix_shell_invocation_from_candidates(
    candidates: impl IntoIterator<Item = PathBuf>,
) -> ShellInvocation {
    let program = candidates
        .into_iter()
        .find(|path| is_supported_unix_shell(path) && is_executable_file(path))
        .unwrap_or_else(|| PathBuf::from("/bin/sh"));
    ShellInvocation {
        args: unix_shell_args(&program),
        program: program.to_string_lossy().to_string(),
    }
}

#[cfg(not(windows))]
fn unix_shell_args(program: &Path) -> Vec<String> {
    if unix_shell_supports_login_flag(program) {
        vec!["-lc".to_string()]
    } else {
        vec!["-c".to_string()]
    }
}

#[cfg(not(windows))]
fn unix_shell_supports_login_flag(program: &Path) -> bool {
    matches!(unix_shell_name(program).as_deref(), Some("bash" | "zsh"))
}

#[cfg(not(windows))]
fn is_supported_unix_shell(program: &Path) -> bool {
    matches!(
        unix_shell_name(program).as_deref(),
        Some("bash" | "zsh" | "sh" | "dash")
    )
}

#[cfg(not(windows))]
fn unix_shell_name(program: &Path) -> Option<String> {
    program
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::to_string)
}

#[cfg(not(windows))]
fn find_executable_in_path(name: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    std::env::split_paths(&path_env)
        .map(|dir| dir.join(name))
        .find(|path| is_executable_file(path))
}

#[cfg(not(windows))]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn parent_explicit_env_for_sandbox(
    sandbox: &crate::sandbox::config::ResolvedSandbox,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<BTreeMap<String, String>> {
    crate::sandbox::runner::parent_explicit_env_for_sandbox(sandbox, approval_provider)
}

fn inherited_proxy_environment() -> BTreeMap<String, String> {
    crate::sandbox::runner::explicit_env_from_current_process()
}

fn resolve_cwd(cwd: Option<&str>) -> Result<PathBuf> {
    let base = std::env::current_dir().context("failed to get current directory")?;
    let path = cwd
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or(base.clone());
    let candidate = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve process cwd: {}", candidate.display()))
}

fn validate_process_id(process_id: &str) -> Result<()> {
    let valid = process_id.starts_with("proc_")
        && process_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    if !valid {
        bail!("invalid process_id: {process_id}");
    }
    Ok(())
}

fn read_log_tail(path: &Path, limit: usize) -> Result<String> {
    let log_len = file_len(path);
    Ok(read_log_tail_limited(path, log_len, limit)?.0)
}

fn file_len(path: &Path) -> usize {
    fs::metadata(path)
        .map(|metadata| usize::try_from(metadata.len()).unwrap_or(usize::MAX))
        .unwrap_or(0)
}

fn read_log_tail_limited(path: &Path, log_len: usize, limit: usize) -> Result<(String, bool)> {
    if log_len <= limit {
        return Ok((read_log_range(path, 0, log_len)?, false));
    }
    let start = log_len.saturating_sub(limit);
    Ok((read_log_range(path, start, limit)?, true))
}

fn read_log_since(
    path: &Path,
    log_len: usize,
    cursor: usize,
    limit: usize,
) -> Result<(String, bool)> {
    if cursor >= log_len {
        return Ok((String::new(), false));
    }
    let end = log_len.min(cursor.saturating_add(limit));
    Ok((read_log_range(path, cursor, end - cursor)?, end < log_len))
}

fn read_log_full_limited(path: &Path, log_len: usize, limit: usize) -> Result<(String, bool)> {
    let read_len = log_len.min(limit);
    Ok((read_log_range(path, 0, read_len)?, log_len > limit))
}

fn read_log_range(path: &Path, start: usize, len: usize) -> Result<String> {
    if len == 0 {
        return Ok(String::new());
    }
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to open process log: {}", path.display()));
        }
    };
    file.seek(SeekFrom::Start(start as u64))
        .with_context(|| format!("failed to seek process log: {}", path.display()))?;
    let mut buffer = Vec::with_capacity(len.min(64 * 1024));
    file.take(len as u64)
        .read_to_end(&mut buffer)
        .with_context(|| format!("failed to read process log: {}", path.display()))?;
    Ok(String::from_utf8_lossy(&buffer).to_string())
}

fn search_log_file(path: &Path, query: &str, limit: usize) -> Result<(String, bool)> {
    if query.trim().is_empty() {
        return Ok((
            "Tool error: process_read mode=search requires args.query.".to_string(),
            false,
        ));
    }
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((String::new(), false));
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to open process log: {}", path.display()));
        }
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut output = String::new();
    let mut truncated = false;

    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .with_context(|| format!("failed to read process log: {}", path.display()))?;
        if read == 0 {
            break;
        }
        let line_text = String::from_utf8_lossy(&line);
        let line_text = line_text.trim_end_matches('\n').trim_end_matches('\r');
        if !line_text.contains(query) {
            continue;
        }
        if output.len() + line_text.len() + 1 > limit {
            truncated = true;
            break;
        }
        output.push_str(line_text);
        output.push('\n');
    }
    Ok((output, truncated))
}

fn new_process_id() -> String {
    format!("proc_{}", Uuid::now_v7().simple())
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
    use crate::session::SessionMessage;
    use tempfile::tempdir;

    fn manager() -> Result<(SessionManager, String)> {
        let dir = tempdir().context("failed to create tempdir")?;
        let manager = SessionManager::new(dir.keep())?;
        let session_id = manager.create_session(Some("process"), "system")?;
        Ok((manager, session_id))
    }

    #[cfg(not(windows))]
    fn make_executable(path: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, "#!/bin/sh\n")?;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
        Ok(())
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_shell_invocation_uses_plain_c_for_sh_fallback() -> Result<()> {
        let dir = tempdir().context("failed to create tempdir")?;
        let sh = dir.path().join("sh");
        make_executable(&sh)?;

        let invocation =
            unix_shell_invocation_from_candidates([dir.path().join("missing-zsh"), sh.clone()]);

        assert_eq!(invocation.program, sh.to_string_lossy().to_string());
        assert_eq!(invocation.args, vec!["-c".to_string()]);
        Ok(())
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_shell_invocation_uses_login_c_for_bash_and_zsh() {
        assert_eq!(
            unix_shell_args(Path::new("/bin/bash")),
            vec!["-lc".to_string()]
        );
        assert_eq!(
            unix_shell_args(Path::new("/bin/zsh")),
            vec!["-lc".to_string()]
        );
        assert_eq!(
            unix_shell_args(Path::new("/bin/sh")),
            vec!["-c".to_string()]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_shell_invocation_uses_cmd_without_autorun() {
        let invocation = default_shell_invocation();
        assert!(!invocation.program.trim().is_empty());
        assert_eq!(invocation.args, vec!["/d".to_string(), "/C".to_string()]);
    }

    #[test]
    fn pty_process_supports_write_and_read_in_current_runtime() -> Result<()> {
        if cfg!(windows) {
            return Ok(());
        }
        let _sandbox = crate::sandbox::config::TestSandboxOverrideGuard::new("danger");
        let (manager, session_id) = manager()?;
        let command = "read line; echo got:$line";
        let started = execute_process_action(
            &manager,
            &session_id,
            json!({
                "action": "start",
                "command": command,
                "pty": true
            }),
            None,
        )?;
        let started: Value = serde_json::from_str(&started)?;
        let process_id = started
            .get("process_id")
            .and_then(Value::as_str)
            .context("missing process_id")?
            .to_string();
        let write = execute_process_action(
            &manager,
            &session_id,
            json!({
                "action": "write",
                "process_id": process_id,
                "data": "hello\n"
            }),
            None,
        )?;
        assert!(write.contains("bytes_written"));

        let mut last_read = String::new();
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let read = execute_process_action(
                &manager,
                &session_id,
                json!({
                    "action": "read",
                    "process_id": process_id,
                    "mode": "tail",
                    "limit": 4000
                }),
                None,
            )?;
            if read.contains("got:hello") {
                return Ok(());
            }
            last_read = read;
        }
        bail!("PTY process output did not include expected response. Last read: {last_read}");
    }

    #[test]
    fn pty_process_can_drive_vim_write_save_and_exit() -> Result<()> {
        if cfg!(windows) || std::env::var_os("CI").is_some() {
            return Ok(());
        }
        let _sandbox = crate::sandbox::config::TestSandboxOverrideGuard::new("danger");
        let has_vim = Command::new("vim")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !has_vim {
            return Ok(());
        }

        let (manager, session_id) = manager()?;
        let workdir = tempdir().context("failed to create vim workdir")?;
        let started = execute_process_action(
            &manager,
            &session_id,
            json!({
                "action": "start",
                "command": "vim -Nu NONE -n -i NONE hello.txt",
                "cwd": workdir.path(),
                "pty": true
            }),
            None,
        )?;
        let started: Value = serde_json::from_str(&started)?;
        let process_id = started
            .get("process_id")
            .and_then(Value::as_str)
            .context("missing process_id")?
            .to_string();

        std::thread::sleep(std::time::Duration::from_millis(500));
        let write = execute_process_action(
            &manager,
            &session_id,
            json!({
                "action": "write",
                "process_id": process_id,
                "data": "ihello world\u{1b}:wq!\r"
            }),
            None,
        )?;
        assert!(write.contains("bytes_written"));

        let hello_path = workdir.path().join("hello.txt");
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let content = fs::read_to_string(&hello_path).unwrap_or_default();
            if content.trim_end_matches(['\r', '\n']) == "hello world" {
                let read = execute_process_action(
                    &manager,
                    &session_id,
                    json!({
                        "action": "read",
                        "process_id": process_id,
                        "mode": "tail",
                        "limit": 4000
                    }),
                    None,
                )?;
                assert!(read.contains("\"status\":\"exited\""));
                return Ok(());
            }
        }

        let _ = execute_process_action(
            &manager,
            &session_id,
            json!({
                "action": "stop",
                "process_id": process_id
            }),
            None,
        );
        bail!("vim PTY process did not write expected hello.txt content");
    }

    #[test]
    fn process_read_supports_cursor_and_query_modes() -> Result<()> {
        let (manager, session_id) = manager()?;
        let dir = process_dir(&manager, &session_id)?;
        let process_id = "proc_read_modes".to_string();
        let state_path = dir.join(format!("{process_id}.json"));
        let log_path = dir.join(format!("{process_id}.log"));
        let log = "alpha\nbeta\nERROR one\ngamma\nERROR two\n";
        fs::write(&log_path, log)?;
        write_state(
            &state_path,
            &ProcessState {
                process_id: process_id.clone(),
                session_id: session_id.clone(),
                command: "fixture".to_string(),
                cwd: ".".to_string(),
                background: true,
                pty: false,
                status: "running".to_string(),
                supervisor_pid: None,
                pid: Some(123),
                pgid: Some(123),
                started_at: now_rfc3339(),
                ended_at: None,
                exit_code: None,
                watch: None,
            },
        )?;

        let since: Value = serde_json::from_str(&execute_process_action(
            &manager,
            &session_id,
            json!({
                "action": "read",
                "process_id": process_id,
                "mode": "since",
                "cursor": 6,
                "limit": 5
            }),
            None,
        )?)?;
        assert_eq!(since["output"], json!("beta\n"));
        assert_eq!(since["truncated"], json!(true));
        assert_eq!(since["cursor"], json!(log.len()));
        assert!(since.get("output_ref").is_none());
        assert!(since.get("log_ref").is_none());

        let search: Value = serde_json::from_str(&execute_process_action(
            &manager,
            &session_id,
            json!({
                "action": "read",
                "process_id": "proc_read_modes",
                "mode": "search",
                "query": "ERROR",
                "limit": 100
            }),
            None,
        )?)?;
        assert_eq!(search["output"], json!("ERROR one\nERROR two\n"));
        assert_eq!(search["truncated"], json!(false));
        Ok(())
    }

    #[test]
    fn process_start_response_reports_output_without_refs() -> Result<()> {
        let dir = tempdir().context("failed to create tempdir")?;
        let process_id = "proc_start_response".to_string();
        let state_path = dir.path().join(format!("{process_id}.json"));
        let log_path = dir.path().join(format!("{process_id}.log"));
        fs::write(&log_path, "hello\n")?;
        let state = ProcessState {
            process_id,
            session_id: "session".to_string(),
            command: "printf hello".to_string(),
            cwd: ".".to_string(),
            background: true,
            pty: false,
            status: "exited".to_string(),
            supervisor_pid: None,
            pid: Some(123),
            pgid: Some(123),
            started_at: now_rfc3339(),
            ended_at: Some(now_rfc3339()),
            exit_code: Some(0),
            watch: None,
        };
        write_state(&state_path, &state)?;

        let output: Value = serde_json::from_str(&process_start_response_after_wait(
            &state_path,
            &log_path,
            &state,
            0,
        )?)?;
        assert_eq!(output["status"], json!("exited"));
        assert_eq!(output["exit_code"], json!(0));
        assert_eq!(output["output"], json!("hello\n"));
        assert_eq!(output["cursor"], json!(6));
        assert_eq!(output["truncated"], json!(false));
        assert!(output.get("output_ref").is_none());
        assert!(output.get("log_ref").is_none());
        Ok(())
    }

    #[test]
    fn pipe_logger_appends_child_output_to_supervisor_log() -> Result<()> {
        let dir = tempdir().context("failed to create tempdir")?;
        let log_path = dir.path().join("process.log");
        let reader = std::io::Cursor::new(b"hello from child\n".to_vec());
        let logger = spawn_pipe_logger(log_path.clone(), reader);

        wait_for_pipe_logger(Some(logger), "stdout", &log_path);

        assert_eq!(fs::read_to_string(log_path)?, "hello from child\n");
        Ok(())
    }

    #[test]
    fn process_watch_appends_event_for_exited_process() -> Result<()> {
        let (manager, session_id) = manager()?;
        let dir = process_dir(&manager, &session_id)?;
        let process_id = "proc_test".to_string();
        let state_path = dir.join(format!("{process_id}.json"));
        let log_path = dir.join(format!("{process_id}.log"));
        fs::write(&log_path, "error: failed\n")?;
        write_state(
            &state_path,
            &ProcessState {
                process_id: process_id.clone(),
                session_id: session_id.clone(),
                command: "false".to_string(),
                cwd: ".".to_string(),
                background: true,
                pty: false,
                status: "exited".to_string(),
                supervisor_pid: None,
                pid: Some(123),
                pgid: Some(123),
                started_at: now_rfc3339(),
                ended_at: Some(now_rfc3339()),
                exit_code: Some(1),
                watch: None,
            },
        )?;

        let output = execute_process_action(
            &manager,
            &session_id,
            json!({
                "action": "watch",
                "process_id": process_id,
                "event": "exit",
                "on_event": "Read output and continue processing"
            }),
            None,
        )?;
        assert!(output.contains("already_triggered"));
        let visible = manager.get_all_messages(&session_id)?;
        assert!(
            visible
                .iter()
                .filter_map(SessionMessage::text_preview)
                .any(|text| text.contains("[PROCESS EVENT]") && text.contains("error: failed"))
        );
        assert!(
            visible
                .iter()
                .filter_map(SessionMessage::text_preview)
                .filter(|text| text.contains("[PROCESS EVENT]"))
                .all(|text| !text.contains("output_ref"))
        );
        Ok(())
    }
}
