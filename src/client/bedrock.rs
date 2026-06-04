use crate::client::types::{AssistantToolCall, AssistantTurn};
use crate::model::{LanguageModelResponseContentType, Message, Tool};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::fs;
use std::process::Command;

/// Bedrock transport currently uses the official AWS CLI as its credentialed
/// subprocess boundary. The module owns Converse message shape so the agent
/// loop can later swap this internals for the AWS Rust SDK without touching
/// other transports.
pub(crate) fn request(model: &str, messages: &[Message], tools: &[Tool]) -> Result<AssistantTurn> {
    let body = build_converse_body(model, messages, tools);
    let path =
        std::env::temp_dir().join(format!("duckagent-bedrock-{}.json", uuid::Uuid::now_v7()));
    fs::write(&path, body.to_string()).context("failed to write Bedrock request body")?;
    let path_arg = format!("file://{}", path.to_string_lossy());
    let output = Command::new("aws")
        .args(["bedrock-runtime", "converse", "--cli-input-json", &path_arg])
        .output()
        .context("failed to execute aws bedrock-runtime converse")?;
    let _ = fs::remove_file(&path);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Bedrock Converse failed: {}", stderr.trim());
    }
    let payload: Value = serde_json::from_slice(&output.stdout)
        .context("failed to parse Bedrock Converse response")?;
    parse_response(&payload)
}

fn build_converse_body(model: &str, messages: &[Message], tools: &[Tool]) -> Value {
    let (system, bedrock_messages) = to_bedrock_messages(messages);
    let mut body = json!({
        "modelId": model,
        "messages": bedrock_messages,
        "inferenceConfig": { "maxTokens": 4096 }
    });
    if let Some(system) = system {
        body["system"] = json!(system);
    }
    if !tools.is_empty() && model_supports_tool_use(model) {
        body["toolConfig"] = json!({
            "tools": tools.iter().map(|tool| json!({
                "toolSpec": {
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": { "json": tool.input_schema }
                }
            })).collect::<Vec<_>>()
        });
    }
    body
}

fn to_bedrock_messages(messages: &[Message]) -> (Option<Vec<Value>>, Vec<Value>) {
    let mut system = Vec::new();
    let mut out = Vec::new();
    for message in messages {
        match message {
            Message::System(system_message) => {
                if !system_message.content.trim().is_empty() {
                    system.push(json!({ "text": system_message.content }));
                }
            }
            Message::Developer(text) => {
                push_bedrock_blocks(&mut out, "user", vec![text_block(text)]);
            }
            Message::User(user) => {
                push_bedrock_blocks(&mut out, "user", vec![text_block(&user.content)]);
            }
            Message::Assistant(assistant) => match &assistant.content {
                LanguageModelResponseContentType::Text(text) => {
                    push_bedrock_blocks(&mut out, "assistant", vec![text_block(text)]);
                }
                LanguageModelResponseContentType::Reasoning { .. } => {}
                LanguageModelResponseContentType::ToolCall(tool_call) => {
                    push_bedrock_blocks(
                        &mut out,
                        "assistant",
                        vec![json!({
                            "toolUse": {
                                "toolUseId": tool_call.tool.id,
                                "name": tool_call.tool.name,
                                "input": tool_call.input
                            }
                        })],
                    );
                }
            },
            Message::Tool(tool) => {
                let text = tool
                    .output
                    .as_ref()
                    .map(|value| {
                        value
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| value.to_string())
                    })
                    .unwrap_or_else(|err| err.to_string());
                push_bedrock_blocks(
                    &mut out,
                    "user",
                    vec![json!({
                        "toolResult": {
                            "toolUseId": tool.tool.id,
                            "content": [{ "text": non_empty_text(&text) }]
                        }
                    })],
                );
            }
        }
    }
    if out
        .first()
        .and_then(|item| item.get("role"))
        .and_then(Value::as_str)
        != Some("user")
    {
        out.insert(0, json!({"role": "user", "content": [{ "text": " " }]}));
    }
    if out
        .last()
        .and_then(|item| item.get("role"))
        .and_then(Value::as_str)
        != Some("user")
    {
        out.push(json!({"role": "user", "content": [{ "text": " " }]}));
    }
    ((!system.is_empty()).then_some(system), out)
}

fn push_bedrock_blocks(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        content.extend(blocks);
        return;
    }
    messages.push(json!({"role": role, "content": blocks}));
}

fn text_block(text: &str) -> Value {
    json!({ "text": non_empty_text(text) })
}

fn non_empty_text(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        " ".to_string()
    } else {
        text.to_string()
    }
}

fn model_supports_tool_use(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    !(lower.contains("deepseek.r1") || lower.contains("reasoning-only"))
}

pub(crate) fn parse_response(payload: &Value) -> Result<AssistantTurn> {
    let content = payload
        .pointer("/output/message/content")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Bedrock response missing output.message.content"))?;
    let mut turn = AssistantTurn::default();
    let mut texts = Vec::new();
    let mut reasoning = Vec::new();
    for item in content {
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            texts.push(text.to_string());
        }
        if let Some(reasoning_content) = item.get("reasoningContent")
            && let Some(text) = reasoning_content
                .get("text")
                .and_then(Value::as_str)
                .or_else(|| reasoning_content.as_str())
        {
            reasoning.push(text.to_string());
        }
        if let Some(tool_use) = item.get("toolUse") {
            turn.tool_calls.push(AssistantToolCall {
                call_id: tool_use
                    .get("toolUseId")
                    .and_then(Value::as_str)
                    .unwrap_or("bedrock_tool_use")
                    .to_string(),
                name: tool_use
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                input: tool_use
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Map::new())),
            });
        }
    }
    if !texts.is_empty() {
        turn.text = Some(texts.join(""));
    }
    if !reasoning.is_empty() {
        turn.reasoning = Some(reasoning.join(""));
    }
    Ok(turn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AssistantMessage;

    #[test]
    fn bedrock_messages_merge_roles_and_end_with_user() {
        let messages = vec![
            Message::Assistant(AssistantMessage::from("hello")),
            Message::User("next".into()),
        ];
        let (_, out) = to_bedrock_messages(&messages);
        assert_eq!(out.first().unwrap()["role"].as_str(), Some("user"));
        assert_eq!(out.last().unwrap()["role"].as_str(), Some("user"));
    }

    #[test]
    fn bedrock_tool_use_parses() -> Result<()> {
        let payload = json!({
            "output": { "message": { "content": [
                { "toolUse": { "toolUseId": "t1", "name": "shell", "input": {"command":"pwd"} } }
            ]}}
        });
        let turn = parse_response(&payload)?;
        assert_eq!(turn.tool_calls[0].name, "shell");
        Ok(())
    }

    #[test]
    fn bedrock_tool_config_stripped_for_known_non_tool_model() {
        let tool = Tool {
            name: "shell".to_string(),
            description: "run shell".to_string(),
            input_schema: json!({"type":"object"}),
            execute: std::sync::Arc::new(|_| Ok("ok".to_string())),
        };
        let body = build_converse_body("deepseek.r1-v1:0", &[Message::User("hi".into())], &[tool]);
        assert!(body.get("toolConfig").is_none());
    }
}
