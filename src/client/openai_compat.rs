use crate::client::sse::{ensure_success_response, read_sse_events};
use crate::client::types::{
    AssistantToolCall, AssistantTurn, PartialToolCall, StreamUpdate, partial_tool_calls_to_final,
};
use crate::model::{LanguageModelResponseContentType, Message, Tool};
use crate::provider::{ProviderKind, RuntimeProvider};
use anyhow::{Context, Result, anyhow};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
struct OpenAiCompatPolicy {
    deepseek_thinking: bool,
}

impl OpenAiCompatPolicy {
    fn for_provider(provider: ProviderKind) -> Self {
        Self {
            deepseek_thinking: provider == ProviderKind::DeepSeek,
        }
    }
}

pub(crate) fn request(
    http: &Client,
    runtime: &RuntimeProvider,
    messages: &[Message],
    tools: &[Tool],
) -> Result<AssistantTurn> {
    let url = chat_completions_url(&runtime.base_url);
    let payload = send_request(http, runtime, &url, messages, tools, false)?
        .json::<Value>()
        .context("failed to parse chat completions response")?;
    parse_response(&payload)
}

pub(crate) fn request_streaming(
    http: &Client,
    runtime: &RuntimeProvider,
    messages: &[Message],
    tools: &[Tool],
    on_update: &mut dyn FnMut(StreamUpdate),
) -> Result<AssistantTurn> {
    let url = chat_completions_url(&runtime.base_url);
    let response = send_request(http, runtime, &url, messages, tools, true)?;
    let mut stream = ChatCompletionStreamState::default();
    read_sse_events(response, |data| {
        if data == "[DONE]" {
            return Ok(());
        }
        let chunk: Value =
            serde_json::from_str(data).context("failed to parse chat completions stream chunk")?;
        stream.apply_chunk(&chunk, on_update)
    })?;
    stream.finish()
}

fn send_request(
    http: &Client,
    runtime: &RuntimeProvider,
    url: &str,
    messages: &[Message],
    tools: &[Tool],
    stream: bool,
) -> Result<reqwest::blocking::Response> {
    let policy = OpenAiCompatPolicy::for_provider(runtime.provider);
    let mut body = json!({
        "model": runtime.model,
        "messages": to_chat_completion_messages(messages),
        "tools": build_openai_tools(tools),
        "tool_choice": if tools.is_empty() { Value::Null } else { Value::String("auto".to_string()) },
        "stream": stream,
        "stream_options": if stream { json!({"include_usage": true}) } else { Value::Null },
    });
    if policy.deepseek_thinking {
        body["reasoning_effort"] = Value::String("high".to_string());
        body["thinking"] = json!({ "type": "enabled" });
    }

    http.post(url)
        .bearer_auth(&runtime.api_key)
        .json(&body)
        .send()
        .with_context(|| format!("failed POST {url}"))
        .and_then(|response| ensure_success_response(url, response))
}

#[derive(Default)]
struct ChatCompletionStreamState {
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<usize, PartialToolCall>,
}

impl ChatCompletionStreamState {
    fn apply_chunk(
        &mut self,
        chunk: &Value,
        on_update: &mut dyn FnMut(StreamUpdate),
    ) -> Result<()> {
        let Some(delta) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("delta"))
        else {
            return Ok(());
        };

        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            self.text.push_str(text);
            on_update(StreamUpdate::TextDelta(text.to_string()));
        }

        if let Some(reasoning) = delta
            .get("reasoning_content")
            .and_then(Value::as_str)
            .or_else(|| delta.get("reasoning").and_then(Value::as_str))
        {
            self.reasoning.push_str(reasoning);
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for tool_call in tool_calls {
                let index = tool_call
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.tool_calls.len() as u64) as usize;
                let partial = self.tool_calls.entry(index).or_default();
                if let Some(id) = tool_call.get("id").and_then(Value::as_str) {
                    partial.call_id = id.to_string();
                }
                if let Some(function) = tool_call.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        partial.name = name.to_string();
                    }
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                        partial.raw_arguments.push_str(arguments);
                        on_update(StreamUpdate::ToolCallDelta(partial.raw_arguments.clone()));
                    }
                }
            }
        }

        Ok(())
    }

    fn finish(self) -> Result<AssistantTurn> {
        let mut turn = AssistantTurn::default();
        if !self.text.trim().is_empty() {
            turn.text = Some(self.text);
        }
        if !self.reasoning.trim().is_empty() {
            turn.reasoning = Some(self.reasoning);
        }
        turn.tool_calls = partial_tool_calls_to_final(self.tool_calls)?;
        Ok(turn)
    }
}

fn chat_completions_url(base_url: &str) -> String {
    if base_url.ends_with("/chat/completions") {
        base_url.to_string()
    } else {
        format!("{}/chat/completions", base_url.trim_end_matches('/'))
    }
}

fn build_openai_tools(tools: &[Tool]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    }
                })
            })
            .collect(),
    )
}

pub(crate) fn to_chat_completion_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut pending_reasoning: Option<String> = None;
    let mut index = 0;

    while index < messages.len() {
        match &messages[index] {
            Message::System(system) => {
                out.push(json!({ "role": "system", "content": system.content }));
            }
            Message::Developer(text) => {
                out.push(json!({ "role": "developer", "content": text }));
            }
            Message::User(user) => {
                out.push(json!({ "role": "user", "content": user.content }));
            }
            Message::Assistant(assistant) => match &assistant.content {
                LanguageModelResponseContentType::Text(text) => {
                    let mut item = json!({ "role": "assistant", "content": text });
                    if let Some(reasoning) = pending_reasoning.take() {
                        item["reasoning_content"] = Value::String(reasoning);
                    }
                    out.push(item);
                }
                LanguageModelResponseContentType::Reasoning { content } => {
                    pending_reasoning = Some(content.clone());
                }
                LanguageModelResponseContentType::ToolCall(_) => {
                    let mut content_parts = Vec::new();
                    let mut tool_calls = Vec::new();
                    let mut scan = index;
                    while let Some(Message::Assistant(assistant)) = messages.get(scan) {
                        let LanguageModelResponseContentType::ToolCall(tool_call) =
                            &assistant.content
                        else {
                            break;
                        };
                        if let Some(content) = tool_call
                            .content
                            .as_ref()
                            .filter(|content| !content.trim().is_empty())
                        {
                            if !content_parts.iter().any(|existing| existing == content) {
                                content_parts.push(content.clone());
                            }
                        }
                        tool_calls.push(json!({
                            "id": tool_call.tool.id,
                            "type": "function",
                            "function": {
                                "name": tool_call.tool.name,
                                "arguments": tool_call.input.to_string(),
                            }
                        }));
                        scan += 1;
                    }
                    let content = content_parts.join("\n\n");
                    let mut item = json!({
                        "role": "assistant",
                        "content": content,
                        "tool_calls": tool_calls
                    });
                    if let Some(reasoning) = pending_reasoning.take() {
                        item["reasoning_content"] = Value::String(reasoning);
                    }
                    out.push(item);
                    index = scan;
                    continue;
                }
            },
            Message::Tool(tool_result) => {
                let content = match &tool_result.output {
                    Ok(value) => value
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| value.to_string()),
                    Err(err) => format!("Error: {err}"),
                };
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_result.tool.id,
                    "content": content,
                }));
            }
        }
        index += 1;
    }

    out
}

pub(crate) fn parse_response(payload: &Value) -> Result<AssistantTurn> {
    let message = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .ok_or_else(|| anyhow!("chat completions response missing choices[0].message"))?;

    let mut turn = AssistantTurn {
        text: message
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|text| !text.trim().is_empty()),
        reasoning: message
            .get("reasoning_content")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                message
                    .get("reasoning")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
        tool_calls: Vec::new(),
    };

    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for tool_call in tool_calls {
            let call_id = tool_call
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("tool call missing id"))?
                .to_string();
            let function = tool_call
                .get("function")
                .ok_or_else(|| anyhow!("tool call missing function"))?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("tool call missing function.name"))?
                .to_string();
            let raw_arguments = function
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let input = serde_json::from_str(raw_arguments)
                .unwrap_or_else(|_| Value::String(raw_arguments.to_string()));
            turn.tool_calls.push(AssistantToolCall {
                call_id,
                name,
                input,
            });
        }
    }

    Ok(turn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssistantMessage, ToolCallInfo};

    #[test]
    fn parse_chat_completion_tool_call_response() -> Result<()> {
        let payload = json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {
                            "name": "shell",
                            "arguments": "{\"command\":\"pwd\"}"
                        }
                    }]
                }
            }]
        });
        let parsed = parse_response(&payload)?;
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].call_id, "call_1");
        Ok(())
    }

    #[test]
    fn chat_completion_messages_group_parallel_tool_calls_under_one_reasoning() {
        let mut first_tool_call = ToolCallInfo::new("shell".to_string());
        first_tool_call.id("call_1".to_string());
        first_tool_call.content("I'll inspect both files now.".to_string());
        first_tool_call.input(json!({ "command": "ls -la" }));
        let mut second_tool_call = ToolCallInfo::new("shell".to_string());
        second_tool_call.id("call_2".to_string());
        second_tool_call.content("I'll inspect both files now.".to_string());
        second_tool_call.input(json!({ "command": "cat AGENTS.md" }));
        let messages = vec![
            Message::Assistant(AssistantMessage::new(
                LanguageModelResponseContentType::Reasoning {
                    content: "parallel tool reasoning".to_string(),
                },
                None,
            )),
            Message::Assistant(AssistantMessage::new(
                LanguageModelResponseContentType::ToolCall(first_tool_call),
                None,
            )),
            Message::Assistant(AssistantMessage::new(
                LanguageModelResponseContentType::ToolCall(second_tool_call),
                None,
            )),
        ];

        let serialized = to_chat_completion_messages(&messages);

        assert_eq!(serialized.len(), 1);
        assert_eq!(
            serialized[0]["reasoning_content"].as_str(),
            Some("parallel tool reasoning")
        );
        assert_eq!(
            serialized[0]["content"].as_str(),
            Some("I'll inspect both files now.")
        );
        assert_eq!(serialized[0]["tool_calls"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn generic_policy_does_not_emit_deepseek_only_fields() {
        let policy = OpenAiCompatPolicy::for_provider(ProviderKind::OpenAi);
        assert!(!policy.deepseek_thinking);
    }
}
