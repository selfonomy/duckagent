use crate::approval::{ApprovalPolicy, ApprovalProvider};
use crate::capabilities::builtins::{
    self, BuiltinExecutionContext, main_agent::MainAgentBuiltinExecutor, memory as memory_builtin,
};
use crate::capabilities::registry::{
    PermissionContext, RuntimeToolExecutionContext, RuntimeToolRegistry, RuntimeToolSource,
};
use crate::character_card::active_profile_context_blocks;
use crate::client::{ModelClient, StreamUpdate};
use crate::context_projection::{ContextProjectionPolicy, project_main_history};
use crate::instructions::{
    DynamicContextBlock, compose_main_user_message_with_context, compose_tool_result,
    path_candidate_dirs_for_runtime_tool,
};
use crate::memory::MemoryStore;
use crate::provider::{RuntimeProvider, resolve_runtime_context_window};
use crate::session::{
    RewindListItem, RewindResult, SessionManager, SessionMessage, SessionRole, SessionRuleHit,
};
use crate::tools::{
    AgentToolCallbacks, CallCapabilityInput, MessageType, UiMessage, agent_tools,
    call_capability_shared_context, model_response_to_outcome,
};
use crate::utils::{count_tool_tokens_cl100k, estimate_tokens_rough};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

const MEMORY_REVIEW_BASE_INTERVAL_TURNS: usize = 4;
const MEMORY_REVIEW_MAX_INTERVAL_TURNS: usize = 8;
const MEMORY_AGENT_USER_MESSAGE_TEMPLATE: &str =
    include_str!("prompts/memory-agent-user-message.md");
const MAIN_CAPABILITY_INDEX_TEMPLATE: &str = include_str!("prompts/main-capability-index.md");
const MAIN_CAPABILITY_BUILTIN_ITEM_TEMPLATE: &str =
    include_str!("prompts/main-capability-builtin-item.md");
const MAIN_CAPABILITY_MCP_ITEM_TEMPLATE: &str = include_str!("prompts/main-capability-mcp-item.md");
const MAIN_CAPABILITY_MCP_EMPTY_TEMPLATE: &str =
    include_str!("prompts/main-capability-mcp-empty.md");
const SESSION_TITLE_SYSTEM_PROMPT: &str = include_str!("prompts/session-title-system.md");
#[derive(Debug, Clone)]
pub enum AgentEvent {
    StatusChanged {
        session_id: String,
        status: String,
    },
    SessionTitleUpdated {
        session_id: String,
        title: String,
    },
    Message {
        session_id: String,
        message: UiMessage,
    },
    StreamDelta {
        session_id: String,
        delta: String,
    },
    StreamToolCallDelta {
        session_id: String,
        delta: String,
    },
    ApprovalRequested {
        session_id: String,
        command: String,
        rule_hits: Vec<SessionRuleHit>,
    },
    ApprovalDecided {
        session_id: String,
        command: String,
        decision: String,
        approved: bool,
    },
    Error {
        session_id: String,
        message: String,
    },
    MainTurnStarted {
        session_id: String,
    },
    MainTurnFinished {
        session_id: String,
    },
    CronRunFinished {
        session_id: String,
        job_id: String,
        run_id: String,
        status: String,
        error: Option<String>,
    },
}

#[derive(Clone, Default)]
pub struct AgentEventBus {
    subscribers: Arc<Mutex<Vec<mpsc::Sender<AgentEvent>>>>,
}

impl AgentEventBus {
    pub fn subscribe(&self) -> mpsc::Receiver<AgentEvent> {
        let (tx, rx) = mpsc::channel();
        self.subscribers
            .lock()
            .expect("agent event subscribers mutex poisoned")
            .push(tx);
        rx
    }

    pub fn publish(&self, event: AgentEvent) {
        let mut subscribers = self
            .subscribers
            .lock()
            .expect("agent event subscribers mutex poisoned");
        subscribers.retain(|tx| tx.send(event.clone()).is_ok());
    }
}

#[derive(Default)]
struct RuntimeState {
    sessions: HashMap<String, SessionRuntimeState>,
}

#[derive(Default)]
struct SessionRuntimeState {
    main_turn_running: bool,
    pending_user_inputs: VecDeque<PendingUserInput>,
    pending_user_steers: Vec<SubmittedUserMessage>,
}

struct PendingUserInput {
    message: SubmittedUserMessage,
    approval_provider: Arc<dyn ApprovalProvider>,
}

#[derive(Debug, Clone)]
pub struct SubmittedUserMessage {
    pub text: String,
    pub source: UserMessageSource,
}

impl SubmittedUserMessage {
    pub fn tui(text: String) -> Self {
        Self {
            text,
            source: UserMessageSource::Tui,
        }
    }

    pub fn gateway(text: String, metadata: GatewayUserMessageMetadata) -> Self {
        Self {
            text,
            source: UserMessageSource::Gateway(metadata),
        }
    }

    pub fn cron(text: String, metadata: CronUserMessageMetadata) -> Self {
        Self {
            text,
            source: UserMessageSource::Cron(metadata),
        }
    }
}

#[derive(Debug, Clone)]
pub enum UserMessageSource {
    Tui,
    Gateway(GatewayUserMessageMetadata),
    Cron(CronUserMessageMetadata),
}

#[derive(Debug, Clone)]
pub struct GatewayUserMessageMetadata {
    pub channel: String,
    pub conversation_id: String,
    pub thread_id: Option<String>,
    pub sender_id: Option<String>,
    pub message_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CronUserMessageMetadata {
    pub job_id: String,
    pub run_id: String,
    pub scheduled_for: String,
}

#[derive(Default)]
struct MemoryReviewState {
    pending_purposes: HashMap<String, Vec<String>>,
    turns_since_review: HashMap<String, usize>,
    intervals: HashMap<String, usize>,
    running_sessions: HashSet<String>,
}

struct MemoryReviewDispatch {
    purposes: Vec<String>,
}

struct MainDispatch {
    composed_user_text: String,
    source: UserMessageSource,
    approval_provider: Arc<dyn ApprovalProvider>,
}

#[derive(Clone)]
pub struct AgentRuntime {
    client: ModelClient,
    session_manager: SessionManager,
    approval_policy: Arc<Mutex<ApprovalPolicy>>,
    title_requested_sessions: Arc<Mutex<HashSet<String>>>,
    event_bus: AgentEventBus,
    state: Arc<Mutex<RuntimeState>>,
    memory_store: MemoryStore,
    memory_reviews: Arc<Mutex<MemoryReviewState>>,
}

impl AgentRuntime {
    pub fn new(client: ModelClient, session_manager: SessionManager) -> Result<Self> {
        let approval_policy = Arc::new(Mutex::new(ApprovalPolicy::load_default()?));

        Ok(Self {
            client,
            session_manager,
            approval_policy,
            title_requested_sessions: Arc::new(Mutex::new(HashSet::new())),
            event_bus: AgentEventBus::default(),
            state: Arc::new(Mutex::new(RuntimeState::default())),
            memory_store: MemoryStore::new_default()?,
            memory_reviews: Arc::new(Mutex::new(MemoryReviewState::default())),
        })
    }

    pub fn runtime(&self) -> &RuntimeProvider {
        self.client.runtime()
    }

    pub fn with_runtime(&self, runtime: RuntimeProvider) -> Result<Self> {
        let client = ModelClient::from_runtime(runtime)?;
        Self::new(client, self.session_manager.clone())
    }

    pub fn update_session_runtime(
        &self,
        session_id: &str,
        runtime: crate::provider::SessionRuntimeConfig,
    ) -> Result<()> {
        self.session_manager
            .update_runtime_config(session_id, runtime)
    }

    pub fn session_title(&self, session_id: &str) -> Option<String> {
        self.session_manager.get_session_title(session_id).ok()
    }

    pub fn create_session_with_title(
        &self,
        title: Option<&str>,
        created_by: &str,
    ) -> Result<String> {
        self.session_manager.create_session_with_runtime_and_source(
            title,
            crate::SYSTEM_PROMPT,
            self.runtime().session_config(),
            created_by,
        )
    }

    pub fn list_main_session_metas(&self) -> Result<Vec<crate::session::SessionMeta>> {
        self.session_manager.list_main_session_metas()
    }

    pub fn main_context_tokens(&self, session_id: &str) -> Option<usize> {
        self.projected_main_model_messages(session_id)
            .ok()
            .map(|messages| estimate_model_messages_tokens(&messages))
    }

    pub fn subscribe(&self) -> mpsc::Receiver<AgentEvent> {
        self.event_bus.subscribe()
    }

    pub fn submit_user_turn(
        &self,
        session_id: String,
        user_text: String,
        approval_provider: Arc<dyn ApprovalProvider>,
    ) {
        self.submit_user_message(
            session_id,
            SubmittedUserMessage::tui(user_text),
            approval_provider,
        );
    }

    pub fn submit_user_message(
        &self,
        session_id: String,
        user_message: SubmittedUserMessage,
        approval_provider: Arc<dyn ApprovalProvider>,
    ) {
        self.maybe_generate_title(&session_id, user_message.text.clone());
        let should_start = {
            let mut state = self.state.lock().expect("runtime state mutex poisoned");
            let session_state = state.sessions.entry(session_id.clone()).or_default();
            enqueue_user_message(session_state, user_message, approval_provider)
        };
        if should_start {
            self.maybe_start_next_main_turn(session_id);
        }
    }

    pub fn clear_session_runtime_state(&self, session_id: &str) {
        self.state
            .lock()
            .expect("runtime state mutex poisoned")
            .sessions
            .remove(session_id);
        self.title_requested_sessions
            .lock()
            .expect("title requested sessions mutex poisoned")
            .remove(session_id);
    }

    pub fn has_pending_or_running_work(&self, session_id: &str) -> bool {
        {
            let mut state = self.state.lock().expect("runtime state mutex poisoned");
            let Some(session_state) = state.sessions.get_mut(session_id) else {
                return false;
            };
            session_state.main_turn_running
                || !session_state.pending_user_inputs.is_empty()
                || !session_state.pending_user_steers.is_empty()
        }
    }

    pub fn rewind_list(&self, session_id: &str) -> Result<Vec<RewindListItem>> {
        self.session_manager.rewind_list(session_id)
    }

    pub fn rewind_session(&self, session_id: &str, index: usize) -> Result<RewindResult> {
        if self.has_pending_or_running_work(session_id) {
            bail!("rewind is available after the current turn finishes");
        }
        let result = self
            .session_manager
            .rewind_to_user_turn(session_id, index)?;
        self.clear_session_runtime_state(session_id);
        Ok(result)
    }

    fn maybe_start_next_main_turn(&self, session_id: String) {
        let dispatch = {
            let mut state = self.state.lock().expect("runtime state mutex poisoned");
            let session_state = state.sessions.entry(session_id.clone()).or_default();
            if session_state.main_turn_running {
                return;
            }
            if session_state.pending_user_inputs.is_empty() {
                return;
            }

            let user_message = session_state
                .pending_user_inputs
                .pop_front()
                .expect("pending user input checked above");
            let composed_user_text =
                build_main_user_message(Some(user_message.message.text.as_str()));
            session_state.main_turn_running = true;
            MainDispatch {
                composed_user_text,
                source: user_message.message.source,
                approval_provider: user_message.approval_provider,
            }
        };

        let runtime = self.clone();
        thread::spawn(move || {
            runtime.event_bus.publish(AgentEvent::MainTurnStarted {
                session_id: session_id.clone(),
            });
            runtime.event_bus.publish(AgentEvent::StatusChanged {
                session_id: session_id.clone(),
                status: "Agent is thinking...".to_string(),
            });

            let result = (|| -> Result<()> {
                runtime.run_main_turn(
                    &session_id,
                    &dispatch.composed_user_text,
                    dispatch.approval_provider.clone(),
                )
            })();

            if let Err(err) = &result {
                runtime.event_bus.publish(AgentEvent::Error {
                    session_id: session_id.clone(),
                    message: format!("{err:#}"),
                });
            }
            if let UserMessageSource::Cron(metadata) = &dispatch.source {
                runtime.event_bus.publish(AgentEvent::CronRunFinished {
                    session_id: session_id.clone(),
                    job_id: metadata.job_id.clone(),
                    run_id: metadata.run_id.clone(),
                    status: if result.is_ok() {
                        "ok".to_string()
                    } else {
                        "error".to_string()
                    },
                    error: result.as_ref().err().map(|err| format!("{err:#}")),
                });
            }

            {
                let mut state = runtime.state.lock().expect("runtime state mutex poisoned");
                let session_state = state.sessions.entry(session_id.clone()).or_default();
                if !session_state.pending_user_steers.is_empty() {
                    let stranded_steers = std::mem::take(&mut session_state.pending_user_steers);
                    session_state
                        .pending_user_inputs
                        .extend(stranded_steers.into_iter().map(|message| PendingUserInput {
                            message,
                            approval_provider: dispatch.approval_provider.clone(),
                        }));
                }
                session_state.main_turn_running = false;
            }
            runtime.event_bus.publish(AgentEvent::StatusChanged {
                session_id: session_id.clone(),
                status: "Ready".to_string(),
            });
            runtime.event_bus.publish(AgentEvent::MainTurnFinished {
                session_id: session_id.clone(),
            });
            runtime.maybe_start_memory_review_after_main_turn(session_id.clone());
            runtime.maybe_start_next_main_turn(session_id);
        });
    }

    fn run_main_turn(
        &self,
        session_id: &str,
        user_text: &str,
        approval_provider: Arc<dyn ApprovalProvider>,
    ) -> Result<()> {
        {
            let mut policy = self
                .approval_policy
                .lock()
                .expect("shell policy mutex poisoned");
            policy.clear_loop_forbidden();
        }

        let existing_session_messages = self.session_manager.get_all_messages(session_id)?;
        let registry = RuntimeToolRegistry::load_best_effort_for_session(approval_provider.clone());
        let capability_index = render_main_capability_index(&registry);
        let mut extra_blocks = active_profile_context_blocks()?;
        extra_blocks.push(current_time_context_block());
        extra_blocks.push(self.memory_store.active_memory_block()?);
        let rendered_user_text = compose_main_user_message_with_context(
            user_text,
            &existing_session_messages,
            Some(capability_index),
            extra_blocks,
        )?;
        let user_message = self.session_manager.append_text_message(
            session_id,
            SessionRole::User,
            rendered_user_text,
        )?;
        self.session_manager
            .append_user_turn_marker(session_id, user_message.id(), user_text)?;

        self.run_main_agent_loop(session_id, approval_provider, registry)?;

        Ok(())
    }

    fn run_main_agent_loop(
        &self,
        session_id: &str,
        approval_provider: Arc<dyn ApprovalProvider>,
        registry: RuntimeToolRegistry,
    ) -> Result<()> {
        let shared_context = Arc::new(Mutex::new(Vec::<String>::new()));
        let instruction_seen = Arc::new(Mutex::new(HashSet::<(String, String)>::new()));
        loop {
            let agent_messages = self.projected_main_model_messages(session_id)?;
            let runtime = self.clone();
            let main_session_id = session_id.to_string();
            let callback_session_id = main_session_id.clone();
            let main_approval = approval_provider.clone();
            let runtime_registry = registry.clone();
            let runtime_shared_context = shared_context.clone();
            let runtime_instruction_seen = instruction_seen.clone();
            let callbacks = AgentToolCallbacks {
                call_capability: Arc::new(move |input| {
                    runtime.run_main_call_capability(
                        &callback_session_id,
                        input,
                        main_approval.clone(),
                        &runtime_registry,
                        runtime_shared_context.clone(),
                        runtime_instruction_seen.clone(),
                    )
                }),
            };

            let event_bus = self.event_bus.clone();
            let model_response = self.client.generate_streaming_with_tools_single_step(
                agent_messages,
                agent_tools(callbacks),
                move |update| match update {
                    StreamUpdate::Status(status) => {
                        event_bus.publish(AgentEvent::StatusChanged {
                            session_id: main_session_id.clone(),
                            status,
                        });
                    }
                    StreamUpdate::TextDelta(delta) => {
                        event_bus.publish(AgentEvent::StreamDelta {
                            session_id: main_session_id.clone(),
                            delta,
                        });
                    }
                    StreamUpdate::ToolTurnText(content) => {
                        if let Some(message) = ui_message_for_tool_turn_text(&content) {
                            event_bus.publish(AgentEvent::Message {
                                session_id: main_session_id.clone(),
                                message,
                            });
                        }
                    }
                    StreamUpdate::ToolCallDelta(delta) => {
                        event_bus.publish(AgentEvent::StreamToolCallDelta {
                            session_id: main_session_id.clone(),
                            delta,
                        });
                    }
                },
            )?;
            let outcome = model_response_to_outcome(model_response)?;
            let had_tool_activity = outcome
                .session_messages
                .iter()
                .any(is_tool_activity_message);

            for ui_message in outcome.ui_messages {
                self.event_bus.publish(AgentEvent::Message {
                    session_id: session_id.to_string(),
                    message: ui_message,
                });
            }

            for session_message in outcome.session_messages {
                self.session_manager
                    .append_message(session_id, &session_message)?;
                self.emit_approval_events(session_id, &session_message);
            }

            if let Some(final_answer) = outcome.final_answer {
                let assistant_message = self.session_manager.append_text_message(
                    session_id,
                    SessionRole::Assistant,
                    &final_answer,
                )?;

                self.event_bus.publish(AgentEvent::Message {
                    session_id: session_id.to_string(),
                    message: UiMessage {
                        msg_type: MessageType::Assistant,
                        content: assistant_message.text_preview().unwrap_or(final_answer),
                    },
                });
                if self.append_pending_user_steer_message(session_id)? {
                    continue;
                }
                return Ok(());
            }

            if !had_tool_activity {
                bail!("main agent returned neither a final answer nor tool activity");
            }
            self.append_pending_user_steer_message(session_id)?;
        }
    }

    fn projected_main_model_messages(&self, session_id: &str) -> Result<crate::model::Messages> {
        let visible = self.session_manager.get_all_messages(session_id)?;
        let runtime = self.client.runtime();
        let context_window = resolve_runtime_context_window(&runtime);
        let projected = project_main_history(
            &visible,
            ContextProjectionPolicy::guarded_mid(Some(context_window)),
        );
        SessionManager::project_session_messages_to_model(&projected)
    }

    fn run_main_call_capability(
        &self,
        parent_session_id: &str,
        input: CallCapabilityInput,
        approval_provider: Arc<dyn ApprovalProvider>,
        registry: &RuntimeToolRegistry,
        shared_context: Arc<Mutex<Vec<String>>>,
        instruction_seen: Arc<Mutex<HashSet<(String, String)>>>,
    ) -> Result<String> {
        let capability = input.capability.trim();
        if capability == "request_memory_review" {
            let executor = RuntimeMainAgentBuiltinExecutor {
                runtime: self,
                parent_session_id,
            };
            return builtins::main_agent::execute_main_agent_builtin(input, &executor);
        }
        if builtins::memory::is_memory_capability(capability) {
            return Ok(builtins::unavailable_capability_result(
                "MainAgent",
                capability,
                &["request_memory_review"],
            ));
        }

        let shared = call_capability_shared_context(&input);
        self.event_bus.publish(AgentEvent::Message {
            session_id: parent_session_id.to_string(),
            message: UiMessage {
                msg_type: MessageType::ToolCall,
                content: shared.clone(),
            },
        });
        shared_context
            .lock()
            .expect("main shared context mutex poisoned")
            .push(shared);

        let candidate_dirs = path_candidate_dirs_for_runtime_tool(capability, &input.args);
        let sandbox = crate::sandbox::resolve_sandbox()?;
        let context = RuntimeToolExecutionContext {
            builtin: BuiltinExecutionContext {
                session_manager: &self.session_manager,
                session_id: parent_session_id,
                approval_provider: approval_provider.clone(),
                vision_client: &self.client,
            },
            permission: PermissionContext {
                approval_policy: self.approval_policy.clone(),
                approval_provider,
                sandbox,
            },
        };
        let capability_name = capability.to_string();
        let result = registry.execute(input, &context)?;
        if capability_name != "request_filesystem_access" {
            crate::sandbox::config::consume_once_sandbox_access_grants();
        }
        let visible_messages = self
            .session_manager
            .get_all_messages(parent_session_id)
            .unwrap_or_default();
        let mut seen = instruction_seen
            .lock()
            .expect("instruction seen mutex poisoned");
        compose_tool_result(&result, &candidate_dirs, &visible_messages, &mut seen)
    }

    fn request_memory_review(&self, session_id: &str, purpose: String) -> Result<String> {
        let purpose = purpose.trim();
        if purpose.is_empty() {
            bail!("request_memory_review requires non-empty call_capability.purpose");
        }
        {
            let mut state = self
                .memory_reviews
                .lock()
                .expect("memory review state mutex poisoned");
            state
                .pending_purposes
                .entry(session_id.to_string())
                .or_default()
                .push(purpose.to_string());
        }
        Ok(json!({
            "status": "accepted",
            "message": "Memory review request accepted. It will run after the current MainAgent turn. No memory change has happened yet."
        })
        .to_string())
    }

    fn maybe_start_memory_review_after_main_turn(&self, session_id: String) {
        {
            let mut state = self
                .memory_reviews
                .lock()
                .expect("memory review state mutex poisoned");
            mark_memory_review_due_after_main_turn(&mut state, &session_id);
        }
        self.maybe_start_pending_memory_review(session_id);
    }

    fn maybe_start_pending_memory_review(&self, session_id: String) {
        let dispatch = {
            let mut state = self
                .memory_reviews
                .lock()
                .expect("memory review state mutex poisoned");
            if state.running_sessions.contains(&session_id) {
                return;
            }
            let Some(purposes) = state
                .pending_purposes
                .remove(&session_id)
                .filter(|purposes| !purposes.is_empty())
            else {
                return;
            };
            state.running_sessions.insert(session_id.clone());
            MemoryReviewDispatch { purposes }
        };

        let runtime = self.clone();
        thread::spawn(move || {
            let result = runtime.run_memory_review(&session_id, dispatch.purposes);
            let changed = match result {
                Ok(changed) => changed,
                Err(err) => {
                    runtime.event_bus.publish(AgentEvent::Error {
                        session_id: session_id.clone(),
                        message: format!("Memory review failed: {err:#}"),
                    });
                    false
                }
            };
            {
                let mut state = runtime
                    .memory_reviews
                    .lock()
                    .expect("memory review state mutex poisoned");
                record_memory_review_finished(&mut state, &session_id, changed);
            }
            runtime.maybe_start_pending_memory_review(session_id);
        });
    }

    fn run_memory_review(&self, parent_session_id: &str, purposes: Vec<String>) -> Result<bool> {
        let parent_runtime = self.session_manager.get_runtime_config(parent_session_id)?;
        let base_messages = self.session_manager.to_openai_messages(parent_session_id)?;
        let purpose = purposes.join("\n");
        let agent_id = self
            .session_manager
            .fork_agent_session_from_model_messages(
                parent_session_id,
                Some("Memory Review"),
                &purpose,
                parent_runtime,
                &base_messages,
                "memory_review",
            )?;
        let operation_message = build_memory_agent_user_message(
            &purposes,
            &self.memory_store.render_active_catalog()?,
            &self.memory_store.workspace_root().display().to_string(),
        );
        self.session_manager.append_text_message(
            &agent_id,
            SessionRole::User,
            operation_message,
        )?;

        let changed = Arc::new(AtomicBool::new(false));
        let source_session_id = parent_session_id.to_string();
        let memory_store = self.memory_store.clone();
        let memory_changed = changed.clone();
        let callbacks = AgentToolCallbacks {
            call_capability: Arc::new(move |input| {
                let context = memory_builtin::MemoryBuiltinContext {
                    memory_store: &memory_store,
                    changed: memory_changed.clone(),
                    source_session_id: &source_session_id,
                };
                memory_builtin::execute_memory_builtin(input, &context)
            }),
        };

        let model_response = self.client.generate_streaming_with_tools(
            self.session_manager.to_openai_messages(&agent_id)?,
            agent_tools(callbacks),
            |_| {},
        )?;
        let outcome = model_response_to_outcome(model_response)?;
        for session_message in outcome.session_messages {
            self.session_manager
                .append_message(&agent_id, &session_message)?;
        }
        if let Some(final_answer) = outcome.final_answer {
            self.session_manager.append_text_message(
                &agent_id,
                SessionRole::Assistant,
                final_answer,
            )?;
        }
        let status = if changed.load(Ordering::SeqCst) {
            "completed"
        } else {
            "no_change"
        };
        self.session_manager
            .update_agent_status(&agent_id, status)?;
        Ok(changed.load(Ordering::SeqCst))
    }

    fn maybe_generate_title(&self, session_id: &str, user_text: String) {
        if self.session_manager.session_has_custom_title(session_id) {
            return;
        }
        {
            let mut guard = self
                .title_requested_sessions
                .lock()
                .expect("title requested mutex poisoned");
            if !guard.insert(session_id.to_string()) {
                return;
            }
        }

        let client = self.client.clone();
        let session_manager = self.session_manager.clone();
        let event_bus = self.event_bus.clone();
        let session_id = session_id.to_string();

        thread::spawn(move || {
            if let Ok(title) = generate_session_title(&client, &user_text) {
                if session_manager.update_title(&session_id, &title).is_ok() {
                    event_bus.publish(AgentEvent::SessionTitleUpdated {
                        session_id: session_id.clone(),
                        title,
                    });
                }
            }
        });
    }

    fn append_pending_user_steer_message(&self, session_id: &str) -> Result<bool> {
        let pending = {
            let mut state = self.state.lock().expect("runtime state mutex poisoned");
            let Some(session_state) = state.sessions.get_mut(session_id) else {
                return Ok(false);
            };
            if session_state.pending_user_steers.is_empty() {
                return Ok(false);
            }
            std::mem::take(&mut session_state.pending_user_steers)
        };
        let raw_steer_text = pending
            .iter()
            .map(|message| message.text.trim())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let steer_text = build_user_message_steer(&pending);
        let message =
            self.session_manager
                .append_text_message(session_id, SessionRole::User, steer_text)?;
        self.session_manager
            .append_user_turn_marker(session_id, message.id(), raw_steer_text)?;
        Ok(true)
    }

    fn emit_approval_events(&self, session_id: &str, message: &SessionMessage) {
        match message {
            SessionMessage::ToolApprovalRequest {
                command, rule_hits, ..
            } => {
                self.event_bus.publish(AgentEvent::ApprovalRequested {
                    session_id: session_id.to_string(),
                    command: command.clone(),
                    rule_hits: rule_hits.clone(),
                });
            }
            SessionMessage::ToolApprovalDecision {
                command,
                decision,
                approved,
                ..
            } => {
                self.event_bus.publish(AgentEvent::ApprovalDecided {
                    session_id: session_id.to_string(),
                    command: command.clone(),
                    decision: decision.clone(),
                    approved: *approved,
                });
            }
            _ => {}
        }
    }
}

fn enqueue_user_message(
    session_state: &mut SessionRuntimeState,
    user_message: SubmittedUserMessage,
    approval_provider: Arc<dyn ApprovalProvider>,
) -> bool {
    if session_state.main_turn_running {
        if matches!(user_message.source, UserMessageSource::Cron(_)) {
            session_state
                .pending_user_inputs
                .push_back(PendingUserInput {
                    message: user_message,
                    approval_provider,
                });
        } else {
            session_state.pending_user_steers.push(user_message);
        }
        false
    } else {
        session_state
            .pending_user_inputs
            .push_back(PendingUserInput {
                message: user_message,
                approval_provider,
            });
        true
    }
}

struct RuntimeMainAgentBuiltinExecutor<'a> {
    runtime: &'a AgentRuntime,
    parent_session_id: &'a str,
}

impl MainAgentBuiltinExecutor for RuntimeMainAgentBuiltinExecutor<'_> {
    fn request_memory_review(&self, purpose: String) -> Result<String> {
        self.runtime
            .request_memory_review(self.parent_session_id, purpose)
    }
}

fn is_tool_activity_message(message: &SessionMessage) -> bool {
    matches!(
        message,
        SessionMessage::ToolCall { .. }
            | SessionMessage::ToolResult { .. }
            | SessionMessage::ToolApprovalRequest { .. }
            | SessionMessage::ToolApprovalDecision { .. }
    )
}

fn mark_memory_review_due_after_main_turn(state: &mut MemoryReviewState, session_id: &str) {
    if state
        .pending_purposes
        .get(session_id)
        .is_some_and(|purposes| !purposes.is_empty())
    {
        return;
    }

    let interval = *state
        .intervals
        .entry(session_id.to_string())
        .or_insert(MEMORY_REVIEW_BASE_INTERVAL_TURNS);
    let turns = state
        .turns_since_review
        .entry(session_id.to_string())
        .or_insert(0);
    *turns += 1;
    if *turns >= interval {
        state
            .pending_purposes
            .entry(session_id.to_string())
            .or_default()
            .push(
                "Periodic durable memory review for the latest MainAgent conversation. Decide whether any stable preference, fact, procedure, episode, correction, or forget signal should update memory; no-op when nothing durable changed."
                    .to_string(),
            );
    }
}

fn record_memory_review_finished(state: &mut MemoryReviewState, session_id: &str, changed: bool) {
    state.running_sessions.remove(session_id);
    state.turns_since_review.insert(session_id.to_string(), 0);
    let interval = if changed {
        MEMORY_REVIEW_BASE_INTERVAL_TURNS
    } else {
        state
            .intervals
            .get(session_id)
            .copied()
            .unwrap_or(MEMORY_REVIEW_BASE_INTERVAL_TURNS)
            .saturating_add(2)
            .min(MEMORY_REVIEW_MAX_INTERVAL_TURNS)
    };
    state.intervals.insert(session_id.to_string(), interval);
}

fn build_memory_agent_user_message(
    purposes: &[String],
    active_catalog: &str,
    workspace_root: &str,
) -> String {
    let purpose = purposes
        .iter()
        .map(|value| format!("- {}", value.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    render_prompt_template(
        MEMORY_AGENT_USER_MESSAGE_TEMPLATE,
        &[
            ("purpose", purpose),
            ("active_memory", active_catalog.trim().to_string()),
            ("workspace_root", workspace_root.trim().to_string()),
        ],
    )
}

fn render_prompt_template(template: &str, values: &[(&str, String)]) -> String {
    let mut rendered = template.to_string();
    for (key, value) in values {
        rendered = rendered.replace(&format!("{{{{{key}}}}}"), value);
    }
    rendered.trim_end().to_string()
}

fn current_time_context_block() -> DynamicContextBlock {
    let utc_now = chrono::Utc::now();
    let local_now = chrono::Local::now();
    DynamicContextBlock {
        id: "duckagent://current-time".to_string(),
        label: "CURRENT TIME".to_string(),
        content: format!(
            "utc_now: {}\nlocal_now: {}\nUse local_now when converting relative schedule requests such as `in five minutes` or `tomorrow morning` into absolute RFC3339 timestamps for cron_create.",
            utc_now.to_rfc3339(),
            local_now.to_rfc3339()
        ),
    }
}

fn build_main_user_message(user_text: Option<&str>) -> String {
    user_text
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .unwrap_or_default()
}

fn build_user_message_steer(messages: &[SubmittedUserMessage]) -> String {
    let mut out = String::from("[User Message Steer]\n");
    out.push_str("The user sent the following message(s) while the current Agent Loop was still running. Continue the current Agent Loop and consider this steer together with the immediately preceding assistant progress and/or tool results.\n");
    for (index, message) in messages.iter().enumerate() {
        if messages.len() > 1 {
            out.push_str(&format!("\n--- User Message Steer {} ---\n", index + 1));
        } else {
            out.push('\n');
        }
        match &message.source {
            UserMessageSource::Tui => {
                out.push_str("source: tui\n");
            }
            UserMessageSource::Gateway(metadata) => {
                out.push_str("source: gateway\n");
                out.push_str(&format!("channel: {}\n", metadata.channel));
                out.push_str(&format!("conversation_id: {}\n", metadata.conversation_id));
                if let Some(thread_id) = metadata.thread_id.as_deref() {
                    out.push_str(&format!("thread_id: {thread_id}\n"));
                }
                if let Some(sender_id) = metadata.sender_id.as_deref() {
                    out.push_str(&format!("sender_id: {sender_id}\n"));
                }
                if let Some(message_id) = metadata.message_id.as_deref() {
                    out.push_str(&format!("message_id: {message_id}\n"));
                }
            }
            UserMessageSource::Cron(metadata) => {
                out.push_str("source: cron\n");
                out.push_str(&format!("job_id: {}\n", metadata.job_id));
                out.push_str(&format!("run_id: {}\n", metadata.run_id));
                out.push_str(&format!("scheduled_for: {}\n", metadata.scheduled_for));
            }
        }
        out.push_str("message:\n");
        out.push_str(message.text.trim());
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn render_main_capability_index(registry: &RuntimeToolRegistry) -> String {
    let mut builtins = Vec::new();
    let mut mcp_tools = Vec::new();
    for entry in registry.entries() {
        match &entry.source {
            RuntimeToolSource::Builtin | RuntimeToolSource::LoadMcp => {
                let args = serde_json::to_string(&entry.input_schema)
                    .unwrap_or_else(|_| "{\"type\":\"object\"}".to_string());
                builtins.push(render_prompt_template(
                    MAIN_CAPABILITY_BUILTIN_ITEM_TEMPLATE,
                    &[
                        ("name", entry.exposed_name.clone()),
                        ("args_schema", args),
                        ("description", entry.description.clone()),
                    ],
                ));
            }
            RuntimeToolSource::Mcp { server_name, .. } => {
                mcp_tools.push(render_prompt_template(
                    MAIN_CAPABILITY_MCP_ITEM_TEMPLATE,
                    &[
                        ("name", entry.exposed_name.clone()),
                        ("server_name", server_name.clone()),
                        ("tool_name", entry.original_name.clone()),
                        ("description", entry.description.clone()),
                    ],
                ));
            }
        }
    }
    let mcp = if mcp_tools.is_empty() {
        MAIN_CAPABILITY_MCP_EMPTY_TEMPLATE.trim_end().to_string()
    } else {
        mcp_tools.join("\n")
    };
    render_prompt_template(
        MAIN_CAPABILITY_INDEX_TEMPLATE,
        &[
            (
                "active_sandbox",
                crate::sandbox::config::current_sandbox_summary(),
            ),
            ("built_in_capabilities", builtins.join("\n")),
            ("mcp_tools", mcp),
            ("skills", crate::skills::render_skill_index()),
        ],
    )
}

fn generate_session_title(client: &ModelClient, user_input: &str) -> Result<String> {
    const TITLE_OUTPUT_MAX_CHARS: usize = 80;

    let raw = client.generate_title(SESSION_TITLE_SYSTEM_PROMPT, user_input)?;
    let single_line = raw.lines().next().unwrap_or(raw.as_str()).trim();
    let compact = single_line.trim_matches('"').trim_matches('\'').trim();
    if compact.is_empty() {
        bail!("title generation produced empty normalized title");
    }
    Ok(compact.chars().take(TITLE_OUTPUT_MAX_CHARS).collect())
}

fn estimate_model_messages_tokens(messages: &[crate::model::Message]) -> usize {
    messages
        .iter()
        .map(|message| {
            count_tokens_with_retry(&match message {
                crate::model::Message::System(system) => system.content.clone(),
                crate::model::Message::Developer(text) => text.clone(),
                crate::model::Message::User(user) => user.content.clone(),
                crate::model::Message::Assistant(assistant) => match &assistant.content {
                    crate::model::LanguageModelResponseContentType::Text(text) => text.clone(),
                    crate::model::LanguageModelResponseContentType::Reasoning { content } => {
                        content.clone()
                    }
                    crate::model::LanguageModelResponseContentType::ToolCall(tool_call) => {
                        format!(
                            "{}\n{}\n{}",
                            tool_call.content.as_deref().unwrap_or_default(),
                            tool_call.tool.name,
                            tool_call.input
                        )
                    }
                },
                crate::model::Message::Tool(tool_result) => match &tool_result.output {
                    Ok(Value::String(text)) => text.clone(),
                    Ok(value) => value.to_string(),
                    Err(error) => error.to_string(),
                },
            })
        })
        .sum()
}

fn count_tokens_with_retry(text: &str) -> usize {
    count_tool_tokens_cl100k(text)
        .or_else(|_| count_tool_tokens_cl100k(text))
        .unwrap_or_else(|_| estimate_tokens_rough(text))
}

fn ui_message_for_tool_turn_text(content: &str) -> Option<UiMessage> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(UiMessage {
            msg_type: MessageType::Assistant,
            content: trimmed.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{ApprovalDecision, ApprovalResponse, RuleHit};
    use std::sync::Arc;

    struct StaticApprovalProvider(ApprovalDecision);

    impl ApprovalProvider for StaticApprovalProvider {
        fn request_approval(
            &self,
            _command: &str,
            _rule_hits: &[RuleHit],
            _options: [ApprovalDecision; 4],
        ) -> Option<ApprovalResponse> {
            Some(ApprovalResponse { decision: self.0 })
        }
    }

    #[test]
    fn build_main_user_message_does_not_duplicate_user_message_header() {
        let message = build_main_user_message(Some("Show the current folder path"));
        assert_eq!(message, "Show the current folder path");
    }

    #[test]
    fn build_user_message_steer_labels_tui_messages() {
        let message = build_user_message_steer(&[
            SubmittedUserMessage::tui("first".to_string()),
            SubmittedUserMessage::tui("second".to_string()),
        ]);
        assert!(message.contains("[User Message Steer]"));
        assert!(message.contains("--- User Message Steer 1 ---"));
        assert!(message.contains("source: tui"));
        assert!(message.contains("first"));
        assert!(message.contains("second"));
    }

    #[test]
    fn build_user_message_steer_preserves_gateway_context() {
        let message = build_user_message_steer(&[SubmittedUserMessage::gateway(
            "hello".to_string(),
            GatewayUserMessageMetadata {
                channel: "webhook".to_string(),
                conversation_id: "conv".to_string(),
                thread_id: Some("thread".to_string()),
                sender_id: Some("sender".to_string()),
                message_id: Some("msg".to_string()),
            },
        )]);
        assert!(message.contains("source: gateway"));
        assert!(message.contains("channel: webhook"));
        assert!(message.contains("conversation_id: conv"));
        assert!(message.contains("message_id: msg"));
        assert!(message.contains("hello"));
    }

    #[test]
    fn tool_turn_text_renders_as_assistant_message() {
        let message = ui_message_for_tool_turn_text("  Let me create that file.  ")
            .expect("non-empty content should render");

        assert_eq!(message.msg_type, MessageType::Assistant);
        assert_eq!(message.content, "Let me create that file.");
        assert!(ui_message_for_tool_turn_text(" \n\t ").is_none());
    }

    #[test]
    fn build_user_message_steer_preserves_cron_context() {
        let message = build_user_message_steer(&[SubmittedUserMessage::cron(
            "run scheduled cleanup".to_string(),
            CronUserMessageMetadata {
                job_id: "job_1".to_string(),
                run_id: "run_1".to_string(),
                scheduled_for: "2026-06-01T00:00:00Z".to_string(),
            },
        )]);
        assert!(message.contains("source: cron"));
        assert!(message.contains("job_id: job_1"));
        assert!(message.contains("run_id: run_1"));
        assert!(message.contains("scheduled_for: 2026-06-01T00:00:00Z"));
        assert!(message.contains("run scheduled cleanup"));
    }

    #[test]
    fn current_time_context_block_guides_relative_cron_conversion() {
        let block = current_time_context_block();
        assert_eq!(block.id, "duckagent://current-time");
        assert_eq!(block.label, "CURRENT TIME");
        assert!(block.content.contains("utc_now:"));
        assert!(block.content.contains("local_now:"));
        assert!(
            block
                .content
                .contains("absolute RFC3339 timestamps for cron_create")
        );
    }

    #[test]
    fn cron_input_queued_while_running_keeps_its_approval_provider() {
        let mut state = SessionRuntimeState {
            main_turn_running: true,
            ..Default::default()
        };
        let provider: Arc<dyn ApprovalProvider> =
            Arc::new(StaticApprovalProvider(ApprovalDecision::Always));

        let should_start = enqueue_user_message(
            &mut state,
            SubmittedUserMessage::cron(
                "scheduled".to_string(),
                CronUserMessageMetadata {
                    job_id: "job_1".to_string(),
                    run_id: "run_1".to_string(),
                    scheduled_for: "2026-06-01T00:00:00Z".to_string(),
                },
            ),
            provider,
        );

        assert!(!should_start);
        assert_eq!(state.pending_user_inputs.len(), 1);
        assert!(state.pending_user_steers.is_empty());
        let decision = state.pending_user_inputs[0]
            .approval_provider
            .request_approval("cmd", &[], ApprovalDecision::options())
            .expect("static provider returns a decision")
            .decision;
        assert_eq!(decision, ApprovalDecision::Always);
    }

    #[test]
    fn non_cron_input_while_running_is_still_a_steer() {
        let mut state = SessionRuntimeState {
            main_turn_running: true,
            ..Default::default()
        };
        let provider: Arc<dyn ApprovalProvider> =
            Arc::new(StaticApprovalProvider(ApprovalDecision::Forbidden));

        let should_start = enqueue_user_message(
            &mut state,
            SubmittedUserMessage::tui("please also do this".to_string()),
            provider,
        );

        assert!(!should_start);
        assert!(state.pending_user_inputs.is_empty());
        assert_eq!(state.pending_user_steers.len(), 1);
    }

    #[test]
    fn capability_index_includes_agent_contract_and_skills_section() {
        let registry = RuntimeToolRegistry::new();
        let index = render_main_capability_index(&registry);
        assert!(index.contains("Native tool for all Agent modes: `call_capability`"));
        assert!(index.contains("Current Agent mode: MainAgent"));
        assert!(index.contains("MainAgent allowed capabilities"));
        assert!(index.contains("Runtime capability names listed below"));
        assert!(index.contains("Do not invent delegation capabilities"));
        assert!(index.contains("It only schedules review after the current MainAgent turn"));
        assert!(index.contains("If Active sandbox lists a protected path"));
        assert!(index.contains("protected user path: `~/.duckagent`"));
        assert!(index.contains("recovery handle"));
        assert!(index.contains("## Active sandbox"));
        assert!(index.contains("`read_file`"));
        assert!(index.contains("## Skills"));
    }

    #[test]
    fn memory_review_scheduler_queues_periodic_after_interval() {
        let mut state = MemoryReviewState::default();
        let session_id = "session";

        for _ in 0..3 {
            mark_memory_review_due_after_main_turn(&mut state, session_id);
            assert!(!state.pending_purposes.contains_key(session_id));
        }

        mark_memory_review_due_after_main_turn(&mut state, session_id);
        assert_eq!(state.pending_purposes[session_id].len(), 1);
    }

    #[test]
    fn memory_review_scheduler_respects_explicit_pending_request() {
        let mut state = MemoryReviewState::default();
        let session_id = "session";
        state
            .pending_purposes
            .insert(session_id.to_string(), vec!["explicit".to_string()]);

        mark_memory_review_due_after_main_turn(&mut state, session_id);

        assert_eq!(state.pending_purposes[session_id], vec!["explicit"]);
        assert_eq!(state.turns_since_review.get(session_id), None);
    }

    #[test]
    fn memory_review_scheduler_expands_no_change_interval_and_resets_on_change() {
        let mut state = MemoryReviewState::default();
        let session_id = "session";

        record_memory_review_finished(&mut state, session_id, false);
        assert_eq!(
            state.intervals[session_id],
            MEMORY_REVIEW_BASE_INTERVAL_TURNS + 2
        );

        record_memory_review_finished(&mut state, session_id, false);
        assert_eq!(
            state.intervals[session_id],
            MEMORY_REVIEW_MAX_INTERVAL_TURNS
        );

        record_memory_review_finished(&mut state, session_id, true);
        assert_eq!(
            state.intervals[session_id],
            MEMORY_REVIEW_BASE_INTERVAL_TURNS
        );
    }
}
