mod access;
mod channels;
mod config;
mod outbox;
mod pairing;
mod service;
mod setup;
mod types;

use crate::agent::{AgentEvent, AgentRuntime, GatewayUserMessageMetadata, SubmittedUserMessage};
use crate::approval::{ApprovalDecision, ApprovalProvider, ApprovalResponse, RuleHit};
use crate::client::ModelClient;
use crate::cron::service::{
    CronJobExecutor, CronRunContext, CronService, CronSubmitResult, build_scheduled_event_prompt,
};
use crate::cron::store::CronStore;
use crate::cron::types::{CronJob, CronRunStatus, CronTarget};
use crate::provider::{refresh_models_dev_cache_background, resolve_runtime_provider};
use crate::session::{
    ContentItem, SessionEntry, SessionLine, SessionManager, SessionMessage, SessionRole,
};
use crate::session_control::{
    SessionControlCommand, SessionListItem, filter_session_items, format_rewind_list,
    format_session_list, paginate_session_items, parse_session_control_command,
    session_meta_to_list_item,
};
use crate::setup::{
    is_runtime_setup_cancelled, run_initial_runtime_setup,
    run_windows_sandbox_setup_after_provider_if_needed,
};
use crate::tools::MessageType;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use clap::{Args, Subcommand};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use uuid::Uuid;

use access::{GatewayAccessDecision, evaluate_gateway_access, send_access_notice};
use pairing::GatewayPairingStore;

pub(in crate::gateway) use outbox::GatewayOutbox;
pub use types::{
    AttachmentRef, ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, GatewaySessionKey,
    InboundAttachmentInput, InboundMessage, InboundMessageInput, OutboundMessage,
    StreamMessageHandle, TypingEvent,
};

const TYPING_REFRESH_INTERVAL: Duration = Duration::from_secs(4);
const STREAM_CONTINUED_SUFFIX: &str = "\n\n[continued below]";
const GATEWAY_THINKING_PLACEHOLDER: &str = "Thinking...";

#[derive(Debug, Args)]
pub struct GatewayCommand {
    #[command(subcommand)]
    command: GatewaySubcommand,
}

#[derive(Debug, Subcommand)]
enum GatewaySubcommand {
    /// Manage configured gateway channels.
    Channels,
    Service {
        #[command(subcommand)]
        command: GatewayServiceCommand,
    },
    #[command(name = "__service-run", hide = true)]
    ServiceRun,
    #[command(hide = true)]
    Pairing {
        #[command(subcommand)]
        command: GatewayPairingCommand,
    },
}

#[derive(Debug, Subcommand)]
enum GatewayPairingCommand {
    List(GatewayPairingListArgs),
    Approve(GatewayPairingApproveArgs),
    Revoke(GatewayPairingRevokeArgs),
}

#[derive(Debug, Subcommand)]
enum GatewayServiceCommand {
    Start,
    Stop,
    Log,
}

#[derive(Debug, Args)]
struct GatewayPairingListArgs {
    #[arg(long)]
    channel: Option<String>,
}

#[derive(Debug, Args)]
struct GatewayPairingApproveArgs {
    code: String,
    #[arg(long)]
    channel: Option<String>,
}

#[derive(Debug, Args)]
struct GatewayPairingRevokeArgs {
    user_id: String,
    #[arg(long)]
    channel: Option<String>,
}

#[derive(Clone)]
struct GatewayRuntime {
    agent: AgentRuntime,
    session_manager: SessionManager,
    system_prompt: &'static str,
    sessions: GatewaySessionStore,
    attachments: GatewayAttachmentStore,
    adapters: Arc<HashMap<String, Arc<dyn ChannelAdapter>>>,
    channel_configs: Arc<HashMap<String, config::GatewayChannelConfig>>,
    outbox: GatewayOutbox,
    api_server_key: Option<String>,
    pairing: GatewayPairingStore,
    approvals: PendingApprovals,
    routes_by_session: Arc<Mutex<HashMap<String, GatewayRoute>>>,
    typing_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    streams: GatewayStreamStateStore,
    session_lists: Arc<Mutex<HashMap<GatewaySessionKey, Vec<SessionListItem>>>>,
    cron: CronService,
}

#[derive(Clone, Default)]
struct GatewayStreamStateStore {
    inner: Arc<Mutex<HashMap<String, GatewayStreamState>>>,
}

struct GatewayStreamState {
    buffer: String,
    last_sent: String,
    last_flush: Option<std::time::Instant>,
    handle: Option<StreamMessageHandle>,
    delivered: bool,
    disabled: bool,
    update_count: usize,
    update_blocked: bool,
}

impl Default for GatewayStreamState {
    fn default() -> Self {
        Self {
            buffer: String::new(),
            last_sent: String::new(),
            last_flush: None,
            handle: None,
            delivered: false,
            disabled: false,
            update_count: 0,
            update_blocked: false,
        }
    }
}

#[derive(Clone)]
struct PendingApprovals {
    inner: Arc<Mutex<HashMap<String, PendingApprovalEntry>>>,
}

struct PendingApprovalEntry {
    response_tx: mpsc::Sender<ApprovalResponse>,
    route: GatewayRoute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ApprovalResolveStatus {
    Resolved,
    NotFound,
}

#[derive(Debug, Clone)]
struct ApprovalResolveResult {
    status: ApprovalResolveStatus,
    id: Option<String>,
    decision: ApprovalDecision,
    pending_count: usize,
}

impl PendingApprovals {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn insert(&self, id: String, route: GatewayRoute, response_tx: mpsc::Sender<ApprovalResponse>) {
        self.inner
            .lock()
            .expect("gateway approvals mutex poisoned")
            .insert(id, PendingApprovalEntry { response_tx, route });
    }

    fn resolve(&self, id: &str, decision: ApprovalDecision) -> bool {
        let entry = self
            .inner
            .lock()
            .expect("gateway approvals mutex poisoned")
            .remove(id);
        entry.is_some_and(|entry| {
            entry
                .response_tx
                .send(ApprovalResponse { decision })
                .is_ok()
        })
    }

    fn resolve_for_key(
        &self,
        key: &GatewaySessionKey,
        decision: ApprovalDecision,
        resolve_all: bool,
    ) -> ApprovalResolveResult {
        let matches = {
            let guard = self.inner.lock().expect("gateway approvals mutex poisoned");
            guard
                .iter()
                .filter(|(_, entry)| approval_route_matches(&entry.route.key, key))
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        };
        if matches.is_empty() {
            return ApprovalResolveResult {
                status: ApprovalResolveStatus::NotFound,
                id: None,
                decision,
                pending_count: 0,
            };
        }
        let targets = if resolve_all {
            matches.clone()
        } else {
            vec![matches[0].clone()]
        };
        let mut resolved_count = 0usize;
        for id in &targets {
            if self.resolve(id, decision) {
                resolved_count += 1;
            }
        }
        ApprovalResolveResult {
            status: if resolved_count > 0 {
                ApprovalResolveStatus::Resolved
            } else {
                ApprovalResolveStatus::NotFound
            },
            id: (resolved_count == 1).then(|| targets[0].clone()),
            decision,
            pending_count: resolved_count,
        }
    }
}

fn approval_route_matches(pending: &GatewaySessionKey, incoming: &GatewaySessionKey) -> bool {
    pending.channel == incoming.channel
        && pending.conversation_id == incoming.conversation_id
        && (pending.thread_id == incoming.thread_id
            || pending.thread_id.is_none()
            || incoming.thread_id.is_none())
}

struct GatewayApprovalProvider {
    route: GatewayRoute,
    adapter: Arc<dyn ChannelAdapter>,
    approvals: PendingApprovals,
    typing_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

impl ApprovalProvider for GatewayApprovalProvider {
    fn request_approval(
        &self,
        command: &str,
        rule_hits: &[RuleHit],
        options: [ApprovalDecision; 4],
    ) -> Option<ApprovalResponse> {
        let id = format!("appr_{}", Uuid::now_v7().simple());
        let (response_tx, response_rx) = mpsc::channel();
        self.approvals
            .insert(id.clone(), self.route.clone(), response_tx);
        let option_labels = options.iter().map(approval_label).collect::<Vec<_>>();
        let prompt = GatewayApprovalPrompt {
            id: id.clone(),
            command: command.to_string(),
            options: option_labels,
            rule_hits: rule_hits
                .iter()
                .map(|hit| format!("{}: {}", hit.rule_id, hit.description))
                .collect(),
            message: render_approval_prompt_message(command, &id),
        };
        stop_typing_refresh(
            self.adapter.clone(),
            self.typing_flags.clone(),
            &self.route,
            "approval_required",
        );
        let _ = self.adapter.send_approval_prompt(&self.route, prompt);
        let response = response_rx.recv().ok();
        if response
            .as_ref()
            .is_some_and(|value| value.decision.approved())
        {
            start_typing_refresh(
                self.adapter.clone(),
                self.typing_flags.clone(),
                self.route.clone(),
                "approval_resolved",
            );
        }
        response
    }
}

struct CronNoRouteApprovalProvider;

impl ApprovalProvider for CronNoRouteApprovalProvider {
    fn request_approval(
        &self,
        _command: &str,
        _rule_hits: &[RuleHit],
        _options: [ApprovalDecision; 4],
    ) -> Option<ApprovalResponse> {
        Some(ApprovalResponse {
            decision: ApprovalDecision::Forbidden,
        })
    }
}

pub fn run(
    command: GatewayCommand,
    session_manager: SessionManager,
    system_prompt: &'static str,
) -> Result<()> {
    match command.command {
        GatewaySubcommand::Service { command } => {
            run_service_command(command, session_manager, system_prompt)
        }
        GatewaySubcommand::Channels => {
            setup::run_gateway_channels_manager()?;
            Ok(())
        }
        GatewaySubcommand::ServiceRun => serve_gateway_service(session_manager, system_prompt),
        GatewaySubcommand::Pairing { command } => run_pairing_command(command),
    }
}

fn run_service_command(
    command: GatewayServiceCommand,
    _session_manager: SessionManager,
    _system_prompt: &'static str,
) -> Result<()> {
    match command {
        GatewayServiceCommand::Start => {
            if prepare_gateway_service_start()? {
                restart_gateway_service()?;
            }
            Ok(())
        }
        GatewayServiceCommand::Stop => stop_and_uninstall_gateway_service(),
        GatewayServiceCommand::Log => run_gateway_service_log(_session_manager),
    }
}

struct GatewayLogTail {
    session_id: String,
    label: String,
    path: PathBuf,
    offset: u64,
}

fn run_gateway_service_log(session_manager: SessionManager) -> Result<()> {
    let map_path = default_gateway_sessions_path()?;
    let mut map_offset = 0u64;
    let mut tails = HashMap::<String, GatewayLogTail>::new();

    let records = read_gateway_session_records_from(&map_path, &mut map_offset)?;
    for record in records {
        upsert_gateway_log_tail(&mut tails, &session_manager, record, true)?;
    }

    println!("Watching gateway messages. Press Ctrl+C to stop.");
    if tails.is_empty() {
        println!("No gateway sessions found yet; waiting for new messages...");
    }

    loop {
        for record in read_gateway_session_records_from(&map_path, &mut map_offset)? {
            upsert_gateway_log_tail(&mut tails, &session_manager, record, false)?;
        }

        for tail in tails.values_mut() {
            for line in read_session_lines_from_tail(tail)? {
                if let Some(display) = format_gateway_log_line(tail, line) {
                    println!("{display}");
                }
            }
        }
        std::io::stdout().flush().ok();
        thread::sleep(Duration::from_millis(700));
    }
}

fn read_gateway_session_records_from(
    path: &Path,
    offset: &mut u64,
) -> Result<Vec<GatewaySessionRecord>> {
    if !path.exists() {
        *offset = 0;
        return Ok(Vec::new());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed to open gateway session map: {}", path.display()))?;
    let len = file.metadata()?.len();
    if *offset > len {
        *offset = 0;
    }
    file.seek(SeekFrom::Start(*offset))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    *offset = file.stream_position()?;

    let mut records = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let record = serde_json::from_str::<GatewaySessionRecord>(line)
            .with_context(|| format!("failed to parse gateway session record: {line}"))?;
        records.push(record);
    }
    Ok(records)
}

fn upsert_gateway_log_tail(
    tails: &mut HashMap<String, GatewayLogTail>,
    session_manager: &SessionManager,
    record: GatewaySessionRecord,
    start_at_end: bool,
) -> Result<()> {
    let label = format_gateway_session_key(&record.key);
    if let Some(tail) = tails.get_mut(&record.session_id) {
        if !tail.label.split(" | ").any(|part| part == label) {
            tail.label.push_str(" | ");
            tail.label.push_str(&label);
        }
        return Ok(());
    }

    let path = gateway_session_messages_path(session_manager, &record.session_id);
    let offset = if start_at_end && path.exists() {
        path.metadata()?.len()
    } else {
        0
    };
    tails.insert(
        record.session_id.clone(),
        GatewayLogTail {
            session_id: record.session_id,
            label,
            path,
            offset,
        },
    );
    Ok(())
}

fn gateway_session_messages_path(session_manager: &SessionManager, session_id: &str) -> PathBuf {
    let session_dir = session_manager.session_dir_path(session_id);
    let modern = session_dir.join("messages.jsonl");
    let legacy = session_dir.with_file_name(format!("{session_id}.jsonl"));
    if modern.exists() || !legacy.exists() {
        modern
    } else {
        legacy
    }
}

fn read_session_lines_from_tail(tail: &mut GatewayLogTail) -> Result<Vec<SessionLine>> {
    if !tail.path.exists() {
        tail.offset = 0;
        return Ok(Vec::new());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .open(&tail.path)
        .with_context(|| format!("failed to open session log: {}", tail.path.display()))?;
    let len = file.metadata()?.len();
    if tail.offset > len {
        tail.offset = 0;
    }
    file.seek(SeekFrom::Start(tail.offset))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    tail.offset = file.stream_position()?;

    let mut lines = Vec::new();
    for raw in text.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<SessionLine>(raw) {
            Ok(line) => lines.push(line),
            Err(error) => eprintln!(
                "duckagent gateway service log skipped malformed session line in {}: {error}",
                tail.session_id
            ),
        }
    }
    Ok(lines)
}

fn format_gateway_log_line(tail: &GatewayLogTail, line: SessionLine) -> Option<String> {
    let SessionEntry::ResponseItem(SessionMessage::Message {
        scope,
        role,
        content,
        ..
    }) = line.entry
    else {
        return None;
    };
    if !scope.is_main() || role == SessionRole::System {
        return None;
    }

    let text = join_gateway_log_content(&content);
    let text = match role {
        SessionRole::User => extract_gateway_user_text(&text).unwrap_or(text),
        SessionRole::Assistant => text,
        SessionRole::System => return None,
    };
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let role = match role {
        SessionRole::User => "user",
        SessionRole::Assistant => "assistant",
        SessionRole::System => return None,
    };
    Some(format!(
        "[{}] [{}] {}: {}",
        line.timestamp, tail.label, role, text
    ))
}

fn join_gateway_log_content(content: &[ContentItem]) -> String {
    content
        .iter()
        .map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn extract_gateway_user_text(text: &str) -> Option<String> {
    let marker = "\ntext:\n";
    let start = text.find(marker)? + marker.len();
    let remainder = &text[start..];
    let end = remainder
        .find("\n\n[User Attachment]")
        .unwrap_or(remainder.len());
    Some(remainder[..end].trim().to_string())
}

fn format_gateway_session_key(key: &GatewaySessionKey) -> String {
    match key.thread_id.as_deref() {
        Some(thread_id) if !thread_id.is_empty() => {
            format!("{}:{}#{}", key.channel, key.conversation_id, thread_id)
        }
        _ => format!("{}:{}", key.channel, key.conversation_id),
    }
}

fn prepare_gateway_service_start() -> Result<bool> {
    let mut gateway_config = config::load_gateway_config()?;
    let mut launch_channels = config::resolve_launch_channels(&gateway_config)?;
    if launch_channels.is_empty() {
        match setup::run_gateway_setup() {
            Ok(true) => {
                gateway_config = config::load_gateway_config()?;
                launch_channels = config::resolve_launch_channels(&gateway_config)?;
            }
            Ok(false) => return Ok(false),
            Err(error) if is_runtime_setup_cancelled(&error) => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    if launch_channels.is_empty() {
        return Ok(false);
    }
    if resolve_runtime_provider(None, None).is_err() {
        match run_initial_runtime_setup() {
            Ok(_) => {}
            Err(error) if is_runtime_setup_cancelled(&error) => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    run_windows_sandbox_setup_after_provider_if_needed()?;
    Ok(true)
}

fn restart_gateway_service() -> Result<()> {
    let installed = service::gateway_service_installed()?;
    if let Some(pid) = service::running_gateway_pid()? {
        if !installed && !service::running_gateway_is_background_service(pid) {
            bail!(
                "{}",
                service::describe_running_gateway_for_service_start(pid)
            );
        }
    }
    if installed {
        service::stop_gateway_service()?;
    } else {
        service::install_gateway_service()?;
    }
    service::start_gateway_service()?;
    println!("Started duckagent gateway user service.");
    Ok(())
}

fn stop_and_uninstall_gateway_service() -> Result<()> {
    let installed = service::gateway_service_installed()?;
    service::stop_gateway_service()?;
    if installed || service::gateway_service_installed()? {
        service::uninstall_gateway_service()?;
    }
    println!("Stopped duckagent gateway user service.");
    Ok(())
}

fn run_pairing_command(command: GatewayPairingCommand) -> Result<()> {
    let store = GatewayPairingStore::new(default_gateway_pairing_dir()?)?;
    match command {
        GatewayPairingCommand::List(args) => {
            let channel = args.channel.as_deref().map(config::normalize_channel_name);
            let pending = store.list_pending(channel.as_deref())?;
            let approved = store.list_approved(channel.as_deref())?;
            println!("Pending pairings:");
            if pending.is_empty() {
                println!("  none");
            } else {
                for item in pending {
                    println!(
                        "  {}  {}  user={}  expires={}",
                        item.code,
                        item.channel,
                        item.user_id,
                        pairing::format_pairing_time(item.expires_at)
                    );
                }
            }
            println!("Approved users:");
            if approved.is_empty() {
                println!("  none");
            } else {
                for item in approved {
                    println!(
                        "  {}  user={}  approved={}",
                        item.channel,
                        item.user_id,
                        pairing::format_pairing_time(item.approved_at)
                    );
                }
            }
        }
        GatewayPairingCommand::Approve(args) => {
            let channel = args.channel.as_deref().map(config::normalize_channel_name);
            match store.approve_code(&args.code, channel.as_deref())? {
                Some(record) => println!("Approved {} user {}.", record.channel, record.user_id),
                None => bail!("pairing code `{}` was not found or has expired", args.code),
            }
        }
        GatewayPairingCommand::Revoke(args) => {
            let channel = args.channel.as_deref().map(config::normalize_channel_name);
            let removed = store.revoke(&args.user_id, channel.as_deref())?;
            if removed.is_empty() {
                bail!("approved user `{}` was not found", args.user_id);
            }
            for record in removed {
                println!("Revoked {} user {}.", record.channel, record.user_id);
            }
        }
    }
    Ok(())
}

fn serve_gateway_service(
    session_manager: SessionManager,
    system_prompt: &'static str,
) -> Result<()> {
    crate::profiles::pin_active_profile()?;
    serve_gateway_inner(session_manager, system_prompt)
}

fn serve_gateway_inner(session_manager: SessionManager, system_prompt: &'static str) -> Result<()> {
    let _instance_guard = service::GatewayInstanceGuard::acquire()?;
    let gateway_config = config::load_gateway_config()?;
    let launch_channels = config::resolve_launch_channels(&gateway_config)?;
    if launch_channels.is_empty() {
        bail!(
            "gateway service cannot start because no gateway channel is configured; run `duck gateway service start` from a terminal to configure it"
        );
    }

    refresh_models_dev_cache_background();
    let runtime = match resolve_runtime_provider(None, None) {
        Ok(runtime) => runtime,
        Err(_) => {
            bail!(
                "gateway service cannot start because Provider is not configured; run `duck gateway service start` from a terminal to configure it"
            );
        }
    };
    run_windows_sandbox_setup_after_provider_if_needed()?;
    let client = ModelClient::from_runtime(runtime)?;
    let agent = AgentRuntime::new(client, session_manager.clone())?;
    let outbox = GatewayOutbox::new();
    let adapters = create_adapters(&launch_channels, outbox.clone())?;
    let channel_configs = launch_channels
        .iter()
        .map(|channel| (channel.channel.clone(), channel.config.clone()))
        .collect::<HashMap<_, _>>();
    let api_server_key = launch_channels
        .iter()
        .find(|channel| {
            config::normalize_channel_name(&channel.channel) == config::API_SERVER_CHANNEL
        })
        .and_then(|channel| channel.credentials.as_ref())
        .and_then(|credentials| {
            credentials
                .token
                .clone()
                .or_else(|| credentials.api_key.clone())
        })
        .filter(|value| !value.trim().is_empty());
    let bind = gateway_config.bind_addr()?;
    let allow_random_bind_fallback = !config::launch_channels_need_stable_bind(&launch_channels);
    let runtime = GatewayRuntime {
        agent,
        session_manager,
        system_prompt,
        sessions: GatewaySessionStore::new(default_gateway_state_dir()?)?,
        attachments: GatewayAttachmentStore::new(default_gateway_attachments_dir()?)?,
        adapters: Arc::new(adapters),
        channel_configs: Arc::new(channel_configs),
        outbox,
        api_server_key,
        pairing: GatewayPairingStore::new(default_gateway_pairing_dir()?)?,
        approvals: PendingApprovals::new(),
        routes_by_session: Arc::new(Mutex::new(HashMap::new())),
        typing_flags: Arc::new(Mutex::new(HashMap::new())),
        streams: GatewayStreamStateStore::default(),
        session_lists: Arc::new(Mutex::new(HashMap::new())),
        cron: CronService::new(CronStore::new_default()?),
    };
    let inbound_runtime = runtime.clone();
    let inbound = GatewayInboundDispatch::new(move |input| {
        inbound_runtime.submit_inbound(input)?;
        Ok(())
    });
    for adapter in runtime.adapters.values() {
        adapter.start(inbound.clone())?;
        let _capabilities = adapter.capabilities();
    }
    runtime.cron.start(Arc::new(GatewayCronExecutor {
        runtime: runtime.clone(),
    }));
    start_agent_event_bridge(runtime.clone());
    let listener = bind_gateway_listener(bind, allow_random_bind_fallback)?;
    let local_addr = listener.local_addr().unwrap_or(bind);
    eprintln!("duckagent gateway listening on http://{local_addr}");
    for stream in listener.incoming() {
        let stream = stream.context("failed to accept gateway connection")?;
        let runtime = runtime.clone();
        thread::spawn(move || {
            if let Err(error) = handle_connection(stream, runtime) {
                eprintln!("duckagent gateway request error: {error:#}");
            }
        });
    }
    Ok(())
}

fn create_adapters(
    launch_channels: &[config::GatewayLaunchChannel],
    outbox: GatewayOutbox,
) -> Result<HashMap<String, Arc<dyn ChannelAdapter>>> {
    let mut adapters = HashMap::new();
    for launch in launch_channels {
        let adapter = channels::create_adapter(
            &launch.channel,
            &launch.config,
            launch.credentials.as_ref(),
            outbox.clone(),
        )?;
        adapters.insert(launch.channel.clone(), adapter);
    }
    Ok(adapters)
}

fn bind_gateway_listener(bind: SocketAddr, allow_random_fallback: bool) -> Result<TcpListener> {
    match TcpListener::bind(bind) {
        Ok(listener) => Ok(listener),
        Err(error)
            if error.kind() == ErrorKind::AddrInUse
                && allow_random_fallback
                && bind.port() != 0 =>
        {
            let fallback = SocketAddr::new(bind.ip(), 0);
            let listener = TcpListener::bind(fallback).with_context(|| {
                format!(
                    "failed to bind gateway server on {bind}; also failed to bind random fallback on {fallback}"
                )
            })?;
            eprintln!(
                "duckagent gateway bind {bind} is already in use; using random local port instead"
            );
            Ok(listener)
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to bind gateway server on {bind}"))
        }
    }
}

fn start_agent_event_bridge(runtime: GatewayRuntime) {
    let rx = runtime.agent.subscribe();
    thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            match event {
                AgentEvent::MainTurnStarted { session_id } => {
                    if let Some(route) = runtime.route_for_session(&session_id) {
                        runtime.clear_stream(&session_id);
                        runtime.start_feedback(route);
                    }
                }
                AgentEvent::MainTurnFinished { session_id } => {
                    if let Some(route) = runtime.route_for_session(&session_id) {
                        runtime.stop_typing(&route, "loop_finished");
                    }
                }
                AgentEvent::StatusChanged { session_id, status }
                    if is_model_retry_status(&status) =>
                {
                    if let Some(route) = runtime.route_for_session(&session_id) {
                        if let Some(adapter) = runtime.adapter_for_route(&route) {
                            runtime.push_status_preview(&session_id, &route, &adapter, &status);
                        }
                    }
                }
                AgentEvent::Error {
                    session_id,
                    message,
                } => {
                    if let Some(route) = runtime.route_for_session(&session_id) {
                        runtime.stop_typing(&route, "error");
                        if let Some(adapter) = runtime.adapter_for_route(&route) {
                            let error_text = format!("Error: {message}");
                            let stream_remainder =
                                runtime.finish_stream(&session_id, &route, &adapter, &error_text);
                            if stream_remainder
                                .as_ref()
                                .is_some_and(|text| text.trim().is_empty())
                            {
                                continue;
                            }
                            if let Err(error) = adapter.send_message(
                                &route,
                                OutboundMessage {
                                    text: stream_remainder.unwrap_or(error_text),
                                    media_paths: Vec::new(),
                                    reply_to: None,
                                    approval_prompt: None,
                                    typing_event: None,
                                },
                            ) {
                                eprintln!(
                                    "gateway error delivery failed for {}: {error:#}",
                                    route.key.channel
                                );
                            }
                        }
                    }
                }
                AgentEvent::Message {
                    session_id,
                    message,
                } if message.msg_type == MessageType::Assistant => {
                    if let Some(route) = runtime.route_for_session(&session_id) {
                        let Some(adapter) = runtime.adapter_for_route(&route) else {
                            continue;
                        };
                        let delivery =
                            parse_outbound_delivery(&message.content).unwrap_or_else(|error| {
                                OutboundDelivery {
                                    text: format!(
                                        "{}\n\n[media delivery error: {error:#}]",
                                        message.content
                                    ),
                                    media_paths: Vec::new(),
                                }
                            });
                        let stream_remainder =
                            runtime.finish_stream(&session_id, &route, &adapter, &delivery.text);
                        let mut final_text = delivery.text;
                        let mut media_paths = delivery.media_paths;
                        if let Some(remainder) = stream_remainder {
                            final_text = remainder;
                            if final_text.trim().is_empty() && media_paths.is_empty() {
                                continue;
                            }
                        }
                        if let Err(error) = adapter.send_message(
                            &route,
                            OutboundMessage {
                                text: final_text,
                                media_paths: std::mem::take(&mut media_paths),
                                reply_to: None,
                                approval_prompt: None,
                                typing_event: None,
                            },
                        ) {
                            eprintln!(
                                "gateway final delivery failed for {}: {error:#}",
                                route.key.channel
                            );
                        }
                    }
                }
                AgentEvent::StreamDelta { session_id, delta } => {
                    if let Some(route) = runtime.route_for_session(&session_id) {
                        if let Some(adapter) = runtime.adapter_for_route(&route) {
                            runtime.push_stream_delta(&session_id, &route, &adapter, &delta);
                        }
                    }
                }
                AgentEvent::CronRunFinished {
                    session_id,
                    job_id,
                    run_id,
                    status,
                    error,
                } => {
                    let run_status = if status == "ok" {
                        CronRunStatus::Ok
                    } else {
                        CronRunStatus::Error
                    };
                    if let Err(err) = runtime.cron.finish_run(
                        &run_id,
                        &job_id,
                        run_status,
                        Some(session_id),
                        error,
                    ) {
                        eprintln!("duckagent cron failed to persist run finish: {err:#}");
                    }
                }
                _ => {}
            }
        }
    });
}

struct GatewayCronExecutor {
    runtime: GatewayRuntime,
}

impl CronJobExecutor for GatewayCronExecutor {
    fn submit(&self, job: CronJob, context: CronRunContext) -> Result<CronSubmitResult> {
        self.runtime.submit_cron_job(job, context)
    }
}

impl GatewayRuntime {
    fn route_for_session(&self, session_id: &str) -> Option<GatewayRoute> {
        if let Some(route) = self
            .routes_by_session
            .lock()
            .expect("gateway routes mutex poisoned")
            .get(session_id)
            .cloned()
        {
            return Some(route);
        }
        let key = self.sessions.key_for_session(session_id)?;
        let route = GatewayRoute {
            session_id: session_id.to_string(),
            key,
        };
        self.routes_by_session
            .lock()
            .expect("gateway routes mutex poisoned")
            .insert(session_id.to_string(), route.clone());
        Some(route)
    }

    fn route_for_key(&self, key: &GatewaySessionKey) -> Option<GatewayRoute> {
        let session_id = self.sessions.get(key)?;
        let route = GatewayRoute {
            session_id: session_id.clone(),
            key: key.clone(),
        };
        self.routes_by_session
            .lock()
            .expect("gateway routes mutex poisoned")
            .insert(session_id, route.clone());
        Some(route)
    }

    fn adapter_for_route(&self, route: &GatewayRoute) -> Option<Arc<dyn ChannelAdapter>> {
        self.adapters.get(&route.key.channel).cloned()
    }

    fn adapter_for_channel(&self, channel: &str) -> Option<Arc<dyn ChannelAdapter>> {
        let channel = config::normalize_channel_name(channel);
        self.adapters.get(&channel).cloned()
    }

    fn submit_cron_job(&self, job: CronJob, context: CronRunContext) -> Result<CronSubmitResult> {
        let (session_id, route) = self.resolve_cron_target(&job)?;
        let text = build_scheduled_event_prompt(&job, &context);
        let approval_provider: Arc<dyn ApprovalProvider> = if let Some((route, adapter)) =
            route.clone().and_then(|route| {
                self.adapter_for_route(&route)
                    .map(|adapter| (route, adapter))
            }) {
            Arc::new(GatewayApprovalProvider {
                route,
                adapter,
                approvals: self.approvals.clone(),
                typing_flags: self.typing_flags.clone(),
            })
        } else {
            Arc::new(CronNoRouteApprovalProvider)
        };
        self.agent.submit_user_message(
            session_id.clone(),
            SubmittedUserMessage::cron(
                text,
                crate::agent::CronUserMessageMetadata {
                    job_id: context.job_id.clone(),
                    run_id: context.run_id.clone(),
                    scheduled_for: context.scheduled_for.clone(),
                },
            ),
            approval_provider,
        );
        Ok(CronSubmitResult {
            session_id: Some(session_id),
            completes_asynchronously: true,
        })
    }

    fn resolve_cron_target(&self, job: &CronJob) -> Result<(String, Option<GatewayRoute>)> {
        match &job.target {
            CronTarget::Session { session_id } => {
                if session_id.trim().is_empty() {
                    bail!("cron job target session_id is empty");
                }
                Ok((session_id.clone(), self.route_for_session(session_id)))
            }
            CronTarget::Gateway {
                channel,
                conversation_id,
                thread_id,
            } => {
                let key = GatewaySessionKey {
                    channel: config::normalize_channel_name(channel),
                    conversation_id: conversation_id.clone(),
                    thread_id: thread_id.clone(),
                };
                if self.adapter_for_channel(&key.channel).is_none() {
                    bail!(
                        "cron gateway target channel `{}` is not configured",
                        key.channel
                    );
                }
                let input = InboundMessageInput {
                    channel: key.channel.clone(),
                    conversation_id: key.conversation_id.clone(),
                    thread_id: key.thread_id.clone(),
                    chat_type: None,
                    sender_id: None,
                    message_id: None,
                    text: String::new(),
                    attachments: Vec::new(),
                    timestamp: Some(now_rfc3339_like()),
                };
                let route = self.ensure_route(&input)?;
                Ok((route.session_id.clone(), Some(route)))
            }
        }
    }

    fn start_feedback(&self, route: GatewayRoute) {
        let Some(adapter) = self.adapter_for_route(&route) else {
            return;
        };
        start_immediate_feedback(
            adapter,
            &self.streams,
            self.typing_flags.clone(),
            route,
            "loop_started",
        );
    }

    fn stop_typing(&self, route: &GatewayRoute, reason: &str) {
        let Some(adapter) = self.adapter_for_route(route) else {
            return;
        };
        stop_typing_refresh(adapter, self.typing_flags.clone(), route, reason);
    }

    fn clear_stream(&self, session_id: &str) {
        self.streams
            .inner
            .lock()
            .expect("gateway stream mutex poisoned")
            .remove(session_id);
    }

    fn push_stream_delta(
        &self,
        session_id: &str,
        route: &GatewayRoute,
        adapter: &Arc<dyn ChannelAdapter>,
        delta: &str,
    ) {
        enum StreamAction {
            Start(String),
            Update(StreamMessageHandle, String),
        }

        let limit = adapter.stream_text_limit();
        let min_delta_chars = adapter.stream_min_delta_chars();
        let flush_interval = adapter.stream_flush_interval();
        let update_budget = adapter.stream_update_budget();
        let action = {
            let mut guard = self
                .streams
                .inner
                .lock()
                .expect("gateway stream mutex poisoned");
            let state = guard.entry(session_id.to_string()).or_default();
            if state.disabled {
                return;
            }
            state.buffer.push_str(delta);
            let visible = clean_stream_preview_text(&state.buffer);
            let (preview, _) = split_stream_text(&visible, limit);
            if preview.trim().is_empty() || preview == state.last_sent {
                return;
            }
            if let Some(handle) = state.handle.clone() {
                if update_budget.is_some_and(|budget| state.update_count >= budget) {
                    state.disabled = true;
                    state.update_blocked = true;
                    return;
                }
                let enough_text =
                    preview.chars().count() >= state.last_sent.chars().count() + min_delta_chars;
                let enough_time = state
                    .last_flush
                    .is_none_or(|last| last.elapsed() >= flush_interval);
                if enough_text || enough_time || preview.ends_with('\n') {
                    Some(StreamAction::Update(handle, preview))
                } else {
                    None
                }
            } else if preview.chars().count() >= min_delta_chars || preview.ends_with('\n') {
                Some(StreamAction::Start(preview))
            } else {
                None
            }
        };

        let Some(action) = action else {
            return;
        };

        match action {
            StreamAction::Start(text) => match adapter.send_stream_start(route, &text) {
                Ok(Some(handle)) => {
                    let mut guard = self
                        .streams
                        .inner
                        .lock()
                        .expect("gateway stream mutex poisoned");
                    if let Some(state) = guard.get_mut(session_id) {
                        state.handle = Some(handle);
                        state.last_sent = text;
                        state.last_flush = Some(std::time::Instant::now());
                        state.delivered = true;
                    }
                }
                Ok(None) => {
                    if let Some(state) = self
                        .streams
                        .inner
                        .lock()
                        .expect("gateway stream mutex poisoned")
                        .get_mut(session_id)
                    {
                        state.disabled = true;
                    }
                }
                Err(error) => {
                    eprintln!(
                        "gateway stream start failed for {}: {error:#}",
                        route.key.channel
                    );
                    if let Some(state) = self
                        .streams
                        .inner
                        .lock()
                        .expect("gateway stream mutex poisoned")
                        .get_mut(session_id)
                    {
                        state.disabled = true;
                    }
                }
            },
            StreamAction::Update(handle, text) => {
                match adapter.update_stream(route, &handle, &text, false) {
                    Ok(()) => {
                        let mut guard = self
                            .streams
                            .inner
                            .lock()
                            .expect("gateway stream mutex poisoned");
                        if let Some(state) = guard.get_mut(session_id) {
                            state.last_sent = text;
                            state.last_flush = Some(std::time::Instant::now());
                            state.update_count = state.update_count.saturating_add(1);
                        }
                    }
                    Err(error) => {
                        eprintln!(
                            "gateway stream update failed for {}: {error:#}",
                            route.key.channel
                        );
                        let mut guard = self
                            .streams
                            .inner
                            .lock()
                            .expect("gateway stream mutex poisoned");
                        if let Some(state) = guard.get_mut(session_id) {
                            state.disabled = true;
                            state.update_blocked = true;
                        }
                    }
                }
            }
        }
    }

    fn push_status_preview(
        &self,
        session_id: &str,
        route: &GatewayRoute,
        adapter: &Arc<dyn ChannelAdapter>,
        status: &str,
    ) {
        enum StatusAction {
            Start(String),
            Update(StreamMessageHandle, String),
            Message(String),
        }

        let (preview, _) = split_stream_text(status, adapter.stream_text_limit());
        if preview.trim().is_empty() {
            return;
        }
        let action = {
            let mut guard = self
                .streams
                .inner
                .lock()
                .expect("gateway stream mutex poisoned");
            let state = guard.entry(session_id.to_string()).or_default();
            if state.last_sent == preview {
                return;
            }
            if let Some(handle) = state.handle.clone() {
                if adapter
                    .stream_update_budget()
                    .is_some_and(|budget| state.update_count >= budget)
                {
                    StatusAction::Message(preview)
                } else {
                    StatusAction::Update(handle, preview)
                }
            } else {
                StatusAction::Start(preview)
            }
        };

        match action {
            StatusAction::Start(text) => match adapter.send_stream_start(route, &text) {
                Ok(Some(handle)) => {
                    let mut guard = self
                        .streams
                        .inner
                        .lock()
                        .expect("gateway stream mutex poisoned");
                    if let Some(state) = guard.get_mut(session_id) {
                        state.handle = Some(handle);
                        state.delivered = true;
                        state.last_sent = text;
                        state.last_flush = Some(std::time::Instant::now());
                    }
                }
                Ok(None) => self.send_status_message(route, adapter, &text),
                Err(error) => {
                    eprintln!(
                        "gateway retry status start failed for {}: {error:#}",
                        route.key.channel
                    );
                    self.send_status_message(route, adapter, &text);
                }
            },
            StatusAction::Update(handle, text) => {
                match adapter.update_stream(route, &handle, &text, false) {
                    Ok(()) => {
                        let mut guard = self
                            .streams
                            .inner
                            .lock()
                            .expect("gateway stream mutex poisoned");
                        if let Some(state) = guard.get_mut(session_id) {
                            state.last_sent = text;
                            state.last_flush = Some(std::time::Instant::now());
                            state.update_count = state.update_count.saturating_add(1);
                            state.delivered = true;
                        }
                    }
                    Err(error) => {
                        eprintln!(
                            "gateway retry status update failed for {}: {error:#}",
                            route.key.channel
                        );
                        self.send_status_message(route, adapter, &text);
                    }
                }
            }
            StatusAction::Message(text) => self.send_status_message(route, adapter, &text),
        }
    }

    fn send_status_message(
        &self,
        route: &GatewayRoute,
        adapter: &Arc<dyn ChannelAdapter>,
        text: &str,
    ) {
        if let Err(error) = adapter.send_message(
            route,
            OutboundMessage {
                text: text.to_string(),
                media_paths: Vec::new(),
                reply_to: None,
                approval_prompt: None,
                typing_event: None,
            },
        ) {
            eprintln!(
                "gateway retry status delivery failed for {}: {error:#}",
                route.key.channel
            );
        }
    }

    fn finish_stream(
        &self,
        session_id: &str,
        route: &GatewayRoute,
        adapter: &Arc<dyn ChannelAdapter>,
        final_text: &str,
    ) -> Option<String> {
        let state = self
            .streams
            .inner
            .lock()
            .expect("gateway stream mutex poisoned")
            .remove(session_id)?;
        if !state.delivered {
            return None;
        }
        let handle = state.handle?;
        let visible = clean_stream_preview_text(final_text);
        let (edit_text, remainder) = split_stream_text(&visible, adapter.stream_text_limit());
        if state.update_blocked || state.disabled {
            return Some(stream_fallback_remainder(
                &visible,
                &state.last_sent,
                &remainder,
            ));
        }
        if edit_text.trim().is_empty() {
            return Some(remainder);
        }
        if edit_text != state.last_sent {
            if let Err(error) = adapter.update_stream(route, &handle, &edit_text, true) {
                eprintln!(
                    "gateway stream final update failed for {}: {error:#}",
                    route.key.channel
                );
                return Some(stream_fallback_remainder(
                    &visible,
                    &state.last_sent,
                    &remainder,
                ));
            }
        }
        Some(remainder)
    }

    fn ensure_route(&self, input: &InboundMessageInput) -> Result<GatewayRoute> {
        let key = input.session_key();
        let session_id = self.sessions.get_or_create(&key, || {
            self.session_manager.create_session_with_runtime_and_source(
                None,
                self.system_prompt,
                self.agent.runtime().session_config(),
                &format!("gateway:{}", key.channel),
            )
        })?;
        let route = GatewayRoute { session_id, key };
        self.routes_by_session
            .lock()
            .expect("gateway routes mutex poisoned")
            .insert(route.session_id.clone(), route.clone());
        Ok(route)
    }

    fn submit_inbound(&self, input: InboundMessageInput) -> Result<GatewayRoute> {
        let channel = config::normalize_channel_name(&input.channel);
        let adapter = self
            .adapter_for_channel(&channel)
            .ok_or_else(|| anyhow!("gateway channel `{channel}` is not configured or enabled"))?;
        let normalized_input = InboundMessageInput { channel, ..input };
        let config = self
            .channel_configs
            .get(&normalized_input.channel)
            .ok_or_else(|| {
                anyhow!(
                    "gateway channel `{}` has no config",
                    normalized_input.channel
                )
            })?;
        match evaluate_gateway_access(&normalized_input, config, &self.pairing)? {
            GatewayAccessDecision::Allowed => {}
            GatewayAccessDecision::PairingRequired { notice } => {
                let _ = send_access_notice(adapter.clone(), &normalized_input, notice);
                return Ok(GatewayRoute {
                    session_id: String::new(),
                    key: normalized_input.session_key(),
                });
            }
            GatewayAccessDecision::Blocked { reason } => {
                eprintln!(
                    "duckagent gateway inbound blocked for {}:{}: {}",
                    normalized_input.channel, normalized_input.conversation_id, reason
                );
                return Ok(GatewayRoute {
                    session_id: String::new(),
                    key: normalized_input.session_key(),
                });
            }
        }
        if let Some(control) = try_handle_approval_text(
            &normalized_input.text,
            &self.approvals,
            Some(&normalized_input.session_key()),
        )? {
            eprintln!("duckagent gateway approval command: {control}");
            if let Some(route) = self.route_for_key(&normalized_input.session_key()) {
                let _ = adapter.send_message(
                    &route,
                    OutboundMessage {
                        text: render_approval_control_response(&control),
                        media_paths: Vec::new(),
                        reply_to: normalized_input.message_id.clone(),
                        approval_prompt: None,
                        typing_event: None,
                    },
                );
                return Ok(route);
            }
            return Ok(GatewayRoute {
                session_id: String::new(),
                key: normalized_input.session_key(),
            });
        }
        if let Some(command) = parse_session_control_command(&normalized_input.text) {
            self.deny_all_pending_approvals_for_input(&normalized_input);
            self.handle_session_control_command(&normalized_input, command, adapter)?;
            return Ok(GatewayRoute {
                session_id: self
                    .sessions
                    .get(&normalized_input.session_key())
                    .unwrap_or_default(),
                key: normalized_input.session_key(),
            });
        }
        if normalized_input.text.trim() == "/model" {
            self.deny_all_pending_approvals_for_input(&normalized_input);
            let route = self.ensure_route(&normalized_input)?;
            send_control_text(
                adapter,
                &route,
                "Model management is available in the local TUI only. Run `duck model` or use `/model` inside `duck`.",
                normalized_input.message_id.clone(),
            )?;
            return Ok(route);
        }
        self.deny_all_pending_approvals_for_input(&normalized_input);
        let route = self.ensure_route(&normalized_input)?;
        let attachments = self
            .attachments
            .ingest_all(&route.session_id, &normalized_input.attachments)?;
        let inbound = InboundMessage {
            channel: normalized_input.channel.clone(),
            conversation_id: normalized_input.conversation_id.clone(),
            thread_id: normalized_input.thread_id.clone(),
            chat_type: normalized_input.chat_type.clone(),
            sender_id: normalized_input.sender_id.clone(),
            message_id: normalized_input.message_id.clone(),
            text: normalized_input.text.clone(),
            attachments,
            timestamp: normalized_input.timestamp.unwrap_or_else(now_rfc3339_like),
        };
        let text = compose_gateway_user_message(&inbound);
        let metadata = GatewayUserMessageMetadata {
            channel: inbound.channel.clone(),
            conversation_id: inbound.conversation_id.clone(),
            thread_id: inbound.thread_id.clone(),
            sender_id: inbound.sender_id.clone(),
            message_id: inbound.message_id.clone(),
        };
        let approval_provider = Arc::new(GatewayApprovalProvider {
            route: route.clone(),
            adapter,
            approvals: self.approvals.clone(),
            typing_flags: self.typing_flags.clone(),
        });
        self.agent.submit_user_message(
            route.session_id.clone(),
            SubmittedUserMessage::gateway(text, metadata),
            approval_provider,
        );
        Ok(route)
    }

    fn deny_all_pending_approvals_for_input(&self, input: &InboundMessageInput) {
        if input.text.trim().is_empty() && input.attachments.is_empty() {
            return;
        }
        let key = input.session_key();
        let control = deny_all_pending_approvals_for_key(&self.approvals, &key);
        if let Some(control) = control {
            eprintln!("duckagent gateway auto-denied pending approvals for new input: {control}");
        }
    }

    fn handle_session_control_command(
        &self,
        input: &InboundMessageInput,
        command: SessionControlCommand,
        adapter: Arc<dyn ChannelAdapter>,
    ) -> Result<()> {
        let key = input.session_key();
        match command {
            SessionControlCommand::New { title } => {
                if let Some(old_session_id) = self.sessions.get(&key) {
                    self.agent.clear_session_runtime_state(&old_session_id);
                    self.routes_by_session
                        .lock()
                        .expect("gateway routes mutex poisoned")
                        .remove(&old_session_id);
                    self.streams
                        .inner
                        .lock()
                        .expect("gateway stream mutex poisoned")
                        .remove(&old_session_id);
                }
                let session_id = self
                    .session_manager
                    .create_session_with_runtime_and_source(
                        title.as_deref(),
                        self.system_prompt,
                        self.agent.runtime().session_config(),
                        &format!("gateway:{}", key.channel),
                    )?;
                self.sessions.set_session(&key, session_id.clone())?;
                let route = GatewayRoute {
                    session_id: session_id.clone(),
                    key: key.clone(),
                };
                self.routes_by_session
                    .lock()
                    .expect("gateway routes mutex poisoned")
                    .insert(session_id, route.clone());
                self.session_lists
                    .lock()
                    .expect("gateway session lists mutex poisoned")
                    .remove(&key);
                let suffix = title
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| format!(": {value}"))
                    .unwrap_or_default();
                send_control_text(
                    adapter,
                    &route,
                    &format!("Started a new session{suffix}."),
                    input.message_id.clone(),
                )?;
            }
            SessionControlCommand::Resume { index: None } => {
                let items = self.gateway_session_items(&key, None)?;
                let (page_items, page, total_pages) = paginate_session_items(&items, 1);
                self.session_lists
                    .lock()
                    .expect("gateway session lists mutex poisoned")
                    .insert(key.clone(), page_items.to_vec());
                let route = self.ensure_route(input)?;
                send_control_text(
                    adapter,
                    &route,
                    &format_session_list(page_items, page, total_pages),
                    input.message_id.clone(),
                )?;
            }
            SessionControlCommand::Resume { index: Some(index) } => {
                let item = self
                    .session_lists
                    .lock()
                    .expect("gateway session lists mutex poisoned")
                    .get(&key)
                    .and_then(|items| items.get(index.saturating_sub(1)).cloned());
                let Some(item) = item else {
                    let route = self.ensure_route(input)?;
                    send_control_text(
                        adapter,
                        &route,
                        "That session number is not in the latest `/resume` list.",
                        input.message_id.clone(),
                    )?;
                    return Ok(());
                };
                if let Some(old_session_id) = self.sessions.get(&key) {
                    self.agent.clear_session_runtime_state(&old_session_id);
                    self.routes_by_session
                        .lock()
                        .expect("gateway routes mutex poisoned")
                        .remove(&old_session_id);
                }
                self.sessions.set_session(&key, item.session_id.clone())?;
                let route = GatewayRoute {
                    session_id: item.session_id,
                    key: key.clone(),
                };
                self.routes_by_session
                    .lock()
                    .expect("gateway routes mutex poisoned")
                    .insert(route.session_id.clone(), route.clone());
                send_control_text(
                    adapter,
                    &route,
                    &format!("Resumed session: {}.", item.title),
                    input.message_id.clone(),
                )?;
            }
            SessionControlCommand::Rewind { index: None } => {
                let route = self.ensure_route(input)?;
                let items = self.agent.rewind_list(&route.session_id)?;
                send_control_text(
                    adapter,
                    &route,
                    &format_rewind_list(&items),
                    input.message_id.clone(),
                )?;
            }
            SessionControlCommand::Rewind { index: Some(index) } => {
                let route = self.ensure_route(input)?;
                if self.agent.has_pending_or_running_work(&route.session_id) {
                    send_control_text(
                        adapter,
                        &route,
                        "Rewind is available after the current turn finishes.",
                        input.message_id.clone(),
                    )?;
                    return Ok(());
                }
                let result = self.agent.rewind_session(&route.session_id, index)?;
                let mut text = format!(
                    "Rewound to before #{}: {}.\nRestored file changes: {}.",
                    result.target.index,
                    result.target.preview,
                    result.restored_files.len()
                );
                if !result.warnings.is_empty() {
                    text.push_str("\n\nWarnings:\n");
                    for warning in &result.warnings {
                        text.push_str("- ");
                        text.push_str(warning);
                        text.push('\n');
                    }
                }
                send_control_text(adapter, &route, text.trim_end(), input.message_id.clone())?;
            }
            SessionControlCommand::Invalid { message } => {
                let route = self.ensure_route(input)?;
                send_control_text(adapter, &route, &message, input.message_id.clone())?;
            }
        }
        Ok(())
    }

    fn gateway_session_items(
        &self,
        key: &GatewaySessionKey,
        query: Option<&str>,
    ) -> Result<Vec<SessionListItem>> {
        let mut items = Vec::new();
        for session_id in self.sessions.session_ids_for_key(key)? {
            if let Ok(meta) = self.session_manager.get_session_meta(&session_id) {
                items.push(session_meta_to_list_item(&meta, "current chat"));
            }
        }
        Ok(filter_session_items(items, query))
    }
}

fn send_control_text(
    adapter: Arc<dyn ChannelAdapter>,
    route: &GatewayRoute,
    text: &str,
    reply_to: Option<String>,
) -> Result<()> {
    for chunk in split_control_text(text, adapter.stream_text_limit()) {
        adapter.send_message(
            route,
            OutboundMessage {
                text: chunk,
                media_paths: Vec::new(),
                reply_to: reply_to.clone(),
                approval_prompt: None,
                typing_event: None,
            },
        )?;
    }
    Ok(())
}

fn split_control_text(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(200);
    if text.chars().count() <= limit {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        let projected = if current.is_empty() {
            line.chars().count()
        } else {
            current.chars().count() + 1 + line.chars().count()
        };
        if projected > limit && !current.is_empty() {
            chunks.push(current);
            current = String::new();
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn start_typing_refresh(
    adapter: Arc<dyn ChannelAdapter>,
    typing_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    route: GatewayRoute,
    reason: &str,
) {
    let flag = Arc::new(AtomicBool::new(true));
    {
        let mut guard = typing_flags.lock().expect("gateway typing mutex poisoned");
        if let Some(previous) = guard.insert(route.session_id.clone(), flag.clone()) {
            previous.store(false, Ordering::SeqCst);
        }
    }
    let refresh_route = route.clone();
    let _ = adapter.send_typing(
        &route,
        TypingEvent {
            active: true,
            reason: reason.to_string(),
        },
    );
    thread::spawn(move || {
        while flag.load(Ordering::SeqCst) {
            thread::sleep(TYPING_REFRESH_INTERVAL);
            if flag.load(Ordering::SeqCst) {
                let _ = adapter.send_typing(
                    &refresh_route,
                    TypingEvent {
                        active: true,
                        reason: "refresh".to_string(),
                    },
                );
            }
        }
    });
}

fn start_immediate_feedback(
    adapter: Arc<dyn ChannelAdapter>,
    streams: &GatewayStreamStateStore,
    typing_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    route: GatewayRoute,
    reason: &str,
) {
    if adapter.capabilities().typing {
        start_typing_refresh(adapter, typing_flags, route, reason);
        return;
    }
    match adapter.send_stream_start(&route, GATEWAY_THINKING_PLACEHOLDER) {
        Ok(Some(handle)) => {
            let mut guard = streams.inner.lock().expect("gateway stream mutex poisoned");
            let state = guard.entry(route.session_id.clone()).or_default();
            state.handle = Some(handle);
            state.delivered = true;
            state.last_sent = GATEWAY_THINKING_PLACEHOLDER.to_string();
            state.last_flush = Some(std::time::Instant::now());
        }
        Ok(None) => {
            if let Err(error) = adapter.send_message(
                &route,
                OutboundMessage {
                    text: GATEWAY_THINKING_PLACEHOLDER.to_string(),
                    media_paths: Vec::new(),
                    reply_to: None,
                    approval_prompt: None,
                    typing_event: None,
                },
            ) {
                eprintln!(
                    "gateway thinking placeholder delivery failed for {}: {error:#}",
                    route.key.channel
                );
            }
        }
        Err(error) => {
            eprintln!(
                "gateway editable thinking placeholder failed for {}: {error:#}",
                route.key.channel
            );
            if let Err(error) = adapter.send_message(
                &route,
                OutboundMessage {
                    text: GATEWAY_THINKING_PLACEHOLDER.to_string(),
                    media_paths: Vec::new(),
                    reply_to: None,
                    approval_prompt: None,
                    typing_event: None,
                },
            ) {
                eprintln!(
                    "gateway thinking placeholder delivery failed for {}: {error:#}",
                    route.key.channel
                );
            }
        }
    }
}

fn stop_typing_refresh(
    adapter: Arc<dyn ChannelAdapter>,
    typing_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    route: &GatewayRoute,
    reason: &str,
) {
    if let Some(flag) = typing_flags
        .lock()
        .expect("gateway typing mutex poisoned")
        .remove(&route.session_id)
    {
        flag.store(false, Ordering::SeqCst);
    }
    let _ = adapter.send_typing(
        route,
        TypingEvent {
            active: false,
            reason: reason.to_string(),
        },
    );
}

fn clean_stream_preview_text(text: &str) -> String {
    let media_line = Regex::new(r#"^\s*MEDIA:\s*\S+\s*$"#).expect("valid MEDIA regex");
    let md_local_image =
        Regex::new(r#"!\[[^\]]*\]\((?P<path>[^)]+)\)"#).expect("valid markdown image regex");
    let mut kept_lines = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if media_line.is_match(line) || trimmed.starts_with("MEDIA:") {
            continue;
        }
        kept_lines.push(line);
    }
    let without_media = kept_lines.join("\n");
    md_local_image
        .replace_all(&without_media, |captures: &regex::Captures<'_>| {
            let path = captures
                .name("path")
                .map_or("", |value| value.as_str())
                .trim();
            if path.starts_with('/') || path.starts_with("$TMPDIR/") {
                String::new()
            } else {
                captures
                    .get(0)
                    .map_or_else(String::new, |value| value.as_str().to_string())
            }
        })
        .trim()
        .to_string()
}

fn is_model_retry_status(status: &str) -> bool {
    status.starts_with("Retrying model ")
        || status.starts_with("Trying fallback model ")
        || status.starts_with("Model request failed ")
}

fn split_stream_text(text: &str, limit: usize) -> (String, String) {
    if limit == 0 || text.chars().count() <= limit {
        return (text.to_string(), String::new());
    }
    let prefix_limit = limit
        .saturating_sub(STREAM_CONTINUED_SUFFIX.chars().count())
        .max(1);
    let prefix = text.chars().take(prefix_limit).collect::<String>();
    let remainder = text.chars().skip(prefix_limit).collect::<String>();
    (
        format!("{prefix}{STREAM_CONTINUED_SUFFIX}"),
        remainder.trim().to_string(),
    )
}

fn stream_fallback_remainder(visible: &str, last_sent: &str, split_remainder: &str) -> String {
    let shown_prefix = last_sent
        .strip_suffix(STREAM_CONTINUED_SUFFIX)
        .unwrap_or(last_sent);
    if !shown_prefix.is_empty() && visible.starts_with(shown_prefix) {
        return visible[shown_prefix.len()..].trim().to_string();
    }
    if !split_remainder.trim().is_empty() {
        return split_remainder.trim().to_string();
    }
    visible.trim().to_string()
}

fn handle_connection(mut stream: TcpStream, runtime: GatewayRuntime) -> Result<()> {
    let request = read_http_request(&mut stream)?;
    let response = handle_http_request(request, runtime);
    write_http_response(&mut stream, response)?;
    Ok(())
}

fn handle_http_request(request: HttpRequest, runtime: GatewayRuntime) -> HttpResponse {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/health") => json_response(200, json!({"status": "ok"})),
        ("GET", "/v1/health") => match handle_api_health(&runtime) {
            Ok(value) => json_response(200, value),
            Err(error) => json_response(404, json!({"error": {"message": format!("{error:#}")}})),
        },
        ("GET", "/health/detailed") | ("GET", "/v1/health/detailed") => {
            match handle_api_health_detailed(&request, &runtime) {
                Ok(value) => json_response(200, value),
                Err(error) => {
                    json_response(401, json!({"error": {"message": format!("{error:#}")}}))
                }
            }
        }
        ("GET", "/webhook/outbox") => {
            let since = request
                .query
                .get("since")
                .and_then(|value| value.parse::<u64>().ok());
            json_response(200, json!({"events": runtime.outbox.list_since(since)}))
        }
        ("POST", "/webhook/inbound") => match handle_inbound_request(request.body, runtime) {
            Ok(value) => json_response(200, value),
            Err(error) => json_response(400, json!({"error": format!("{error:#}")})),
        },
        ("GET", "/v1/models") => match handle_api_models(&request, &runtime) {
            Ok(value) => json_response(200, value),
            Err(error) => json_response(401, json!({"error": {"message": format!("{error:#}")}})),
        },
        ("GET", "/v1/capabilities") => match handle_api_capabilities(&request, &runtime) {
            Ok(value) => json_response(200, value),
            Err(error) => json_response(401, json!({"error": {"message": format!("{error:#}")}})),
        },
        ("POST", "/v1/chat/completions") => match handle_api_chat_completions(request, runtime) {
            Ok(value) => json_response(200, value),
            Err(error) => json_response(400, json!({"error": {"message": format!("{error:#}")}})),
        },
        ("POST", "/v1/responses") => match handle_api_responses(request, runtime) {
            Ok(value) => json_response(200, value),
            Err(error) => json_response(400, json!({"error": {"message": format!("{error:#}")}})),
        },
        ("GET", path) if path.starts_with("/v1/responses/") => {
            match handle_api_get_response(&request, &runtime, path) {
                Ok(Some(value)) => json_response(200, value),
                Ok(None) => json_response(
                    404,
                    json!({"error": {"message": "response not found", "type": "not_found"}}),
                ),
                Err(error) => {
                    json_response(401, json!({"error": {"message": format!("{error:#}")}}))
                }
            }
        }
        ("DELETE", path) if path.starts_with("/v1/responses/") => {
            match handle_api_delete_response(&request, &runtime, path) {
                Ok(value) => json_response(200, value),
                Err(error) => {
                    json_response(401, json!({"error": {"message": format!("{error:#}")}}))
                }
            }
        }
        _ => match handle_channel_http_request(request, runtime) {
            Ok(Some(response)) => HttpResponse {
                status: response.status,
                content_type: response.content_type,
                body: response.body,
            },
            Ok(None) => json_response(404, json!({"error": "not found"})),
            Err(error) => json_response(400, json!({"error": format!("{error:#}")})),
        },
    }
}

fn handle_api_models(request: &HttpRequest, runtime: &GatewayRuntime) -> Result<Value> {
    ensure_api_server_enabled(runtime)?;
    ensure_api_authorized(request, runtime)?;
    Ok(json!({
        "object": "list",
        "data": [{
            "id": "duckagent",
            "object": "model",
            "created": 0,
            "owned_by": "duckagent"
        }]
    }))
}

fn handle_api_capabilities(request: &HttpRequest, runtime: &GatewayRuntime) -> Result<Value> {
    ensure_api_server_enabled(runtime)?;
    ensure_api_authorized(request, runtime)?;
    Ok(json!({
        "object": "duckagent.api_server.capabilities",
        "platform": "duckagent",
        "model": "duckagent",
        "auth": {
            "type": "bearer",
            "required": runtime.api_server_key.is_some()
        },
        "features": {
            "chat_completions": true,
            "responses_api": true,
            "streaming": false,
            "health": true,
            "health_detailed": true,
            "run_submission": false,
            "run_status": false,
            "run_events_sse": false,
            "run_stop": false
        },
        "endpoints": [
            "GET /v1/models",
            "GET /v1/capabilities",
            "GET /v1/health",
            "GET /v1/health/detailed",
            "POST /v1/chat/completions",
            "POST /v1/responses",
            "GET /v1/responses/{response_id}",
            "DELETE /v1/responses/{response_id}"
        ],
        "sessions": {
            "headers": ["X-DuckAgent-Session-Id"],
            "response_field": "duckagent.session_id"
        },
        "media": {
            "image_url_parts": true,
            "file_parts": false,
            "description": "image_url/input_image parts are normalized into labeled input text"
        }
    }))
}

fn handle_api_health(runtime: &GatewayRuntime) -> Result<Value> {
    ensure_api_server_enabled(runtime)?;
    Ok(json!({
        "status": "ok",
        "object": "duckagent.api_server.health"
    }))
}

fn handle_api_health_detailed(request: &HttpRequest, runtime: &GatewayRuntime) -> Result<Value> {
    ensure_api_server_enabled(runtime)?;
    ensure_api_authorized(request, runtime)?;
    Ok(json!({
        "status": "ok",
        "object": "duckagent.api_server.health.detailed",
        "api_server": {
            "enabled": true,
            "auth_required": runtime.api_server_key.is_some(),
            "streaming": false
        },
        "features": {
            "chat_completions": true,
            "responses_api": true,
            "responses_retrieve": true,
            "responses_delete": true,
            "health": true,
            "capabilities": true
        },
        "sessions": {
            "header": "X-DuckAgent-Session-Id",
            "response_field": "duckagent.session_id"
        }
    }))
}

fn handle_api_chat_completions(request: HttpRequest, runtime: GatewayRuntime) -> Result<Value> {
    ensure_api_server_enabled(&runtime)?;
    ensure_api_authorized(&request, &runtime)?;
    let body: Value =
        serde_json::from_slice(&request.body).context("failed to parse chat completions JSON")?;
    let messages = body["messages"]
        .as_array()
        .ok_or_else(|| anyhow!("messages must be an array"))?;
    let user_message = compose_openai_messages(messages)?;
    if user_message.trim().is_empty() {
        bail!("messages contain no user-visible content");
    }
    let conversation_id =
        api_session_id(&request).unwrap_or_else(|| format!("chatcmpl-{}", Uuid::now_v7()));
    let output = run_api_user_message(runtime, conversation_id.clone(), user_message)?;
    let id = format!("chatcmpl-{}", Uuid::now_v7());
    Ok(json!({
        "id": id.clone(),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": body["model"].as_str().unwrap_or("duckagent"),
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": output},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
        "duckagent": {"session_id": conversation_id}
    }))
}

fn handle_api_responses(request: HttpRequest, runtime: GatewayRuntime) -> Result<Value> {
    ensure_api_server_enabled(&runtime)?;
    ensure_api_authorized(&request, &runtime)?;
    let body: Value =
        serde_json::from_slice(&request.body).context("failed to parse responses JSON")?;
    let input = body
        .get("input")
        .ok_or_else(|| anyhow!("input is required"))?;
    let user_message = normalize_openai_content(input)?;
    if user_message.trim().is_empty() {
        bail!("input contains no user-visible content");
    }
    let previous_response_id = body["previous_response_id"].as_str();
    let conversation_id = api_session_id(&request)
        .or_else(|| previous_response_id.and_then(api_response_session_id))
        .or_else(|| previous_response_id.map(str::to_string))
        .unwrap_or_else(|| format!("resp-{}", Uuid::now_v7()));
    let output = run_api_user_message(runtime, conversation_id.clone(), user_message)?;
    let id = format!("resp_{}", Uuid::now_v7());
    let response = json!({
        "id": id,
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "model": body["model"].as_str().unwrap_or("duckagent"),
        "status": "completed",
        "previous_response_id": previous_response_id,
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": output}]
        }],
        "output_text": output,
        "duckagent": {"session_id": conversation_id}
    });
    store_api_response(&id, response.clone());
    Ok(response)
}

fn handle_api_get_response(
    request: &HttpRequest,
    runtime: &GatewayRuntime,
    path: &str,
) -> Result<Option<Value>> {
    ensure_api_server_enabled(runtime)?;
    ensure_api_authorized(request, runtime)?;
    let response_id = api_response_id_from_path(path)?;
    Ok(load_api_response(response_id))
}

fn handle_api_delete_response(
    request: &HttpRequest,
    runtime: &GatewayRuntime,
    path: &str,
) -> Result<Value> {
    ensure_api_server_enabled(runtime)?;
    ensure_api_authorized(request, runtime)?;
    let response_id = api_response_id_from_path(path)?;
    let deleted = delete_api_response(response_id);
    Ok(json!({
        "id": response_id,
        "object": "response.deleted",
        "deleted": deleted
    }))
}

fn api_response_id_from_path(path: &str) -> Result<&str> {
    let response_id = path
        .strip_prefix("/v1/responses/")
        .unwrap_or_default()
        .trim();
    if response_id.is_empty() || response_id.contains('/') {
        bail!("response id is required");
    }
    Ok(response_id)
}

const API_RESPONSE_STORE_LIMIT: usize = 100;

#[derive(Default)]
struct ApiResponseStore {
    order: std::collections::VecDeque<String>,
    items: std::collections::HashMap<String, Value>,
}

static API_RESPONSE_STORE: std::sync::OnceLock<std::sync::Mutex<ApiResponseStore>> =
    std::sync::OnceLock::new();

fn api_response_store() -> &'static std::sync::Mutex<ApiResponseStore> {
    API_RESPONSE_STORE.get_or_init(|| std::sync::Mutex::new(ApiResponseStore::default()))
}

fn store_api_response(response_id: &str, response: Value) {
    let mut store = api_response_store()
        .lock()
        .expect("api response store mutex poisoned");
    if !store.items.contains_key(response_id) {
        store.order.push_back(response_id.to_string());
    }
    store.items.insert(response_id.to_string(), response);
    while store.order.len() > API_RESPONSE_STORE_LIMIT {
        if let Some(oldest) = store.order.pop_front() {
            store.items.remove(&oldest);
        }
    }
}

fn load_api_response(response_id: &str) -> Option<Value> {
    api_response_store()
        .lock()
        .expect("api response store mutex poisoned")
        .items
        .get(response_id)
        .cloned()
}

fn delete_api_response(response_id: &str) -> bool {
    let mut store = api_response_store()
        .lock()
        .expect("api response store mutex poisoned");
    let deleted = store.items.remove(response_id).is_some();
    if deleted {
        let mut next_order = std::collections::VecDeque::new();
        while let Some(existing) = store.order.pop_front() {
            if existing != response_id {
                next_order.push_back(existing);
            }
        }
        store.order = next_order;
    }
    deleted
}

fn api_response_session_id(response_id: &str) -> Option<String> {
    load_api_response(response_id).and_then(|response| {
        response["duckagent"]["session_id"]
            .as_str()
            .map(str::to_string)
    })
}

fn run_api_user_message(
    runtime: GatewayRuntime,
    conversation_id: String,
    text: String,
) -> Result<String> {
    let rx = runtime.agent.subscribe();
    let route = runtime.submit_inbound(InboundMessageInput {
        channel: config::API_SERVER_CHANNEL.to_string(),
        conversation_id,
        thread_id: None,
        chat_type: Some("dm".to_string()),
        sender_id: Some("api".to_string()),
        message_id: Some(format!("api_{}", Uuid::now_v7())),
        text,
        attachments: Vec::new(),
        timestamp: Some(now_rfc3339_like()),
    })?;
    let mut last_assistant = String::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(180)) {
            Ok(AgentEvent::Message {
                session_id,
                message,
            }) if session_id == route.session_id && message.msg_type == MessageType::Assistant => {
                last_assistant = message.content;
            }
            Ok(AgentEvent::MainTurnFinished { session_id }) if session_id == route.session_id => {
                return Ok(last_assistant);
            }
            Ok(AgentEvent::Error {
                session_id,
                message,
            }) if session_id == route.session_id => bail!(message),
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => bail!("api_server request timed out"),
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("agent event bus disconnected"),
        }
    }
}

fn ensure_api_server_enabled(runtime: &GatewayRuntime) -> Result<()> {
    if runtime
        .adapters
        .contains_key(&config::normalize_channel_name(config::API_SERVER_CHANNEL))
    {
        Ok(())
    } else {
        bail!("api_server gateway channel is not enabled")
    }
}

fn ensure_api_authorized(request: &HttpRequest, runtime: &GatewayRuntime) -> Result<()> {
    let Some(expected) = runtime.api_server_key.as_deref() else {
        return Ok(());
    };
    let bearer = header_value(request, "authorization")
        .and_then(|value| value.trim().strip_prefix("Bearer "))
        .unwrap_or_default();
    if bearer == expected {
        Ok(())
    } else {
        bail!("missing or invalid bearer token")
    }
}

fn header_value<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn api_session_id(request: &HttpRequest) -> Option<String> {
    header_value(request, "x-duckagent-session-id")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn compose_openai_messages(messages: &[Value]) -> Result<String> {
    let mut out = String::new();
    out.push_str("[OpenAI Chat Completion Request]\n");
    for message in messages {
        let role = message["role"].as_str().unwrap_or("user");
        let content = normalize_openai_content(&message["content"])?;
        if content.trim().is_empty() {
            continue;
        }
        out.push_str(&format!("\n[{role}]\n{content}\n"));
    }
    Ok(out.trim().to_string())
}

fn normalize_openai_content(content: &Value) -> Result<String> {
    match content {
        Value::Null => Ok(String::new()),
        Value::String(text) => Ok(text.clone()),
        Value::Array(parts) => {
            let mut out = Vec::new();
            for part in parts {
                if let Some(text) = part.as_str() {
                    out.push(text.to_string());
                    continue;
                }
                let part_type = part["type"].as_str().unwrap_or_default();
                match part_type {
                    "text" | "input_text" | "output_text" => {
                        if let Some(text) = part["text"].as_str() {
                            out.push(text.to_string());
                        }
                    }
                    "image_url" | "input_image" => {
                        let url = part["image_url"]["url"]
                            .as_str()
                            .or_else(|| part["image_url"].as_str())
                            .ok_or_else(|| anyhow!("image content part missing image_url"))?;
                        out.push(format!("[Image URL]\n{url}"));
                    }
                    "file" | "input_file" => bail!("file content parts are not supported yet"),
                    other => bail!("unsupported content part type `{other}`"),
                }
            }
            Ok(out.join("\n"))
        }
        other => Ok(other.to_string()),
    }
}

fn handle_channel_http_request(
    request: HttpRequest,
    runtime: GatewayRuntime,
) -> Result<Option<ChannelHttpResponse>> {
    let inbound_runtime = runtime.clone();
    let inbound = GatewayInboundDispatch::new(move |input| {
        inbound_runtime.submit_inbound(input)?;
        Ok(())
    });
    let channel_request = ChannelHttpRequest {
        method: request.method,
        path: request.path,
        query: request.query,
        headers: request.headers,
        body: request.body,
    };
    for adapter in runtime.adapters.values() {
        if let Some(response) = adapter.handle_http(channel_request.clone(), inbound.clone())? {
            return Ok(Some(response));
        }
    }
    Ok(None)
}

fn handle_inbound_request(body: Vec<u8>, runtime: GatewayRuntime) -> Result<Value> {
    let request: WebhookInboundRequest =
        serde_json::from_slice(&body).context("failed to parse inbound webhook JSON")?;
    if let Some(control) = try_handle_approval_command(&request, &runtime)? {
        return Ok(control);
    }
    let input = inbound_from_webhook_request(request)?;
    let route = runtime.submit_inbound(input)?;
    Ok(json!({
        "status": "accepted",
        "session_id": route.session_id,
        "running": runtime.agent.has_pending_or_running_work(&route.session_id),
    }))
}

fn inbound_from_webhook_request(request: WebhookInboundRequest) -> Result<InboundMessageInput> {
    let channel = request.channel();
    let attachments = request
        .attachments
        .into_iter()
        .map(WebhookAttachmentInput::into_inbound_attachment)
        .collect::<Result<Vec<_>>>()?;
    Ok(InboundMessageInput {
        channel,
        conversation_id: request.conversation_id,
        thread_id: request.thread_id,
        chat_type: request.chat_type,
        sender_id: request.sender_id,
        message_id: request.message_id,
        text: request.text.unwrap_or_default(),
        attachments,
        timestamp: request.timestamp,
    })
}

fn try_handle_approval_command(
    request: &WebhookInboundRequest,
    runtime: &GatewayRuntime,
) -> Result<Option<Value>> {
    let text = request.text.as_deref().unwrap_or_default().trim();
    let key = GatewaySessionKey {
        channel: config::normalize_channel_name(&request.channel()),
        conversation_id: request.conversation_id.clone(),
        thread_id: request.thread_id.clone(),
    };
    try_handle_approval_text(text, &runtime.approvals, Some(&key))
}

fn try_handle_approval_text(
    text: &str,
    approvals: &PendingApprovals,
    key: Option<&GatewaySessionKey>,
) -> Result<Option<Value>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let mut parts = text.split_whitespace();
    let Some(command) = parts.next() else {
        return Ok(None);
    };
    match command {
        "/approve" => {
            let first = parts.next();
            let second = parts.next();
            let (id, decision_text) = if first.is_some_and(is_approval_id) {
                (first, second)
            } else if first == Some("all") {
                (None, second)
            } else {
                (None, first)
            };
            let decision = approval_decision_from_text(decision_text.unwrap_or("once"))
                .filter(|decision| decision.approved())
                .ok_or_else(|| {
                    anyhow!(
                        "unknown approval decision `{}`",
                        decision_text.unwrap_or("once")
                    )
                })?;
            Ok(Some(resolve_approval_control(
                approvals,
                key,
                id,
                decision,
                first == Some("all"),
            )))
        }
        "/deny" => {
            let first = parts.next();
            let id = first.filter(|value| is_approval_id(value));
            Ok(Some(resolve_approval_control(
                approvals,
                key,
                id,
                ApprovalDecision::Forbidden,
                first == Some("all"),
            )))
        }
        _ => Ok(None),
    }
}

fn resolve_approval_control(
    approvals: &PendingApprovals,
    key: Option<&GatewaySessionKey>,
    id: Option<&str>,
    decision: ApprovalDecision,
    resolve_all: bool,
) -> Value {
    if let Some(id) = id {
        let resolved = approvals.resolve(id, decision);
        return approval_control_json(
            if resolved {
                ApprovalResolveStatus::Resolved
            } else {
                ApprovalResolveStatus::NotFound
            },
            Some(id.to_string()),
            decision,
            0,
        );
    }
    let Some(key) = key else {
        return approval_control_json(ApprovalResolveStatus::NotFound, None, decision, 0);
    };
    let result = approvals.resolve_for_key(key, decision, resolve_all);
    approval_control_json(
        result.status,
        result.id,
        result.decision,
        result.pending_count,
    )
}

fn deny_all_pending_approvals_for_key(
    approvals: &PendingApprovals,
    key: &GatewaySessionKey,
) -> Option<Value> {
    let result = approvals.resolve_for_key(key, ApprovalDecision::Forbidden, true);
    (result.status == ApprovalResolveStatus::Resolved).then(|| {
        approval_control_json(
            result.status,
            result.id,
            result.decision,
            result.pending_count,
        )
    })
}

fn approval_control_json(
    status: ApprovalResolveStatus,
    id: Option<String>,
    decision: ApprovalDecision,
    pending_count: usize,
) -> Value {
    json!({
        "status": match status {
            ApprovalResolveStatus::Resolved => "approval_resolved",
            ApprovalResolveStatus::NotFound => "approval_not_found",
        },
        "approval_id": id,
        "decision": approval_label(&decision),
        "pending_count": pending_count,
    })
}

fn is_approval_id(value: &str) -> bool {
    value.starts_with("appr_")
}

fn approval_decision_from_text(text: &str) -> Option<ApprovalDecision> {
    match text.trim().to_ascii_lowercase().as_str() {
        "once" => Some(ApprovalDecision::Once),
        "session" | "ses" => Some(ApprovalDecision::Session),
        "always" | "permanent" | "permanently" => Some(ApprovalDecision::Always),
        _ => None,
    }
}

fn render_approval_prompt_message(command: &str, _id: &str) -> String {
    format!(
        "Approval required.\n\nCommand:\n```\n{command}\n```\n\nReply `/approve`, `/approve session`, `/approve always`, or `/deny`.\n\nMultiple pending approvals are handled oldest-first; use `/approve all`, `/approve all session`, `/approve all always`, or `/deny all` to resolve every pending approval in this chat."
    )
}

fn render_approval_control_response(control: &Value) -> String {
    let id = control["approval_id"].as_str().unwrap_or("approval");
    match control["status"].as_str() {
        Some("approval_resolved") => match control["decision"].as_str().unwrap_or_default() {
            "forbidden" => format!("Approval denied: `{id}`."),
            "" => format!("Approval resolved: `{id}`."),
            decision => {
                let count = control["pending_count"].as_u64().unwrap_or(0);
                if count > 1 {
                    format!("Approved {count} pending commands ({decision}).")
                } else {
                    format!("Approval resolved: `{id}` ({decision}).")
                }
            }
        },
        Some("approval_not_found") => format!("Approval not found or already resolved: `{id}`."),
        _ => format!("Approval command received for `{id}`."),
    }
}

#[derive(Debug, Deserialize)]
struct WebhookInboundRequest {
    #[serde(default)]
    channel: Option<String>,
    pub conversation_id: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub chat_type: Option<String>,
    #[serde(default)]
    pub sender_id: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub attachments: Vec<WebhookAttachmentInput>,
    #[serde(default)]
    pub timestamp: Option<String>,
}

impl WebhookInboundRequest {
    fn channel(&self) -> String {
        self.channel
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("webhook")
            .to_string()
    }
}

#[derive(Debug, Deserialize)]
struct WebhookAttachmentInput {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    content_base64: Option<String>,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    mime: Option<String>,
}

impl WebhookAttachmentInput {
    fn into_inbound_attachment(self) -> Result<InboundAttachmentInput> {
        let bytes = match self
            .content_base64
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            Some(content) => Some(
                base64::engine::general_purpose::STANDARD
                    .decode(content.trim())
                    .context("failed to decode attachment content_base64")?,
            ),
            None => None,
        };
        Ok(InboundAttachmentInput {
            bytes,
            path: self.path,
            filename: self.filename,
            mime: self.mime,
        })
    }
}

fn compose_gateway_user_message(inbound: &InboundMessage) -> String {
    let mut out = String::new();
    out.push_str("[Channel Delivery Context]\n");
    out.push_str(&format!("channel: {}\n", inbound.channel));
    out.push_str(&format!("conversation_id: {}\n", inbound.conversation_id));
    if let Some(thread_id) = inbound.thread_id.as_deref() {
        out.push_str(&format!("thread_id: {thread_id}\n"));
    }
    if let Some(chat_type) = inbound.chat_type.as_deref() {
        out.push_str(&format!("chat_type: {chat_type}\n"));
    }
    out.push_str("Native media delivery is available. To send an attachment, put `MEDIA:/absolute/path/to/file` on its own line. The gateway will deliver supported local files as native channel attachments and remove the directive from visible text.\n\n");
    out.push_str("[User Message]\n");
    if let Some(sender_id) = inbound.sender_id.as_deref() {
        out.push_str(&format!("sender_id: {sender_id}\n"));
    }
    if let Some(message_id) = inbound.message_id.as_deref() {
        out.push_str(&format!("message_id: {message_id}\n"));
    }
    out.push_str(&format!("timestamp: {}\n", inbound.timestamp));
    out.push_str("text:\n");
    out.push_str(inbound.text.trim());
    out.push('\n');
    for attachment in &inbound.attachments {
        out.push_str("\n[User Attachment]\n");
        out.push_str(&format!("id: {}\n", attachment.id));
        out.push_str(&format!("filename: {}\n", attachment.original_filename));
        out.push_str(&format!("mime: {}\n", attachment.mime));
        out.push_str(&format!("size_bytes: {}\n", attachment.size_bytes));
        out.push_str(&format!("sha256: {}\n", attachment.sha256));
        out.push_str(&format!("path: {}\n", attachment.agent_path));
    }
    out.trim_end().to_string()
}

#[derive(Clone)]
struct GatewaySessionStore {
    path: PathBuf,
    mappings: Arc<Mutex<HashMap<GatewaySessionKey, String>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GatewaySessionRecord {
    timestamp: String,
    key: GatewaySessionKey,
    session_id: String,
}

impl GatewaySessionStore {
    fn new(state_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&state_dir).with_context(|| {
            format!(
                "failed to create gateway state dir: {}",
                state_dir.display()
            )
        })?;
        let path = state_dir.join("sessions.jsonl");
        let mut mappings = HashMap::new();
        if path.exists() {
            let text = fs::read_to_string(&path).with_context(|| {
                format!("failed to read gateway session map: {}", path.display())
            })?;
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                let record: GatewaySessionRecord = serde_json::from_str(line)
                    .with_context(|| format!("failed to parse gateway session record: {line}"))?;
                mappings.insert(record.key, record.session_id);
            }
        }
        Ok(Self {
            path,
            mappings: Arc::new(Mutex::new(mappings)),
        })
    }

    fn get_or_create<F>(&self, key: &GatewaySessionKey, create: F) -> Result<String>
    where
        F: FnOnce() -> Result<String>,
    {
        if let Some(session_id) = self
            .mappings
            .lock()
            .expect("gateway session map mutex poisoned")
            .get(key)
            .cloned()
        {
            return Ok(session_id);
        }
        let session_id = create()?;
        {
            let mut guard = self
                .mappings
                .lock()
                .expect("gateway session map mutex poisoned");
            if let Some(existing) = guard.get(key) {
                return Ok(existing.clone());
            }
            guard.insert(key.clone(), session_id.clone());
        }
        let record = GatewaySessionRecord {
            timestamp: now_rfc3339_like(),
            key: key.clone(),
            session_id: session_id.clone(),
        };
        append_jsonl(&self.path, &record)?;
        Ok(session_id)
    }

    fn get(&self, key: &GatewaySessionKey) -> Option<String> {
        self.mappings
            .lock()
            .expect("gateway session map mutex poisoned")
            .get(key)
            .cloned()
    }

    fn key_for_session(&self, session_id: &str) -> Option<GatewaySessionKey> {
        self.mappings
            .lock()
            .expect("gateway session map mutex poisoned")
            .iter()
            .find_map(|(key, value)| (value == session_id).then(|| key.clone()))
    }

    fn set_session(&self, key: &GatewaySessionKey, session_id: String) -> Result<()> {
        self.mappings
            .lock()
            .expect("gateway session map mutex poisoned")
            .insert(key.clone(), session_id.clone());
        let record = GatewaySessionRecord {
            timestamp: now_rfc3339_like(),
            key: key.clone(),
            session_id,
        };
        append_jsonl(&self.path, &record)
    }

    fn session_ids_for_key(&self, key: &GatewaySessionKey) -> Result<Vec<String>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let text = fs::read_to_string(&self.path).with_context(|| {
            format!(
                "failed to read gateway session map: {}",
                self.path.display()
            )
        })?;
        let mut ids = Vec::new();
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let record: GatewaySessionRecord = serde_json::from_str(line)
                .with_context(|| format!("failed to parse gateway session record: {line}"))?;
            if record.key == *key && !ids.contains(&record.session_id) {
                ids.push(record.session_id);
            }
        }
        Ok(ids)
    }
}

#[derive(Clone)]
struct GatewayAttachmentStore {
    attachment_dir: PathBuf,
    blob_dir: PathBuf,
    staging_root: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct AttachmentMetadataRecord {
    timestamp: String,
    attachment: AttachmentRef,
}

impl GatewayAttachmentStore {
    fn new(attachment_dir: PathBuf) -> Result<Self> {
        let blob_dir = attachment_dir.join("blobs").join("sha256");
        let staging_root = std::env::temp_dir()
            .join("duckagent")
            .join("gateway")
            .join("attachments");
        fs::create_dir_all(&blob_dir).with_context(|| {
            format!("failed to create gateway blob dir: {}", blob_dir.display())
        })?;
        fs::create_dir_all(&staging_root)
            .with_context(|| format!("failed to create staging dir: {}", staging_root.display()))?;
        Ok(Self {
            attachment_dir,
            blob_dir,
            staging_root,
        })
    }

    fn ingest_all(
        &self,
        session_id: &str,
        inputs: &[InboundAttachmentInput],
    ) -> Result<Vec<AttachmentRef>> {
        let mut attachments = Vec::new();
        for input in inputs {
            attachments.push(self.ingest_one(session_id, input)?);
        }
        Ok(attachments)
    }

    fn ingest_one(
        &self,
        session_id: &str,
        input: &InboundAttachmentInput,
    ) -> Result<AttachmentRef> {
        let (bytes, fallback_name) = read_attachment_input(input)?;
        let sha256 = sha256_hex(&bytes);
        let filename = input
            .filename
            .as_deref()
            .map(safe_filename)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| fallback_name.unwrap_or_else(|| format!("attachment-{sha256}")));
        let mime = input
            .mime
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| infer_mime_from_name(&filename));
        let ext = Path::new(&filename)
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{value}"))
            .unwrap_or_default();
        let shard = &sha256[..2];
        let blob_path = self.blob_dir.join(shard).join(format!("{sha256}{ext}"));
        if !blob_path.exists() {
            if let Some(parent) = blob_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&blob_path, &bytes)
                .with_context(|| format!("failed to write blob: {}", blob_path.display()))?;
        }
        let attachment_id = format!("att_{}", Uuid::now_v7().simple());
        let stage_dir = self.staging_root.join(session_id).join(&attachment_id);
        fs::create_dir_all(&stage_dir)
            .with_context(|| format!("failed to create stage dir: {}", stage_dir.display()))?;
        let agent_path = stage_dir.join(&filename);
        if fs::hard_link(&blob_path, &agent_path).is_err() {
            fs::copy(&blob_path, &agent_path).with_context(|| {
                format!(
                    "failed to stage attachment from {} to {}",
                    blob_path.display(),
                    agent_path.display()
                )
            })?;
        }
        let attachment = AttachmentRef {
            id: attachment_id,
            original_filename: filename,
            mime,
            size_bytes: bytes.len() as u64,
            storage_path: blob_path.to_string_lossy().to_string(),
            agent_path: agent_path.to_string_lossy().to_string(),
            sha256,
        };
        let record = AttachmentMetadataRecord {
            timestamp: now_rfc3339_like(),
            attachment: attachment.clone(),
        };
        let metadata_path = self
            .attachment_dir
            .join("sessions")
            .join(session_id)
            .join("attachments.jsonl");
        append_jsonl(&metadata_path, &record)?;
        Ok(attachment)
    }
}

fn read_attachment_input(input: &InboundAttachmentInput) -> Result<(Vec<u8>, Option<String>)> {
    if let Some(path) = input
        .path
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let path = expand_host_path(path)?;
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read attachment path: {}", path.display()))?;
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .map(safe_filename);
        return Ok((bytes, filename));
    }
    if let Some(bytes) = input.bytes.as_ref().filter(|value| !value.is_empty()) {
        return Ok((bytes.clone(), None));
    }
    bail!("attachment requires either path or bytes");
}

#[derive(Debug)]
struct OutboundDelivery {
    text: String,
    media_paths: Vec<String>,
}

fn parse_outbound_delivery(text: &str) -> Result<OutboundDelivery> {
    let media_line = Regex::new(r#"^\s*MEDIA:\s*(?P<path>\S+)\s*$"#)?;
    let md_image = Regex::new(r#"!\[[^\]]*\]\((?P<path>[^)]+)\)"#)?;
    let mut media_paths = Vec::new();
    let mut kept_lines = Vec::new();
    for line in text.lines() {
        if let Some(captures) = media_line.captures(line) {
            let raw = captures
                .name("path")
                .map(|value| value.as_str())
                .unwrap_or_default();
            media_paths.push(validate_outbound_media(raw)?);
        } else {
            kept_lines.push(line);
        }
    }
    let without_media_lines = kept_lines.join("\n");
    let mut cleaned = String::new();
    let mut last = 0usize;
    for captures in md_image.captures_iter(&without_media_lines) {
        let Some(full) = captures.get(0) else {
            continue;
        };
        let Some(path) = captures.name("path") else {
            continue;
        };
        media_paths.push(validate_outbound_media(path.as_str().trim())?);
        cleaned.push_str(&without_media_lines[last..full.start()]);
        last = full.end();
    }
    cleaned.push_str(&without_media_lines[last..]);
    Ok(OutboundDelivery {
        text: cleaned.trim().to_string(),
        media_paths,
    })
}

fn validate_outbound_media(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_matches(['`', '"', '\'']);
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Ok(trimmed.to_string());
    }
    let path = expand_host_path(trimmed)?;
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to resolve outbound media path: {}", path.display()))?;
    let workspace = std::env::current_dir()?.canonicalize()?;
    let temp_gateway = std::env::temp_dir()
        .join("duckagent")
        .join("gateway")
        .canonicalize()
        .unwrap_or_else(|_| std::env::temp_dir().join("duckagent").join("gateway"));
    if canonical.starts_with(&workspace) || canonical.starts_with(&temp_gateway) {
        return Ok(canonical.to_string_lossy().to_string());
    }
    bail!(
        "outbound media path is outside the workspace or gateway temp dir: {}",
        canonical.display()
    )
}

fn default_gateway_root_dir() -> Result<PathBuf> {
    crate::profiles::active_profile_path("gateway")
}

fn default_gateway_state_dir() -> Result<PathBuf> {
    Ok(default_gateway_root_dir()?.join("state"))
}

fn default_gateway_sessions_path() -> Result<PathBuf> {
    Ok(default_gateway_state_dir()?.join("sessions.jsonl"))
}

fn default_gateway_pairing_dir() -> Result<PathBuf> {
    Ok(default_gateway_state_dir()?.join("pairing"))
}

fn default_gateway_attachments_dir() -> Result<PathBuf> {
    Ok(default_gateway_root_dir()?.join("attachments"))
}

fn default_gateway_logs_dir() -> Result<PathBuf> {
    Ok(default_gateway_root_dir()?.join("logs"))
}

fn default_gateway_run_dir() -> Result<PathBuf> {
    Ok(default_gateway_root_dir()?.join("run"))
}

#[cfg(any(test, target_os = "windows"))]
fn default_gateway_service_dir() -> Result<PathBuf> {
    Ok(default_gateway_root_dir()?.join("service"))
}

fn default_gateway_channel_state_dir(channel: &str) -> Result<PathBuf> {
    Ok(default_gateway_state_dir()?
        .join("channels")
        .join(config::normalize_channel_name(channel)))
}

fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent dir: {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open jsonl for append: {}", path.display()))?;
    let line = serde_json::to_string(value)?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn expand_host_path(path: &str) -> Result<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        bail!("path must be non-empty");
    }
    if trimmed == "~" {
        return dirs::home_dir().context("failed to expand `~`");
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return Ok(dirs::home_dir().context("failed to expand `~`")?.join(rest));
    }
    let raw = PathBuf::from(trimmed);
    Ok(if raw.is_absolute() {
        raw
    } else {
        std::env::current_dir()?.join(raw)
    })
}

fn safe_filename(value: &str) -> String {
    let name = Path::new(value)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("attachment");
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches(['.', '_']).to_string();
    if trimmed.is_empty() {
        "attachment".to_string()
    } else {
        trimmed
    }
}

fn infer_mime_from_name(filename: &str) -> String {
    match Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" | "md" | "log" => "text/plain",
        "json" => "application/json",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn approval_label(decision: &ApprovalDecision) -> String {
    match decision {
        ApprovalDecision::Once => "once",
        ApprovalDecision::Session => "session",
        ApprovalDecision::Always => "always",
        ApprovalDecision::Forbidden => "forbidden",
    }
    .to_string()
}

fn now_rfc3339_like() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct HttpResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut buffer = Vec::new();
    let mut temp = [0u8; 4096];
    loop {
        let read = stream.read(&mut temp)?;
        if read == 0 {
            bail!("connection closed before headers");
        }
        buffer.extend_from_slice(&temp[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > 1024 * 1024 {
            bail!("request headers too large");
        }
    }
    let header_end = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|idx| idx + 4)
        .context("missing HTTP header terminator")?;
    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = headers.split("\r\n");
    let request_line = lines.next().context("missing request line")?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_string();
    let target = request_parts.next().unwrap_or_default();
    let mut content_length = 0usize;
    let mut headers_vec = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers_vec.push((name.trim().to_string(), value.trim().to_string()));
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().context("invalid content-length")?;
            }
        }
    }
    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut temp)?;
        if read == 0 {
            bail!("connection closed before body completed");
        }
        body.extend_from_slice(&temp[..read]);
    }
    body.truncate(content_length);
    let (path, query) = parse_target(target);
    Ok(HttpRequest {
        method,
        path,
        query,
        headers: headers_vec,
        body,
    })
}

fn parse_target(target: &str) -> (String, HashMap<String, String>) {
    let Some((path, raw_query)) = target.split_once('?') else {
        return (target.to_string(), HashMap::new());
    };
    let mut query = HashMap::new();
    for pair in raw_query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(key.to_string(), value.to_string());
    }
    (path.to_string(), query)
}

fn write_http_response(stream: &mut TcpStream, response: HttpResponse) -> Result<()> {
    let status_text = match response.status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        status_text,
        response.content_type,
        response.body.len()
    )?;
    stream.write_all(&response.body)?;
    Ok(())
}

fn json_response(status: u16, value: Value) -> HttpResponse {
    HttpResponse {
        status,
        content_type: "application/json",
        body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"error\":\"json\"}".to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    struct FeedbackTestAdapter {
        typing: bool,
        editable: bool,
        events: Arc<StdMutex<Vec<String>>>,
    }

    impl FeedbackTestAdapter {
        fn new(typing: bool, editable: bool) -> Self {
            Self {
                typing,
                editable,
                events: Arc::new(StdMutex::new(Vec::new())),
            }
        }

        fn events(&self) -> Arc<StdMutex<Vec<String>>> {
            self.events.clone()
        }
    }

    impl ChannelAdapter for FeedbackTestAdapter {
        fn start(&self, _inbound: GatewayInboundDispatch) -> Result<()> {
            Ok(())
        }

        fn send_message(&self, _route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
            self.events
                .lock()
                .unwrap()
                .push(format!("message:{}", message.text));
            Ok(())
        }

        fn send_stream_start(
            &self,
            _route: &GatewayRoute,
            text: &str,
        ) -> Result<Option<StreamMessageHandle>> {
            self.events.lock().unwrap().push(format!("stream:{text}"));
            Ok(self.editable.then(|| StreamMessageHandle {
                message_id: "stream1".to_string(),
            }))
        }

        fn update_stream(
            &self,
            _route: &GatewayRoute,
            _handle: &StreamMessageHandle,
            _text: &str,
            _final_update: bool,
        ) -> Result<()> {
            Ok(())
        }

        fn send_typing(&self, _route: &GatewayRoute, event: TypingEvent) -> Result<()> {
            self.events
                .lock()
                .unwrap()
                .push(format!("typing:{}", event.active));
            Ok(())
        }

        fn send_approval_prompt(
            &self,
            _route: &GatewayRoute,
            _prompt: GatewayApprovalPrompt,
        ) -> Result<()> {
            Ok(())
        }

        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities {
                media: false,
                typing: self.typing,
                approval_prompt: false,
            }
        }
    }

    #[test]
    fn gateway_default_paths_do_not_live_under_cache() -> Result<()> {
        let root = default_gateway_root_dir()?;
        let profile_root = crate::profiles::active_profile_dir()?;
        assert_eq!(root.strip_prefix(&profile_root)?, Path::new("gateway"));

        let sessions = default_gateway_sessions_path()?;
        assert_eq!(
            sessions.strip_prefix(&root)?,
            Path::new("state").join("sessions.jsonl")
        );
        assert_eq!(
            default_gateway_pairing_dir()?.strip_prefix(&root)?,
            Path::new("state").join("pairing")
        );
        assert_eq!(
            default_gateway_channel_state_dir("whatsapp")?.strip_prefix(&root)?,
            Path::new("state").join("channels").join("whatsapp")
        );
        assert_eq!(
            default_gateway_attachments_dir()?.strip_prefix(&root)?,
            Path::new("attachments")
        );
        assert_eq!(
            default_gateway_logs_dir()?.strip_prefix(&root)?,
            Path::new("logs")
        );
        assert_eq!(
            default_gateway_run_dir()?.strip_prefix(&root)?,
            Path::new("run")
        );
        assert_eq!(
            default_gateway_service_dir()?.strip_prefix(&root)?,
            Path::new("service")
        );
        Ok(())
    }

    #[test]
    fn gateway_log_extracts_visible_user_text() {
        let raw = "[Channel Delivery Context]\nchannel: lark\nconversation_id: chat1\n\n[User Message]\nsender_id: u1\ntimestamp: 2026-05-16T00:00:00Z\ntext:\nhello from chat\n\n[User Attachment]\nid: att1\n";
        assert_eq!(
            extract_gateway_user_text(raw).as_deref(),
            Some("hello from chat")
        );
    }

    #[test]
    fn gateway_log_formats_main_user_and_assistant_messages() -> Result<()> {
        let tail = GatewayLogTail {
            session_id: "s1".to_string(),
            label: "lark:chat1".to_string(),
            path: PathBuf::from("unused"),
            offset: 0,
        };
        let user = SessionLine {
            timestamp: "2026-05-16T00:00:00Z".to_string(),
            entry: SessionEntry::ResponseItem(SessionManager::new_text_message(
                SessionRole::User,
                "[Channel Delivery Context]\nchannel: lark\nconversation_id: chat1\n\n[User Message]\ntimestamp: now\ntext:\nhi",
            )),
        };
        let assistant = SessionLine {
            timestamp: "2026-05-16T00:00:01Z".to_string(),
            entry: SessionEntry::ResponseItem(SessionManager::new_text_message(
                SessionRole::Assistant,
                "hello",
            )),
        };

        assert_eq!(
            format_gateway_log_line(&tail, user).as_deref(),
            Some("[2026-05-16T00:00:00Z] [lark:chat1] user: hi")
        );
        assert_eq!(
            format_gateway_log_line(&tail, assistant).as_deref(),
            Some("[2026-05-16T00:00:01Z] [lark:chat1] assistant: hello")
        );
        Ok(())
    }

    #[test]
    fn gateway_log_tails_existing_sessions_from_end() -> Result<()> {
        let temp = TempDir::new()?;
        let manager = SessionManager::new(temp.path().join("duckagent"))?;
        let session_id = manager.create_session(None, "system")?;
        manager.append_text_message(&session_id, SessionRole::User, "before")?;

        let mut tails = HashMap::new();
        let record = GatewaySessionRecord {
            timestamp: "now".to_string(),
            key: GatewaySessionKey {
                channel: "lark".to_string(),
                conversation_id: "chat1".to_string(),
                thread_id: None,
            },
            session_id: session_id.clone(),
        };
        upsert_gateway_log_tail(&mut tails, &manager, record, true)?;
        let tail = tails.get_mut(&session_id).expect("tail");
        assert!(read_session_lines_from_tail(tail)?.is_empty());

        manager.append_text_message(&session_id, SessionRole::Assistant, "after")?;
        let lines = read_session_lines_from_tail(tail)?;
        assert_eq!(lines.len(), 1);
        let display =
            format_gateway_log_line(tail, lines.into_iter().next().unwrap()).expect("display line");
        assert!(display.contains("[lark:chat1] assistant: after"));
        Ok(())
    }

    fn feedback_test_route() -> GatewayRoute {
        GatewayRoute {
            session_id: "session1".to_string(),
            key: GatewaySessionKey {
                channel: "test".to_string(),
                conversation_id: "chat1".to_string(),
                thread_id: None,
            },
        }
    }

    #[test]
    fn bind_listener_falls_back_to_random_port_when_allowed() -> Result<()> {
        let occupied = TcpListener::bind("127.0.0.1:0")?;
        let occupied_addr = occupied.local_addr()?;

        let listener = bind_gateway_listener(occupied_addr, true)?;
        assert_eq!(listener.local_addr()?.ip(), occupied_addr.ip());
        assert_ne!(listener.local_addr()?.port(), occupied_addr.port());
        Ok(())
    }

    #[test]
    fn bind_listener_preserves_addr_in_use_for_stable_channels() -> Result<()> {
        let occupied = TcpListener::bind("127.0.0.1:0")?;
        let occupied_addr = occupied.local_addr()?;

        let error = bind_gateway_listener(occupied_addr, false).unwrap_err();
        assert!(format!("{error:#}").contains("failed to bind gateway server"));
        Ok(())
    }

    #[test]
    fn immediate_feedback_prefers_native_typing() {
        let adapter = FeedbackTestAdapter::new(true, true);
        let events = adapter.events();
        let streams = GatewayStreamStateStore::default();
        start_immediate_feedback(
            Arc::new(adapter),
            &streams,
            Arc::new(Mutex::new(HashMap::new())),
            feedback_test_route(),
            "test",
        );

        assert_eq!(events.lock().unwrap().as_slice(), ["typing:true"]);
        assert!(streams.inner.lock().unwrap().is_empty());
    }

    #[test]
    fn immediate_feedback_uses_editable_placeholder_without_typing() {
        let adapter = FeedbackTestAdapter::new(false, true);
        let events = adapter.events();
        let streams = GatewayStreamStateStore::default();
        start_immediate_feedback(
            Arc::new(adapter),
            &streams,
            Arc::new(Mutex::new(HashMap::new())),
            feedback_test_route(),
            "test",
        );

        assert_eq!(events.lock().unwrap().as_slice(), ["stream:Thinking..."]);
        let guard = streams.inner.lock().unwrap();
        let state = guard.get("session1").expect("stream state");
        assert_eq!(state.last_sent, GATEWAY_THINKING_PLACEHOLDER);
        assert!(state.delivered);
        assert!(state.handle.is_some());
    }

    #[test]
    fn immediate_feedback_falls_back_to_standalone_message() {
        let adapter = FeedbackTestAdapter::new(false, false);
        let events = adapter.events();
        let streams = GatewayStreamStateStore::default();
        start_immediate_feedback(
            Arc::new(adapter),
            &streams,
            Arc::new(Mutex::new(HashMap::new())),
            feedback_test_route(),
            "test",
        );

        assert_eq!(
            events.lock().unwrap().as_slice(),
            ["stream:Thinking...", "message:Thinking..."]
        );
        assert!(streams.inner.lock().unwrap().is_empty());
    }

    #[test]
    fn outbound_media_lines_are_stripped_and_validated() -> Result<()> {
        let dir = std::env::current_dir()?
            .join("target")
            .join("duckagent-gateway-tests")
            .join(Uuid::now_v7().to_string());
        fs::create_dir_all(&dir)?;
        let image = dir.join("image.png");
        fs::write(&image, b"x")?;
        let delivery =
            parse_outbound_delivery(&format!("hello\nMEDIA:{}\nworld", image.display()))?;
        assert_eq!(delivery.text, "hello\nworld");
        assert_eq!(delivery.media_paths.len(), 1);
        assert!(delivery.media_paths[0].ends_with("image.png"));
        Ok(())
    }

    #[test]
    fn stream_preview_strips_media_directives() {
        let cleaned = clean_stream_preview_text(
            "hello\nMEDIA:/tmp/duckagent/gateway/a.png\n![img](/tmp/duckagent/gateway/a.png)\nworld",
        );
        assert_eq!(cleaned, "hello\n\nworld");
    }

    #[test]
    fn stream_split_leaves_remainder_for_followup_send() {
        let (first, rest) = split_stream_text("abcdef", 5);
        assert_eq!(first, "a\n\n[continued below]");
        assert_eq!(rest, "bcdef");
    }

    #[test]
    fn stream_fallback_remainder_sends_only_unshown_tail() {
        let tail = stream_fallback_remainder("hello world", "hello", "");
        assert_eq!(tail, "world");
    }

    #[test]
    fn stream_fallback_remainder_handles_split_suffix() {
        let tail =
            stream_fallback_remainder("abcdef", &format!("ab{STREAM_CONTINUED_SUFFIX}"), "cdef");
        assert_eq!(tail, "cdef");
    }

    #[test]
    fn attachment_store_writes_blob_metadata_and_stage_file() -> Result<()> {
        let base = TempDir::new()?;
        let store = GatewayAttachmentStore::new(base.path().join("gateway"))?;
        let attachments = vec![InboundAttachmentInput {
            bytes: Some(b"abc".to_vec()),
            path: None,
            filename: Some("pic.png".to_string()),
            mime: Some("image/png".to_string()),
        }];
        let attachments = store.ingest_all("session1", &attachments)?;
        assert_eq!(attachments.len(), 1);
        assert!(Path::new(&attachments[0].storage_path).exists());
        assert!(Path::new(&attachments[0].agent_path).exists());
        assert_eq!(attachments[0].mime, "image/png");
        Ok(())
    }

    #[test]
    fn webhook_request_decodes_base64_attachments() -> Result<()> {
        let request = WebhookInboundRequest {
            channel: Some("webhook".to_string()),
            conversation_id: "c1".to_string(),
            thread_id: None,
            chat_type: None,
            sender_id: None,
            message_id: None,
            text: Some("hi".to_string()),
            attachments: vec![WebhookAttachmentInput {
                path: None,
                content_base64: Some(base64::engine::general_purpose::STANDARD.encode(b"abc")),
                filename: Some("pic.png".to_string()),
                mime: Some("image/png".to_string()),
            }],
            timestamp: None,
        };
        let input = inbound_from_webhook_request(request)?;
        assert_eq!(input.attachments.len(), 1);
        assert_eq!(
            input.attachments[0].bytes.as_deref(),
            Some(b"abc".as_slice())
        );
        Ok(())
    }

    #[test]
    fn openai_chat_messages_are_labeled_for_gateway_dispatch() -> Result<()> {
        let messages = vec![
            json!({"role": "system", "content": "You are concise."}),
            json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "look at this"},
                    {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
                ]
            }),
        ];
        let text = compose_openai_messages(&messages)?;
        assert!(text.starts_with("[OpenAI Chat Completion Request]"));
        assert!(text.contains("[system]\nYou are concise."));
        assert!(text.contains("[user]\nlook at this\n[Image URL]\nhttps://example.com/a.png"));
        Ok(())
    }

    #[test]
    fn openai_file_parts_are_rejected_until_attachment_support_is_native() {
        let err = normalize_openai_content(&json!([
            {"type": "input_file", "file_id": "file_123"}
        ]))
        .expect_err("input_file parts must not be silently dropped");
        assert!(format!("{err:#}").contains("file content parts are not supported"));
    }

    #[test]
    fn gateway_session_store_reuses_existing_mapping() -> Result<()> {
        let base = TempDir::new()?;
        let store = GatewaySessionStore::new(base.path().join("gateway"))?;
        let key = GatewaySessionKey {
            channel: "webhook".to_string(),
            conversation_id: "c1".to_string(),
            thread_id: None,
        };
        let first = store.get_or_create(&key, || Ok("s1".to_string()))?;
        let second = store.get_or_create(&key, || Ok("s2".to_string()))?;
        assert_eq!(first, "s1");
        assert_eq!(second, "s1");
        assert_eq!(store.get(&key).as_deref(), Some("s1"));
        Ok(())
    }

    #[test]
    fn approval_prompt_matches_duckagent_gateway_commands() {
        let message = render_approval_prompt_message("ls /tmp", "appr_1");
        assert!(message.contains("/approve"));
        assert!(message.contains("/approve session"));
        assert!(message.contains("/approve always"));
        assert!(message.contains("/deny"));
        assert!(message.contains("/approve all"));
        assert!(message.contains("/deny all"));
        assert!(!message.contains("`1`"));
        assert!(!message.contains("once|session|always"));
    }

    #[test]
    fn approval_control_response_is_user_visible() {
        let resolved = json!({
            "status": "approval_resolved",
            "approval_id": "appr_1",
            "decision": "once"
        });
        assert_eq!(
            render_approval_control_response(&resolved),
            "Approval resolved: `appr_1` (once)."
        );
        let missing = json!({
            "status": "approval_not_found",
            "approval_id": "appr_2"
        });
        assert!(render_approval_control_response(&missing).contains("not found"));
    }

    #[test]
    fn approve_resolves_oldest_current_chat_pending_approval() -> Result<()> {
        let approvals = PendingApprovals::new();
        let key = GatewaySessionKey {
            channel: "lark".to_string(),
            conversation_id: "oc_chat".to_string(),
            thread_id: None,
        };
        let route = GatewayRoute {
            session_id: "session1".to_string(),
            key: key.clone(),
        };
        let (tx, rx) = mpsc::channel();
        approvals.insert("appr_1".to_string(), route, tx);

        let control =
            try_handle_approval_text("/approve", &approvals, Some(&key))?.expect("approve command");
        assert_eq!(control["status"].as_str(), Some("approval_resolved"));
        assert_eq!(control["approval_id"].as_str(), Some("appr_1"));
        assert!(matches!(rx.recv()?.decision, ApprovalDecision::Once));
        Ok(())
    }

    #[test]
    fn approve_all_resolves_multiple_current_chat_pending_approvals() -> Result<()> {
        let approvals = PendingApprovals::new();
        let key = GatewaySessionKey {
            channel: "lark".to_string(),
            conversation_id: "oc_chat".to_string(),
            thread_id: None,
        };
        let mut receivers = Vec::new();
        for id in ["appr_1", "appr_2"] {
            let (tx, rx) = mpsc::channel();
            receivers.push(rx);
            approvals.insert(
                id.to_string(),
                GatewayRoute {
                    session_id: format!("session-{id}"),
                    key: key.clone(),
                },
                tx,
            );
        }

        let control = try_handle_approval_text("/approve all session", &approvals, Some(&key))?
            .expect("approve all command");
        assert_eq!(control["status"].as_str(), Some("approval_resolved"));
        assert_eq!(control["pending_count"].as_u64(), Some(2));
        for rx in receivers {
            assert!(matches!(rx.recv()?.decision, ApprovalDecision::Session));
        }
        Ok(())
    }

    #[test]
    fn new_input_auto_denies_all_current_chat_pending_approvals() -> Result<()> {
        let approvals = PendingApprovals::new();
        let key = GatewaySessionKey {
            channel: "lark".to_string(),
            conversation_id: "oc_chat".to_string(),
            thread_id: None,
        };
        let other_key = GatewaySessionKey {
            channel: "lark".to_string(),
            conversation_id: "other_chat".to_string(),
            thread_id: None,
        };
        let mut receivers = Vec::new();
        for id in ["appr_1", "appr_2"] {
            let (tx, rx) = mpsc::channel();
            receivers.push(rx);
            approvals.insert(
                id.to_string(),
                GatewayRoute {
                    session_id: format!("session-{id}"),
                    key: key.clone(),
                },
                tx,
            );
        }
        let (other_tx, other_rx) = mpsc::channel();
        approvals.insert(
            "appr_other".to_string(),
            GatewayRoute {
                session_id: "session-other".to_string(),
                key: other_key,
            },
            other_tx,
        );

        let control =
            deny_all_pending_approvals_for_key(&approvals, &key).expect("pending approvals");
        assert_eq!(control["status"].as_str(), Some("approval_resolved"));
        assert_eq!(control["decision"].as_str(), Some("forbidden"));
        assert_eq!(control["pending_count"].as_u64(), Some(2));
        for rx in receivers {
            assert!(matches!(
                rx.recv_timeout(Duration::from_secs(1))?.decision,
                ApprovalDecision::Forbidden
            ));
        }
        assert!(other_rx.try_recv().is_err());
        Ok(())
    }
}
