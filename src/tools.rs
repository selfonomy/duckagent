use crate::client::ModelResponse;
use crate::model::{LanguageModelResponseContentType, Message, ModelToolError, Tool, ToolExecute};
use crate::session::{GoalStatus, SessionGoal, SessionManager, SessionMessage};
use crate::utils::truncate_head_middle_tail_by_tokens;
use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

const UI_TOOL_OUTPUT_MAX_TOKENS: usize = 1600;
const TOOL_INPUT_MAX_BYTES: usize = 32 * 1024;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MessageType {
    User,
    Assistant,
    ToolCall,
    ToolResult,
}

#[derive(Clone, Debug)]
pub struct UiMessage {
    pub msg_type: MessageType,
    pub content: String,
}

#[derive(Debug)]
pub struct AgentToolOutcome {
    pub ui_messages: Vec<UiMessage>,
    pub session_messages: Vec<SessionMessage>,
    pub final_answer: Option<String>,
}

#[derive(Clone)]
pub struct AgentToolCallbacks {
    pub call_capability: Arc<dyn Fn(CallCapabilityInput) -> Result<String> + Send + Sync>,
    pub goal: Option<GoalToolCallbacks>,
}

#[derive(Clone)]
pub struct GoalToolCallbacks {
    pub get_goal: Arc<dyn Fn() -> Result<Option<SessionGoal>> + Send + Sync>,
    pub create_goal: Arc<dyn Fn(CreateGoalInput) -> Result<SessionGoal> + Send + Sync>,
    pub update_goal: Arc<dyn Fn(UpdateGoalInput) -> Result<SessionGoal> + Send + Sync>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct CallCapabilityInput {
    /// Capability name. The current Agent mode decides which capabilities are allowed.
    pub capability: String,
    /// Capability-specific arguments.
    #[serde(default)]
    pub args: Value,
    /// Why this capability is needed for the current task.
    pub purpose: String,
}

pub type RuntimeToolInput = CallCapabilityInput;

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct CreateGoalInput {
    /// Required. The concrete objective to start pursuing.
    pub objective: String,
    /// Positive token budget for the new goal. Omit unless explicitly requested.
    #[serde(default)]
    pub token_budget: Option<i64>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct UpdateGoalInput {
    /// Required. Only `complete` and `blocked` are accepted.
    pub status: UpdateGoalStatus,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateGoalStatus {
    Complete,
    Blocked,
}

impl UpdateGoalStatus {
    pub(crate) fn into_goal_status(self) -> GoalStatus {
        match self {
            UpdateGoalStatus::Complete => GoalStatus::Complete,
            UpdateGoalStatus::Blocked => GoalStatus::Blocked,
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
struct GetGoalInput {}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct GoalToolResponse {
    goal: Option<SessionGoal>,
    remaining_tokens: Option<i64>,
    completion_budget_report: Option<String>,
}

pub fn agent_tools(callbacks: AgentToolCallbacks) -> Vec<Tool> {
    let mut tools = vec![create_call_capability_tool(callbacks.call_capability)];
    if let Some(goal_callbacks) = callbacks.goal {
        tools.push(create_get_goal_tool(goal_callbacks.get_goal.clone()));
        tools.push(create_create_goal_tool(goal_callbacks.create_goal.clone()));
        tools.push(create_update_goal_tool(goal_callbacks.update_goal.clone()));
    }
    tools
}

pub fn call_capability_action_preview(input: &CallCapabilityInput) -> String {
    let capability = input.capability.trim();
    if capability.is_empty() {
        "call_capability: (missing capability)".to_string()
    } else {
        capability.to_string()
    }
}

pub fn call_capability_shared_context(input: &CallCapabilityInput) -> String {
    let purpose = input.purpose.trim();
    let action = call_capability_action_preview(input);
    if purpose.is_empty() {
        format!("Action: {action}")
    } else {
        format!("{purpose}\nAction: {action}")
    }
}

pub fn model_response_to_outcome(response: ModelResponse) -> Result<AgentToolOutcome> {
    model_response_to_outcome_for_scope(response, crate::session::SessionScope::main())
}

pub fn model_response_to_outcome_for_scope(
    response: ModelResponse,
    scope: crate::session::SessionScope,
) -> Result<AgentToolOutcome> {
    let mut ui_messages = Vec::new();
    let mut session_messages = Vec::new();

    for step in response.steps {
        for msg in step {
            match msg {
                Message::Assistant(assistant_msg) => match &assistant_msg.content {
                    LanguageModelResponseContentType::ToolCall(tool_call) => {
                        if should_show_tool_call_preview(&tool_call.tool.name) {
                            ui_messages.push(UiMessage {
                                msg_type: MessageType::ToolCall,
                                content: format_tool_call_preview(
                                    &tool_call.tool.name,
                                    &tool_call.input,
                                ),
                            });
                        }
                        session_messages.push(SessionManager::new_tool_use_message_with_content(
                            scope.clone(),
                            tool_call.tool.id.clone(),
                            tool_call.content.clone(),
                            tool_call.tool.name.clone(),
                            sanitize_tool_input(&tool_call.input),
                        ));
                    }
                    LanguageModelResponseContentType::Reasoning { content, .. } => {
                        session_messages.push(SessionManager::new_reasoning_message_with_scope(
                            scope.clone(),
                            content.clone(),
                        ));
                    }
                    _ => {}
                },
                Message::Tool(tool_result) => {
                    let (output, approval_messages) = tool_result_to_text_and_approval_messages(
                        &tool_result.output,
                        &tool_result.tool.id,
                        scope.clone(),
                    );
                    if should_show_tool_result_output(&tool_result.tool.name, &tool_result.output) {
                        ui_messages.push(UiMessage {
                            msg_type: MessageType::ToolResult,
                            content: tool_result_ui_preview(&output),
                        });
                    }
                    session_messages.push(SessionManager::new_tool_result_message_with_scope(
                        scope.clone(),
                        tool_result.tool.id.clone(),
                        output,
                    ));
                    session_messages.extend(approval_messages);
                }
                _ => {}
            }
        }
    }

    let final_answer = response
        .final_text
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    Ok(AgentToolOutcome {
        ui_messages,
        session_messages,
        final_answer,
    })
}

fn create_call_capability_tool(
    call_capability: Arc<dyn Fn(CallCapabilityInput) -> Result<String> + Send + Sync>,
) -> Tool {
    Tool::builder()
        .name("call_capability")
        .description("Call one capability allowed by the current Agent mode. The latest mode instructions define the available capability names and their args. This tool returns an unavailable-capability result when the requested capability is not allowed in the current mode.")
        .input_schema(schemars::schema_for!(CallCapabilityInput))
        .execute(ToolExecute::new(Box::new(move |params| {
            let input: CallCapabilityInput = serde_json::from_value(params)
                .map_err(|e| format!("Failed to parse call_capability input: {e}"))?;
            call_capability(input).map_err(|e| format!("call_capability failed: {e}"))
        })))
        .build()
        .expect("Failed to build call_capability tool")
}

fn create_get_goal_tool(
    get_goal: Arc<dyn Fn() -> Result<Option<SessionGoal>> + Send + Sync>,
) -> Tool {
    Tool::builder()
        .name("get_goal")
        .description("Get the current goal for this thread, including status, budgets, token and elapsed-time usage, and remaining token budget.")
        .input_schema(schemars::schema_for!(GetGoalInput))
        .execute(ToolExecute::new(Box::new(move |_| {
            let goal = get_goal().map_err(|e| format!("failed to read goal: {e}"))?;
            serde_json::to_string(&GoalToolResponse::new(goal, false))
                .map_err(|e| format!("failed to serialize goal response: {e}"))
        })))
        .build()
        .expect("Failed to build get_goal tool")
}

fn create_create_goal_tool(
    create_goal: Arc<dyn Fn(CreateGoalInput) -> Result<SessionGoal> + Send + Sync>,
) -> Tool {
    Tool::builder()
        .name("create_goal")
        .description("Create a goal only when explicitly requested by the user or system/developer instructions; do not infer goals from ordinary tasks. Set token_budget only when an explicit token budget is requested. Fails if an unfinished goal exists; use update_goal only for status.")
        .input_schema(schemars::schema_for!(CreateGoalInput))
        .execute(ToolExecute::new(Box::new(move |params| {
            let input: CreateGoalInput = serde_json::from_value(params)
                .map_err(|e| format!("Failed to parse create_goal input: {e}"))?;
            let goal = create_goal(input).map_err(|e| format!("create_goal failed: {e}"))?;
            serde_json::to_string(&GoalToolResponse::new(Some(goal), false))
                .map_err(|e| format!("failed to serialize goal response: {e}"))
        })))
        .build()
        .expect("Failed to build create_goal tool")
}

fn create_update_goal_tool(
    update_goal: Arc<dyn Fn(UpdateGoalInput) -> Result<SessionGoal> + Send + Sync>,
) -> Tool {
    Tool::builder()
        .name("update_goal")
        .description("Update the existing goal. Use this tool only to mark the goal achieved or genuinely blocked. Set status to `complete` only when the objective has actually been achieved and no required work remains. Set status to `blocked` only when the same blocking condition has repeated for at least three consecutive goal turns, counting the original/user-triggered turn and any automatic continuations, and the agent cannot make meaningful progress without user input or an external-state change. Do not use `blocked` merely because the work is hard, slow, uncertain, incomplete, or would benefit from clarification. Do not mark a goal complete merely because its budget is nearly exhausted or because you are stopping work. You cannot use this tool to pause, resume, budget-limit, or usage-limit a goal; those status changes are controlled by the user or system.")
        .input_schema(schemars::schema_for!(UpdateGoalInput))
        .execute(ToolExecute::new(Box::new(move |params| {
            let input: UpdateGoalInput = serde_json::from_value(params)
                .map_err(|e| format!("Failed to parse update_goal input: {e}"))?;
            let include_completion_report = input.status == UpdateGoalStatus::Complete;
            let goal = update_goal(input).map_err(|e| format!("update_goal failed: {e}"))?;
            serde_json::to_string(&GoalToolResponse::new(
                Some(goal),
                include_completion_report,
            ))
            .map_err(|e| format!("failed to serialize goal response: {e}"))
        })))
        .build()
        .expect("Failed to build update_goal tool")
}

impl GoalToolResponse {
    fn new(goal: Option<SessionGoal>, include_completion_report: bool) -> Self {
        let remaining_tokens = goal.as_ref().and_then(|goal| {
            goal.token_budget
                .map(|budget| (budget - goal.tokens_used).max(0))
        });
        let completion_budget_report = goal
            .as_ref()
            .filter(|goal| include_completion_report && goal.status == GoalStatus::Complete)
            .and_then(|goal| {
                (goal.token_budget.is_some() || goal.time_used_seconds > 0).then(|| {
                    "Goal achieved. Report final usage from this tool result's structured goal fields. If `goal.tokenBudget` is present, include token usage from `goal.tokensUsed` and `goal.tokenBudget`. If `goal.timeUsedSeconds` is greater than 0, summarize elapsed time concisely."
                        .to_string()
                })
            });
        Self {
            goal,
            remaining_tokens,
            completion_budget_report,
        }
    }
}

fn format_tool_call_preview(tool_name: &str, input: &Value) -> String {
    match tool_name {
        "call_capability" => input
            .get("capability")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|capability| format!("call_capability: {capability}"))
            .unwrap_or_else(|| "call_capability".to_string()),
        _ => tool_name.to_string(),
    }
}

fn should_show_tool_result(tool_name: &str) -> bool {
    !is_native_wrapper_tool(tool_name)
}

fn should_show_tool_result_output(
    tool_name: &str,
    output: &std::result::Result<Value, ModelToolError>,
) -> bool {
    if is_unknown_native_tool_error(output) {
        return false;
    }
    should_show_tool_result(tool_name)
}

fn should_show_tool_call_preview(tool_name: &str) -> bool {
    !is_native_wrapper_tool(tool_name)
}

fn is_native_wrapper_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "call_capability" | "get_goal" | "create_goal" | "update_goal"
    )
}

fn is_unknown_native_tool_error(output: &std::result::Result<Value, ModelToolError>) -> bool {
    matches!(output, Err(error) if error.0.starts_with("tool not found:"))
}

fn tool_result_to_text_and_approval_messages(
    output: &std::result::Result<Value, ModelToolError>,
    _call_id: &str,
    _scope: crate::session::SessionScope,
) -> (String, Vec<SessionMessage>) {
    match output {
        Ok(Value::String(text)) => (text.clone(), Vec::new()),
        Ok(value) => {
            let text = value.to_string();
            (text, Vec::new())
        }
        Err(err) => (format!("Error: {err}"), Vec::new()),
    }
}

fn tool_result_ui_preview(output: &str) -> String {
    truncate_head_middle_tail_by_tokens(output, UI_TOOL_OUTPUT_MAX_TOKENS)
        .unwrap_or_else(|_| output.to_string())
}

pub fn sanitize_tool_input(input: &Value) -> Value {
    let serialized = input.to_string();
    if serialized.len() <= TOOL_INPUT_MAX_BYTES {
        return input.clone();
    }
    let preview = take_char_prefix(&serialized, 2048);
    json!({
        "_truncated": true,
        "original_bytes": serialized.len(),
        "preview": preview,
    })
}

fn take_char_prefix(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssistantMessage, ToolCallInfo, ToolResultInfo};
    use serde_json::json;

    #[test]
    fn call_capability_tool_preview_does_not_depend_on_specific_capability_args() {
        let runtime_preview = format_tool_call_preview(
            "call_capability",
            &json!({
                "capability": "process_start",
                "args": { "command": "cat AGENTS.md" },
            }),
        );

        assert_eq!(runtime_preview, "call_capability: process_start");
        assert!(!should_show_tool_result("call_capability"));
    }

    #[test]
    fn agent_tools_have_fixed_cache_friendly_schema() {
        let callbacks = AgentToolCallbacks {
            call_capability: Arc::new(|_| Ok("ok".to_string())),
            goal: None,
        };
        let tools = agent_tools(callbacks);
        let names = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["call_capability"]);
        let schema = tools[0].input_schema.to_string();
        assert!(schema.contains("capability"));
        assert!(schema.contains("args"));
        assert!(schema.contains("purpose"));
        assert!(tools[0].description.contains("current Agent mode"));
    }

    #[test]
    fn goal_tools_are_exposed_with_restricted_update_schema() {
        let callbacks = AgentToolCallbacks {
            call_capability: Arc::new(|_| Ok("ok".to_string())),
            goal: Some(GoalToolCallbacks {
                get_goal: Arc::new(|| Ok(None)),
                create_goal: Arc::new(|input| {
                    Ok(test_goal(
                        input.objective,
                        GoalStatus::Active,
                        input.token_budget,
                    ))
                }),
                update_goal: Arc::new(|input| {
                    Ok(test_goal(
                        "done".to_string(),
                        input.status.into_goal_status(),
                        None,
                    ))
                }),
            }),
        };
        let tools = agent_tools(callbacks);
        let names = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["call_capability", "get_goal", "create_goal", "update_goal"]
        );

        let update_tool = tools
            .iter()
            .find(|tool| tool.name == "update_goal")
            .expect("update_goal tool");
        let schema = update_tool.input_schema.to_string();
        assert!(schema.contains("complete"));
        assert!(schema.contains("blocked"));
        assert!(!schema.contains("paused"));

        let output = (update_tool.execute)(json!({ "status": "complete" }))
            .expect("complete update should succeed");
        assert!(output.contains("\"status\":\"complete\""));
        assert!(
            (update_tool.execute)(json!({ "status": "paused" })).is_err(),
            "paused is user-controlled and must not be accepted by update_goal"
        );
    }

    #[test]
    fn call_capability_shared_context_uses_purpose_and_safe_preview() {
        let input = RuntimeToolInput {
            capability: "write_file".to_string(),
            args: json!({
                "path": "src/tools.rs",
                "content": "secret full file content"
            }),
            purpose: "Update tool definitions".to_string(),
        };

        let shared = call_capability_shared_context(&input);
        assert!(shared.contains("Update tool definitions"));
        assert!(shared.contains("Action: write_file"));
        assert!(!shared.contains("secret full file content"));
        assert!(!shared.contains("src/tools.rs"));
    }

    #[test]
    fn vision_action_preview_uses_capability_name() {
        let input = RuntimeToolInput {
            capability: "vision_analyze".to_string(),
            args: json!({
                "path": "src/default/avatar.gif",
                "question": "describe it"
            }),
            purpose: "Analyze image".to_string(),
        };

        let shared = call_capability_shared_context(&input);
        assert!(shared.contains("Action: vision_analyze"));
        assert!(!shared.contains("Action: call_capability: vision_analyze"));
        assert!(!shared.contains("src/default/avatar.gif"));
    }

    #[test]
    fn action_preview_uses_only_dynamic_capability_name() {
        let input = RuntimeToolInput {
            capability: "process_start".to_string(),
            args: json!({
                "command": format!("printf 'hello'\n{}", "x".repeat(250)),
            }),
            purpose: String::new(),
        };

        let preview = call_capability_action_preview(&input);
        assert_eq!(preview, "process_start");
    }

    #[test]
    fn model_response_to_outcome_preserves_tool_call_content() {
        let mut tool_call = ToolCallInfo::new("call_capability".to_string());
        tool_call.id("call_1".to_string());
        tool_call.input(json!({
            "capability": "read_file",
            "args": {"path": "index.js"},
            "purpose": "inspect before answering"
        }));
        tool_call.content("Let me inspect the file first.".to_string());

        let mut tool_result = ToolResultInfo::new("call_capability".to_string());
        tool_result.id("call_1".to_string());
        tool_result.output(Value::String("status: completed\n".to_string()));

        let response = ModelResponse {
            steps: vec![vec![
                Message::Assistant(AssistantMessage::new(
                    LanguageModelResponseContentType::Reasoning {
                        content: "Need to inspect before running.".to_string(),
                    },
                    None,
                )),
                Message::Assistant(AssistantMessage::new(
                    LanguageModelResponseContentType::ToolCall(tool_call),
                    None,
                )),
                Message::Tool(tool_result),
            ]],
            tool_turn_texts: vec!["Let me inspect the file first.".to_string()],
            final_text: Some("done".to_string()),
        };

        let outcome = model_response_to_outcome(response).expect("outcome should build");
        let tool_call_message = outcome
            .session_messages
            .iter()
            .find_map(|message| match message {
                SessionMessage::ToolCall { content, .. } => content.as_deref(),
                _ => None,
            });

        assert_eq!(tool_call_message, Some("Let me inspect the file first."));
        assert!(outcome.ui_messages.is_empty());
    }

    #[test]
    fn model_response_to_outcome_can_store_auxiliary_scope() {
        let mut tool_call = ToolCallInfo::new("call_capability".to_string());
        tool_call.id("call_auxiliary".to_string());
        tool_call.input(json!({
            "capability": "read_file",
            "args": { "path": "src/main.rs" },
            "purpose": "inspect"
        }));
        let mut tool_result = ToolResultInfo::new("call_capability".to_string());
        tool_result.id("call_auxiliary".to_string());
        tool_result.output(Value::String("ok".to_string()));
        let scope = crate::session::SessionScope::agent("memory_review_test");

        let outcome = model_response_to_outcome_for_scope(
            ModelResponse {
                steps: vec![vec![
                    Message::Assistant(AssistantMessage::new(
                        LanguageModelResponseContentType::ToolCall(tool_call),
                        None,
                    )),
                    Message::Tool(tool_result),
                ]],
                tool_turn_texts: Vec::new(),
                final_text: None,
            },
            scope.clone(),
        )
        .expect("outcome should build");

        assert!(
            outcome
                .session_messages
                .iter()
                .all(|message| message.scope() == &scope)
        );
    }

    #[test]
    fn model_response_to_outcome_stores_full_tool_result_but_previews_ui() {
        let long_output = (0..5_000)
            .map(|idx| format!("{idx:04}: {}\n", "tool output ".repeat(8)))
            .collect::<String>();
        let mut tool_result = ToolResultInfo::new("read_file".to_string());
        tool_result.id("call_read".to_string());
        tool_result.output(Value::String(long_output.clone()));

        let outcome = model_response_to_outcome(ModelResponse {
            steps: vec![vec![Message::Tool(tool_result)]],
            tool_turn_texts: Vec::new(),
            final_text: None,
        })
        .expect("outcome should build");

        let stored_output = outcome
            .session_messages
            .iter()
            .find_map(|message| match message {
                SessionMessage::ToolResult { output, .. } => Some(output),
                _ => None,
            });

        assert_eq!(stored_output, Some(&long_output));
        assert_eq!(outcome.ui_messages.len(), 1);
        assert!(outcome.ui_messages[0].content.len() < long_output.len());
    }

    #[test]
    fn model_response_hides_unknown_native_tool_error_from_ui() {
        let mut tool_call = ToolCallInfo::new("process_start".to_string());
        tool_call.id("call_bad".to_string());
        tool_call.input(json!({"command": "cat ~/.duckagent/config.json"}));
        tool_call.content("Trying a command directly.".to_string());

        let mut tool_result = ToolResultInfo::new("process_start".to_string());
        tool_result.id("call_bad".to_string());
        tool_result.output = Err(ModelToolError(
            "tool not found: process_start. Available native tools: call_capability".to_string(),
        ));

        let outcome = model_response_to_outcome(ModelResponse {
            steps: vec![vec![
                Message::Assistant(AssistantMessage::new(
                    LanguageModelResponseContentType::ToolCall(tool_call),
                    None,
                )),
                Message::Tool(tool_result),
            ]],
            tool_turn_texts: Vec::new(),
            final_text: Some("blocked".to_string()),
        })
        .expect("outcome should build");

        assert!(
            outcome
                .ui_messages
                .iter()
                .all(|message| { !message.content.contains("tool not found: process_start") })
        );
        assert!(
            outcome
                .session_messages
                .iter()
                .any(|message| match message {
                    SessionMessage::ToolResult { output, .. } => {
                        output.contains("tool not found: process_start")
                    }
                    _ => false,
                })
        );
    }

    #[test]
    fn tool_input_truncation_is_deterministic() {
        let input = json!({ "content": "x".repeat(40_000) });
        let first = sanitize_tool_input(&input);
        let second = sanitize_tool_input(&input);
        assert_eq!(first, second);
        assert_eq!(first["_truncated"], true);
        assert_eq!(first["original_bytes"], input.to_string().len());
    }

    fn test_goal(objective: String, status: GoalStatus, token_budget: Option<i64>) -> SessionGoal {
        SessionGoal {
            session_id: "session".to_string(),
            objective,
            status,
            token_budget,
            tokens_used: 0,
            time_used_seconds: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }
}
