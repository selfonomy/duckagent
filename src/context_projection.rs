use crate::session::{ContentItem, SessionMessage, SessionRole};
use crate::utils::{count_tool_tokens_cl100k, estimate_tokens_rough};
use serde_json::Value;
use std::collections::HashMap;

const FALLBACK_CONTEXT_WINDOW: usize = 32_000;
const SMALL_CONTEXT_ACTIVE_PRESSURE: f32 = 0.80;
const LARGE_CONTEXT_ACTIVE_PRESSURE: f32 = 0.85;
const LARGE_CONTEXT_THRESHOLD: usize = 200_000;
const COMPLETED_EXACT_EVIDENCE_BUDGET_TOKENS: usize = 2_000;
const ACTIVE_EXACT_EVIDENCE_BUDGET_TOKENS: usize = 18_000;
const TOOL_SUMMARY_PREVIEW_TOKENS: usize = 220;
const COMPLETED_LOOP_SUMMARY_TEMPLATE: &str =
    include_str!("prompts/context-completed-loop-summary.md");
const TOOL_RESULT_SUMMARY_TEMPLATE: &str = include_str!("prompts/context-tool-result-summary.md");

#[derive(Debug, Clone, Copy)]
pub struct ContextProjectionPolicy {
    pub context_window_tokens: usize,
    pub active_loop_pressure_ratio: f32,
    pub completed_exact_evidence_budget_tokens: usize,
    pub active_exact_evidence_budget_tokens: usize,
}

impl ContextProjectionPolicy {
    pub fn guarded_mid(context_window_tokens: Option<u64>) -> Self {
        let context_window_tokens = context_window_tokens
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0)
            .unwrap_or(FALLBACK_CONTEXT_WINDOW);
        let active_loop_pressure_ratio = if context_window_tokens >= LARGE_CONTEXT_THRESHOLD {
            LARGE_CONTEXT_ACTIVE_PRESSURE
        } else {
            SMALL_CONTEXT_ACTIVE_PRESSURE
        };
        Self {
            context_window_tokens,
            active_loop_pressure_ratio,
            completed_exact_evidence_budget_tokens: COMPLETED_EXACT_EVIDENCE_BUDGET_TOKENS,
            active_exact_evidence_budget_tokens: ACTIVE_EXACT_EVIDENCE_BUDGET_TOKENS,
        }
    }

    fn active_loop_pressure_tokens(&self) -> usize {
        ((self.context_window_tokens as f32) * self.active_loop_pressure_ratio) as usize
    }
}

pub fn project_main_history(
    visible: &[SessionMessage],
    policy: ContextProjectionPolicy,
) -> Vec<SessionMessage> {
    let Some(active_start) = last_user_message_index(visible) else {
        return visible.to_vec();
    };

    let mut projected = project_completed_history(
        &visible[..active_start],
        policy.completed_exact_evidence_budget_tokens,
    );
    let active = &visible[active_start..];
    if estimate_messages_tokens(&projected) + estimate_messages_tokens(active)
        > policy.active_loop_pressure_tokens()
    {
        projected.extend(project_active_loop_under_pressure(
            active,
            policy.active_exact_evidence_budget_tokens,
        ));
    } else {
        projected.extend_from_slice(active);
    }
    projected
}

fn last_user_message_index(messages: &[SessionMessage]) -> Option<usize> {
    messages.iter().rposition(|message| {
        matches!(
            message,
            SessionMessage::Message {
                role: SessionRole::User,
                ..
            }
        )
    })
}

fn project_completed_history(
    messages: &[SessionMessage],
    exact_budget_tokens: usize,
) -> Vec<SessionMessage> {
    let mut projected = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        if matches!(
            messages[index],
            SessionMessage::Message {
                role: SessionRole::System,
                ..
            }
        ) {
            projected.push(messages[index].clone());
            index += 1;
            continue;
        }

        let loop_start = index;
        let mut loop_end = messages.len();
        for (relative, message) in messages[index + 1..].iter().enumerate() {
            if matches!(
                message,
                SessionMessage::Message {
                    role: SessionRole::User,
                    ..
                }
            ) {
                loop_end = index + 1 + relative;
                break;
            }
        }
        let loop_messages = &messages[loop_start..loop_end];
        if loop_has_tool_activity(loop_messages) {
            projected.extend(compact_completed_loop(loop_messages, exact_budget_tokens));
        } else {
            projected.extend_from_slice(loop_messages);
        }
        index = loop_end;
    }
    projected
}

fn project_active_loop_under_pressure(
    messages: &[SessionMessage],
    exact_budget_tokens: usize,
) -> Vec<SessionMessage> {
    let mut calls = HashMap::<String, ToolCallProjection>::new();
    let mut pending = Vec::<String>::new();
    let mut remaining_exact_budget = exact_budget_tokens;
    let mut projected = Vec::with_capacity(messages.len());

    for message in messages {
        match message {
            SessionMessage::ToolCall {
                id,
                call_id,
                name,
                input,
                ..
            } => {
                let resolved = resolve_call_id(id, call_id.as_deref());
                calls.insert(
                    resolved.clone(),
                    ToolCallProjection::from_tool_call(name, input),
                );
                pending.push(resolved);
                projected.push(message.clone());
            }
            SessionMessage::ToolResult {
                id,
                call_id,
                output,
                ..
            } => {
                let resolved = call_id
                    .as_deref()
                    .map(str::to_string)
                    .or_else(|| pending.first().cloned())
                    .unwrap_or_else(|| format!("call_{id}"));
                if pending.first() == Some(&resolved) {
                    pending.remove(0);
                }
                let call = calls.get(&resolved);
                let compacted =
                    compact_tool_output_for_model(call, output, &mut remaining_exact_budget);
                let mut cloned = message.clone();
                if let SessionMessage::ToolResult { output, .. } = &mut cloned {
                    *output = compacted;
                }
                projected.push(cloned);
            }
            _ => projected.push(message.clone()),
        }
    }

    projected
}

fn compact_completed_loop(
    messages: &[SessionMessage],
    exact_budget_tokens: usize,
) -> Vec<SessionMessage> {
    let mut user_messages = Vec::new();
    let mut assistant_texts = Vec::new();
    let mut tool_summaries = Vec::new();
    let mut calls = HashMap::<String, ToolCallProjection>::new();
    let mut pending = Vec::<String>::new();
    let mut remaining_exact_budget = exact_budget_tokens;

    for message in messages {
        match message {
            SessionMessage::Message {
                role: SessionRole::User,
                ..
            } => {
                user_messages.push(message.clone());
            }
            SessionMessage::Message {
                role: SessionRole::Assistant,
                content,
                ..
            } => {
                let text = join_content_text(content);
                if !text.trim().is_empty() {
                    assistant_texts.push(text);
                }
            }
            SessionMessage::ToolCall {
                id,
                call_id,
                name,
                input,
                ..
            } => {
                let resolved = resolve_call_id(id, call_id.as_deref());
                calls.insert(
                    resolved.clone(),
                    ToolCallProjection::from_tool_call(name, input),
                );
                pending.push(resolved);
            }
            SessionMessage::ToolResult {
                id,
                call_id,
                output,
                ..
            } => {
                let resolved = call_id
                    .as_deref()
                    .map(str::to_string)
                    .or_else(|| pending.first().cloned())
                    .unwrap_or_else(|| format!("call_{id}"));
                if pending.first() == Some(&resolved) {
                    pending.remove(0);
                }
                let call = calls.get(&resolved);
                tool_summaries.push(compact_tool_output_for_model(
                    call,
                    output,
                    &mut remaining_exact_budget,
                ));
            }
            SessionMessage::Reasoning { .. }
            | SessionMessage::ToolApprovalRequest { .. }
            | SessionMessage::ToolApprovalDecision { .. }
            | SessionMessage::Message {
                role: SessionRole::System,
                ..
            } => {}
        }
    }

    let tool_activity = if tool_summaries.is_empty() {
        "none".to_string()
    } else {
        tool_summaries
            .iter()
            .map(|item| indent_lines(item, "- "))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let assistant_result = if assistant_texts.is_empty() {
        "none".to_string()
    } else {
        indent_lines(&assistant_texts.join("\n\n"), "  ")
    };
    let summary = render_prompt_template(
        COMPLETED_LOOP_SUMMARY_TEMPLATE,
        &[
            ("tool_activity", tool_activity),
            ("assistant_result", assistant_result),
        ],
    );

    let mut compacted = user_messages;
    compacted.push(SessionMessage::Message {
        id: format!("projected_loop_summary_{}", stable_loop_suffix(messages)),
        scope: crate::session::SessionScope::main(),
        role: SessionRole::Assistant,
        content: vec![ContentItem::OutputText { text: summary }],
    });
    compacted
}

fn loop_has_tool_activity(messages: &[SessionMessage]) -> bool {
    messages.iter().any(|message| {
        matches!(
            message,
            SessionMessage::ToolCall { .. }
                | SessionMessage::ToolResult { .. }
                | SessionMessage::ToolApprovalRequest { .. }
                | SessionMessage::ToolApprovalDecision { .. }
        )
    })
}

fn compact_tool_output_for_model(
    call: Option<&ToolCallProjection>,
    output: &str,
    remaining_exact_budget: &mut usize,
) -> String {
    let capability = call
        .map(|call| call.capability.as_str())
        .unwrap_or("(unknown)");
    let mut detail_lines = Vec::new();
    if let Some(call) = call {
        detail_lines.extend(call.recovery_handles());
    }

    match capability {
        "read_file" => {
            append_read_file_summary(&mut detail_lines, output, remaining_exact_budget);
        }
        "process_start" | "process_read" | "process_watch" => {
            append_process_summary(&mut detail_lines, output);
        }
        "process_list" | "process_stop" | "process_write" => {
            append_json_preview(&mut detail_lines, output);
        }
        _ => {
            detail_lines.push(format!(
                "preview: {}",
                preview_tokens(output, TOOL_SUMMARY_PREVIEW_TOKENS)
            ));
        }
    }
    let purpose = call
        .map(|call| call.purpose.as_str())
        .filter(|purpose| !purpose.is_empty())
        .map(|purpose| format!("purpose: {purpose}"))
        .unwrap_or_default();
    let handles = if detail_lines.is_empty() {
        String::new()
    } else {
        detail_lines.join("\n")
    };
    render_prompt_template(
        TOOL_RESULT_SUMMARY_TEMPLATE,
        &[
            ("capability", capability.to_string()),
            ("purpose", purpose),
            ("handles", handles),
            ("details", String::new()),
        ],
    )
}

fn append_read_file_summary(
    lines: &mut Vec<String>,
    output: &str,
    remaining_exact_budget: &mut usize,
) {
    for tag in [
        "path",
        "sha256",
        "type",
        "size_bytes",
        "total_chars",
        "total_lines",
        "offset",
        "limit",
        "returned_lines",
        "truncated",
        "next_offset",
        "hint",
    ] {
        if let Some(value) = xml_tag(output, tag).filter(|value| !value.trim().is_empty()) {
            lines.push(format!("{tag}: {}", value.trim()));
        }
    }
    if let Some(content) = xml_tag(output, "content").filter(|value| !value.trim().is_empty()) {
        let content_tokens = token_count(&content);
        if content_tokens <= *remaining_exact_budget {
            *remaining_exact_budget = remaining_exact_budget.saturating_sub(content_tokens);
            lines.push("exact_evidence:".to_string());
            lines.push(indent_lines(content.trim(), "  "));
        } else {
            lines.push(format!(
                "content_preview: {}",
                preview_tokens(content.trim(), TOOL_SUMMARY_PREVIEW_TOKENS)
            ));
            lines.push(
                "content_recovery: call read_file with the same path and a narrower offset/limit for exact lines."
                    .to_string(),
            );
        }
    }
}

fn append_process_summary(lines: &mut Vec<String>, output: &str) {
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        lines.push(format!(
            "preview: {}",
            preview_tokens(output, TOOL_SUMMARY_PREVIEW_TOKENS)
        ));
        return;
    };
    for key in ["process_id", "status", "exit_code", "cursor", "truncated"] {
        if let Some(field) = value.get(key) {
            lines.push(format!("{key}: {}", compact_json_value(field)));
        }
    }
    if let Some(output) = value.get("output").and_then(Value::as_str) {
        lines.push(format!(
            "output_preview: {}",
            preview_tokens(output, TOOL_SUMMARY_PREVIEW_TOKENS)
        ));
    }
    lines.push(
        "output_recovery: call process_read with process_id plus cursor/mode/search query for exact log chunks."
            .to_string(),
    );
}

fn append_json_preview(lines: &mut Vec<String>, output: &str) {
    if let Ok(value) = serde_json::from_str::<Value>(output) {
        lines.push(format!(
            "json_preview: {}",
            preview_tokens(&compact_json_value(&value), TOOL_SUMMARY_PREVIEW_TOKENS)
        ));
    } else {
        lines.push(format!(
            "preview: {}",
            preview_tokens(output, TOOL_SUMMARY_PREVIEW_TOKENS)
        ));
    }
}

#[derive(Debug, Clone)]
struct ToolCallProjection {
    capability: String,
    purpose: String,
    args: Value,
}

impl ToolCallProjection {
    fn from_tool_call(tool_name: &str, input: &Value) -> Self {
        if tool_name == "call_capability" {
            return Self {
                capability: input
                    .get("capability")
                    .and_then(Value::as_str)
                    .unwrap_or("call_capability")
                    .trim()
                    .to_string(),
                purpose: input
                    .get("purpose")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                args: input.get("args").cloned().unwrap_or(Value::Null),
            };
        }
        Self {
            capability: tool_name.trim().to_string(),
            purpose: String::new(),
            args: input.clone(),
        }
    }

    fn recovery_handles(&self) -> Vec<String> {
        let mut handles = Vec::new();
        match self.capability.as_str() {
            "read_file" | "write_file" | "vision_analyze" => {
                if let Some(path) = self.args.get("path").and_then(Value::as_str) {
                    handles.push(format!("path: {path}"));
                }
                if let Some(offset) = self.args.get("offset").and_then(Value::as_u64) {
                    handles.push(format!("requested_offset: {offset}"));
                }
                if let Some(limit) = self.args.get("limit").and_then(Value::as_u64) {
                    handles.push(format!("requested_limit: {limit}"));
                }
            }
            "process_start" => {
                if let Some(command) = self.args.get("command").and_then(Value::as_str) {
                    handles.push(format!("command: {}", one_line(command)));
                }
                if let Some(cwd) = self.args.get("cwd").and_then(Value::as_str) {
                    handles.push(format!("cwd: {cwd}"));
                }
            }
            "process_read" | "process_stop" | "process_write" | "process_watch" => {
                if let Some(process_id) = self.args.get("process_id").and_then(Value::as_str) {
                    handles.push(format!("process_id: {process_id}"));
                }
                if let Some(mode) = self.args.get("mode").and_then(Value::as_str) {
                    handles.push(format!("mode: {mode}"));
                }
                if let Some(cursor) = self.args.get("cursor").and_then(Value::as_u64) {
                    handles.push(format!("cursor: {cursor}"));
                }
                if let Some(query) = self.args.get("query").and_then(Value::as_str) {
                    handles.push(format!("query: {query}"));
                }
            }
            _ => {}
        }
        handles
    }
}

fn estimate_messages_tokens(messages: &[SessionMessage]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

fn estimate_message_tokens(message: &SessionMessage) -> usize {
    match message {
        SessionMessage::Message { content, .. } => token_count(&join_content_text(content)),
        SessionMessage::ToolCall {
            content,
            name,
            input,
            ..
        } => token_count(&format!(
            "{}\n{name}\n{input}",
            content.as_deref().unwrap_or_default()
        )),
        SessionMessage::ToolResult { output, .. } => token_count(output),
        SessionMessage::Reasoning { content, .. } => token_count(content),
        SessionMessage::ToolApprovalRequest {
            command, options, ..
        } => token_count(&format!("{command}\n{}", options.join("\n"))),
        SessionMessage::ToolApprovalDecision {
            command, decision, ..
        } => token_count(&format!("{command}\n{decision}")),
    }
}

fn token_count(text: &str) -> usize {
    count_tool_tokens_cl100k(text).unwrap_or_else(|_| estimate_tokens_rough(text))
}

fn join_content_text(content: &[ContentItem]) -> String {
    content
        .iter()
        .map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => text.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn resolve_call_id(id: &str, call_id: Option<&str>) -> String {
    call_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("call_{id}"))
}

fn stable_loop_suffix(messages: &[SessionMessage]) -> String {
    messages
        .first()
        .map(|message| {
            message
                .id()
                .chars()
                .filter(|ch| ch.is_ascii_alphanumeric())
                .take(16)
                .collect()
        })
        .unwrap_or_else(|| "empty".to_string())
}

fn indent_lines(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn preview_tokens(text: &str, max_tokens: usize) -> String {
    if token_count(text) <= max_tokens {
        return one_line(text);
    }
    let approx_chars = max_tokens.saturating_mul(4).max(64);
    let mut preview = text.chars().take(approx_chars).collect::<String>();
    preview.push_str(" ...");
    one_line(&preview)
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn xml_tag(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(text[start..end].to_string())
}

fn compact_json_value(value: &Value) -> String {
    match value {
        Value::String(text) => one_line(text),
        _ => value.to_string(),
    }
}

fn render_prompt_template(template: &str, values: &[(&str, String)]) -> String {
    let mut rendered = template.to_string();
    for (key, value) in values {
        rendered = rendered.replace(&format!("{{{{{key}}}}}"), value);
    }
    rendered
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionManager, SessionScope};
    use serde_json::json;

    #[test]
    fn completed_tool_loop_becomes_recoverable_summary() {
        let messages = vec![
            SessionManager::new_text_message(SessionRole::System, "system"),
            SessionManager::new_text_message(SessionRole::User, "read file"),
            SessionManager::new_tool_use_message_with_scope(
                SessionScope::main(),
                "call_read",
                "call_capability",
                json!({
                    "capability": "read_file",
                    "purpose": "inspect file",
                    "args": {"path": "src/main.rs", "offset": 1, "limit": 500}
                }),
            ),
            SessionManager::new_tool_result_message_with_scope(
                SessionScope::main(),
                "call_read",
                "<path>src/main.rs</path>\n<sha256>abc</sha256>\n<offset>1</offset>\n<limit>500</limit>\n<next_offset>501</next_offset>\n<content>\n     1|fn main() {}\n</content>",
            ),
            SessionManager::new_text_message(SessionRole::Assistant, "done"),
            SessionManager::new_text_message(SessionRole::User, "next"),
        ];

        let projected = project_main_history(
            &messages,
            ContextProjectionPolicy::guarded_mid(Some(128_000)),
        );
        let rendered = projected
            .iter()
            .filter_map(SessionMessage::text_preview)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("[COMPLETED AGENT LOOP SUMMARY]"));
        assert!(rendered.contains("capability: read_file"));
        assert!(rendered.contains("path: src/main.rs"));
        assert!(rendered.contains("sha256: abc"));
        assert!(rendered.contains("assistant_result"));
        assert!(rendered.contains("next"));
        assert!(
            !projected
                .iter()
                .any(|message| matches!(message, SessionMessage::ToolResult { .. }))
        );
    }

    #[test]
    fn guarded_mid_uses_32k_for_unknown_context_window() {
        let policy = ContextProjectionPolicy::guarded_mid(None);
        assert_eq!(policy.context_window_tokens, 32_000);
        assert_eq!(policy.active_loop_pressure_tokens(), 25_600);
    }

    #[test]
    fn active_loop_stays_raw_below_pressure() {
        let messages = vec![
            SessionManager::new_text_message(SessionRole::System, "system"),
            SessionManager::new_text_message(SessionRole::User, "read file"),
            SessionManager::new_tool_use_message_with_scope(
                SessionScope::main(),
                "call_read",
                "call_capability",
                json!({"capability": "read_file", "args": {"path": "a.rs"}, "purpose": "inspect"}),
            ),
            SessionManager::new_tool_result_message_with_scope(
                SessionScope::main(),
                "call_read",
                "raw output",
            ),
        ];

        let projected = project_main_history(
            &messages,
            ContextProjectionPolicy::guarded_mid(Some(128_000)),
        );

        assert!(projected.iter().any(|message| match message {
            SessionMessage::ToolResult { output, .. } => output == "raw output",
            _ => false,
        }));
    }

    #[test]
    fn active_loop_compacts_tool_results_under_pressure() {
        let big = "x ".repeat(4_000);
        let messages = vec![
            SessionManager::new_text_message(SessionRole::System, "system"),
            SessionManager::new_text_message(SessionRole::User, "read file"),
            SessionManager::new_tool_use_message_with_scope(
                SessionScope::main(),
                "call_read",
                "call_capability",
                json!({"capability": "read_file", "args": {"path": "a.rs"}, "purpose": "inspect"}),
            ),
            SessionManager::new_tool_result_message_with_scope(
                SessionScope::main(),
                "call_read",
                format!("<path>a.rs</path>\n<content>{big}</content>"),
            ),
        ];

        let projected = project_main_history(
            &messages,
            ContextProjectionPolicy {
                context_window_tokens: 1_000,
                active_loop_pressure_ratio: 0.5,
                completed_exact_evidence_budget_tokens: 100,
                active_exact_evidence_budget_tokens: 100,
            },
        );

        let tool_output = projected
            .iter()
            .find_map(|message| match message {
                SessionMessage::ToolResult { output, .. } => Some(output.as_str()),
                _ => None,
            })
            .expect("tool result should remain paired with call");
        assert!(tool_output.contains("[TOOL RESULT SUMMARY]"));
        assert!(tool_output.contains("content_recovery"));
        assert!(!tool_output.contains(&big));
    }
}
