use crate::client::sse::{ensure_success_response, read_sse_events};
use crate::client::types::{AssistantToolCall, AssistantTurn, StreamUpdate};
use crate::model::{LanguageModelResponseContentType, Message, Tool};
use crate::provider::RuntimeProvider;
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

const VISION_ANALYZE_INSTRUCTIONS: &str = include_str!("../prompts/vision-analyze-instructions.md");

pub(crate) fn request(
    http: &Client,
    runtime: &RuntimeProvider,
    messages: &[Message],
    tools: &[Tool],
) -> Result<AssistantTurn> {
    let url = responses_url(&runtime.base_url);
    let payload = send_request(http, runtime, &url, messages, tools, false)?
        .json::<Value>()
        .context("failed to parse responses API payload")?;
    parse_response(&payload)
}

pub(crate) fn request_streaming(
    http: &Client,
    runtime: &RuntimeProvider,
    messages: &[Message],
    tools: &[Tool],
    on_update: &mut dyn FnMut(StreamUpdate),
) -> Result<AssistantTurn> {
    let url = responses_url(&runtime.base_url);
    let response = send_request(http, runtime, &url, messages, tools, true)?;
    let mut stream = ResponsesStreamState::default();
    read_sse_events(response, |data| {
        let chunk: Value =
            serde_json::from_str(data).context("failed to parse responses stream chunk")?;
        stream.apply_chunk(&chunk, on_update)
    })?;
    stream.finish()
}

pub(crate) fn request_image_analysis(
    http: &Client,
    runtime: &RuntimeProvider,
    path: &Path,
    mime: &str,
    question: &str,
) -> Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read image for responses API: {}", path.display()))?;
    let encoded = BASE64_STANDARD.encode(bytes);
    let data_url = format!("data:{mime};base64,{encoded}");
    let url = responses_url(&runtime.base_url);
    let body = json!({
        "model": runtime.model,
        "instructions": VISION_ANALYZE_INSTRUCTIONS.trim(),
        "input": [{
            "type": "message",
            "role": "user",
            "content": [
                { "type": "input_text", "text": question },
                { "type": "input_image", "image_url": data_url, "detail": "auto" }
            ]
        }],
        "store": false,
        "stream": true,
    });
    let mut request = http.post(&url).bearer_auth(&runtime.api_key);
    if let Some(account_id) = runtime.account_id.as_deref() {
        request = request.header("ChatGPT-Account-Id", account_id);
    }
    let response = request
        .json(&body)
        .send()
        .with_context(|| format!("failed POST {url}"))?;
    let response = ensure_success_response(&url, response)?;
    let mut stream = ResponsesStreamState::default();
    read_sse_events(response, |data| {
        let chunk: Value =
            serde_json::from_str(data).context("failed to parse responses vision stream chunk")?;
        stream.apply_chunk(&chunk, &mut |_| {})
    })?;
    stream
        .finish()?
        .text
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .ok_or_else(|| anyhow!("responses vision stream did not contain text output"))
}

fn send_request(
    http: &Client,
    runtime: &RuntimeProvider,
    url: &str,
    messages: &[Message],
    tools: &[Tool],
    stream: bool,
) -> Result<reqwest::blocking::Response> {
    let (instructions, input) = to_responses_input(messages);
    let body = json!({
        "model": runtime.model,
        "instructions": instructions,
        "input": input,
        "tools": build_responses_tools(tools),
        "tool_choice": if tools.is_empty() { Value::Null } else { Value::String("auto".to_string()) },
        "store": false,
        "stream": stream,
    });

    let mut request = http.post(url).bearer_auth(&runtime.api_key);
    if let Some(account_id) = runtime.account_id.as_deref() {
        request = request.header("ChatGPT-Account-Id", account_id);
    }

    request
        .json(&body)
        .send()
        .with_context(|| format!("failed POST {url}"))
        .and_then(|response| ensure_success_response(url, response))
}

#[derive(Default)]
struct ResponsesToolBuffer {
    item_id: String,
    call_id: String,
    name: String,
    raw_arguments: String,
}

#[derive(Default)]
struct ResponsesStreamState {
    text: String,
    reasoning: String,
    tools_by_index: BTreeMap<u64, ResponsesToolBuffer>,
    final_response: Option<Value>,
}

impl ResponsesStreamState {
    fn apply_chunk(
        &mut self,
        chunk: &Value,
        on_update: &mut dyn FnMut(StreamUpdate),
    ) -> Result<()> {
        match chunk
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "response.output_text.delta" => {
                if let Some(delta) = chunk.get("delta").and_then(Value::as_str) {
                    self.text.push_str(delta);
                    on_update(StreamUpdate::TextDelta(delta.to_string()));
                }
            }
            "response.reasoning_text.delta" => {
                if let Some(delta) = chunk.get("delta").and_then(Value::as_str) {
                    self.reasoning.push_str(delta);
                }
            }
            "response.output_item.added" => {
                if let Some(item) = chunk.get("item") {
                    let output_index = chunk
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        let entry = self.tools_by_index.entry(output_index).or_default();
                        entry.item_id = item
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        entry.call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        entry.name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let output_index = chunk
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let entry = self.tools_by_index.entry(output_index).or_default();
                if entry.item_id.is_empty() {
                    entry.item_id = chunk
                        .get("item_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                }
                if let Some(delta) = chunk.get("delta").and_then(Value::as_str) {
                    entry.raw_arguments.push_str(delta);
                    on_update(StreamUpdate::ToolCallDelta(entry.raw_arguments.clone()));
                }
            }
            "response.completed" | "response.incomplete" => {
                self.final_response = chunk.get("response").cloned();
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self) -> Result<AssistantTurn> {
        if let Some(response) = self.final_response {
            let mut turn = parse_response(&response)?;
            if turn.text.is_none() && !self.text.trim().is_empty() {
                turn.text = Some(self.text);
            }
            if turn.reasoning.is_none() && !self.reasoning.trim().is_empty() {
                turn.reasoning = Some(self.reasoning);
            }
            if turn.tool_calls.is_empty() {
                turn.tool_calls = self
                    .tools_by_index
                    .into_values()
                    .map(response_tool_buffer_to_final)
                    .collect::<Result<Vec<_>>>()?;
            }
            return Ok(turn);
        }

        let mut turn = AssistantTurn::default();
        if !self.text.trim().is_empty() {
            turn.text = Some(self.text);
        }
        if !self.reasoning.trim().is_empty() {
            turn.reasoning = Some(self.reasoning);
        }
        turn.tool_calls = self
            .tools_by_index
            .into_values()
            .map(response_tool_buffer_to_final)
            .collect::<Result<Vec<_>>>()?;
        Ok(turn)
    }
}

fn response_tool_buffer_to_final(tool: ResponsesToolBuffer) -> Result<AssistantToolCall> {
    let input = if tool.raw_arguments.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        serde_json::from_str(&tool.raw_arguments)
            .unwrap_or_else(|_| Value::String(tool.raw_arguments.clone()))
    };
    Ok(AssistantToolCall {
        call_id: tool.call_id,
        name: tool.name,
        input,
    })
}

fn responses_url(base_url: &str) -> String {
    if base_url.ends_with("/responses") {
        base_url.to_string()
    } else {
        format!("{}/responses", base_url.trim_end_matches('/'))
    }
}

fn build_responses_tools(tools: &[Tool]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": responses_tool_schema(&tool.input_schema),
                    "strict": false,
                })
            })
            .collect(),
    )
}

fn responses_tool_schema(schema: &Value) -> Value {
    let mut schema = schema.clone();
    normalize_responses_schema_node(&mut schema);
    schema
}

fn normalize_responses_schema_node(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("$schema");
            map.remove("title");
            map.remove("default");
            map.remove("examples");

            for child in map.values_mut() {
                normalize_responses_schema_node(child);
            }

            let is_object_schema = map.get("properties").and_then(Value::as_object).is_some()
                || map
                    .get("type")
                    .is_some_and(json_schema_type_includes_object);

            if is_object_schema {
                map.insert("additionalProperties".to_string(), Value::Bool(false));
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_responses_schema_node(item);
            }
        }
        _ => {}
    }
}

fn json_schema_type_includes_object(value: &Value) -> bool {
    match value {
        Value::String(kind) => kind == "object",
        Value::Array(kinds) => kinds
            .iter()
            .any(|kind| kind.as_str().is_some_and(|kind| kind == "object")),
        _ => false,
    }
}

fn to_responses_input(messages: &[Message]) -> (Option<String>, Vec<Value>) {
    let mut instructions = None;
    let mut out = Vec::new();

    for message in messages {
        match message {
            Message::System(system) => {
                if instructions.is_none() {
                    instructions = Some(system.content.clone());
                } else {
                    out.push(json!({
                        "type": "message",
                        "role": "developer",
                        "content": [{ "type": "input_text", "text": system.content }],
                    }));
                }
            }
            Message::Developer(text) => out.push(json!({
                "type": "message",
                "role": "developer",
                "content": [{ "type": "input_text", "text": text }],
            })),
            Message::User(user) => out.push(json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": user.content }],
            })),
            Message::Assistant(assistant) => match &assistant.content {
                LanguageModelResponseContentType::Text(text) => out.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": text }],
                })),
                LanguageModelResponseContentType::Reasoning { .. } => {}
                LanguageModelResponseContentType::ToolCall(tool_call) => out.push(json!({
                    "type": "function_call",
                    "call_id": tool_call.tool.id,
                    "name": tool_call.tool.name,
                    "arguments": tool_call.input.to_string(),
                })),
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
                    "type": "function_call_output",
                    "call_id": tool_result.tool.id,
                    "output": content,
                }));
            }
        }
    }

    (instructions, out)
}

pub(crate) fn parse_response(payload: &Value) -> Result<AssistantTurn> {
    let output = payload
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("responses API payload missing output"))?;
    let mut turn = AssistantTurn::default();
    let mut texts = Vec::new();

    for item in output {
        match item.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message" => {
                if let Some(contents) = item.get("content").and_then(Value::as_array) {
                    for content in contents {
                        let kind = content
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if matches!(kind, "output_text" | "text")
                            && let Some(text) = content.get("text").and_then(Value::as_str)
                        {
                            texts.push(text.to_string());
                        }
                    }
                }
            }
            "function_call" => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("responses function_call missing call_id"))?
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("responses function_call missing name"))?
                    .to_string();
                let raw_arguments = item
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
            "reasoning" => {
                let mut parts = Vec::new();
                if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                    for part in summary {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            parts.push(text.to_string());
                        }
                    }
                }
                if !parts.is_empty() {
                    turn.reasoning = Some(parts.join(""));
                }
            }
            _ => {}
        }
    }

    if !texts.is_empty() {
        turn.text = Some(texts.join(""));
    }
    Ok(turn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn responses_tool_schema_closes_root_object_without_forcing_required() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "SampleToolInput",
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "description": { "type": "string" },
                "task": { "type": "string" },
                "acceptanceCriteria": { "type": "string" }
            },
            "required": ["name", "description"]
        });

        let strict = responses_tool_schema(&schema);

        assert_eq!(
            strict.get("additionalProperties").and_then(Value::as_bool),
            Some(false)
        );
        assert!(strict.get("$schema").is_none());
        assert!(strict.get("title").is_none());
        assert_eq!(strict["required"], json!(["name", "description"]));
    }

    #[test]
    fn parse_responses_vision_text_output() -> Result<()> {
        let payload = json!({
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "This is an image"
                }]
            }]
        });
        let turn = parse_response(&payload)?;
        assert_eq!(turn.text.as_deref(), Some("This is an image"));
        Ok(())
    }

    #[test]
    fn responses_tool_schema_closes_nested_objects_without_forcing_required() {
        let schema = json!({
            "type": "object",
            "properties": {
                "outer": {
                    "type": "object",
                    "properties": {
                        "inner": { "type": "string" }
                    }
                },
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "value": { "type": "string" }
                        }
                    }
                }
            }
        });

        let strict = responses_tool_schema(&schema);
        let outer = &strict["properties"]["outer"];
        let item = &strict["properties"]["items"]["items"];

        assert_eq!(
            outer.get("additionalProperties").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            item.get("additionalProperties").and_then(Value::as_bool),
            Some(false)
        );
        assert!(outer.get("required").is_none());
        assert!(item.get("required").is_none());
    }
}
