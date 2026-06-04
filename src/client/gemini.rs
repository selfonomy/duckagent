use crate::client::sse::{ensure_success_response, read_sse_events};
use crate::client::types::{AssistantToolCall, AssistantTurn, StreamUpdate};
use crate::model::{LanguageModelResponseContentType, Message, Tool};
use crate::provider::RuntimeProvider;
use anyhow::{Context, Result, anyhow};
use reqwest::blocking::Client;
use serde_json::{Map, Value, json};

pub(crate) fn request(
    http: &Client,
    runtime: &RuntimeProvider,
    messages: &[Message],
    tools: &[Tool],
) -> Result<AssistantTurn> {
    request_inner(http, runtime, messages, tools, false, None)
}

pub(crate) fn request_streaming(
    http: &Client,
    runtime: &RuntimeProvider,
    messages: &[Message],
    tools: &[Tool],
    on_update: &mut dyn FnMut(StreamUpdate),
) -> Result<AssistantTurn> {
    request_inner(http, runtime, messages, tools, true, Some(on_update))
}

fn request_inner(
    http: &Client,
    runtime: &RuntimeProvider,
    messages: &[Message],
    tools: &[Tool],
    stream: bool,
    on_update: Option<&mut dyn FnMut(StreamUpdate)>,
) -> Result<AssistantTurn> {
    let body = build_gemini_body(messages, tools);
    let url = if stream {
        format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            runtime.base_url.trim_end_matches('/'),
            runtime.model
        )
    } else {
        format!(
            "{}/models/{}:generateContent",
            runtime.base_url.trim_end_matches('/'),
            runtime.model
        )
    };
    let mut request = http.post(&url).json(&body);
    if !runtime.api_key.is_empty() {
        request = request.query(&[("key", runtime.api_key.as_str())]);
    }
    let response = request
        .send()
        .with_context(|| format!("failed POST {url}"))
        .and_then(|response| ensure_success_response(&url, response))?;
    if stream {
        let mut state = GeminiStreamState::default();
        let mut on_update = on_update;
        read_sse_events(response, |data| {
            let chunk: Value =
                serde_json::from_str(data).context("failed to parse Gemini SSE chunk")?;
            let turn = parse_response(&chunk)?;
            if let Some(text) = turn.text {
                if let Some(callback) = on_update.as_mut() {
                    callback(StreamUpdate::TextDelta(text.clone()));
                }
                state.text.push_str(&text);
            }
            state.tool_calls.extend(turn.tool_calls);
            Ok(())
        })?;
        state.finish()
    } else {
        let payload: Value = response.json().context("failed to parse Gemini response")?;
        parse_response(&payload)
    }
}

pub(crate) fn build_gemini_body(messages: &[Message], tools: &[Tool]) -> Value {
    let (contents, system_instruction) = to_gemini_contents(messages);
    let mut body = json!({ "contents": contents });
    if let Some(system) = system_instruction {
        body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
    }
    let declarations: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": sanitize_gemini_schema(&tool.input_schema),
            })
        })
        .collect();
    if !declarations.is_empty() {
        body["tools"] = json!([{ "functionDeclarations": declarations }]);
        body["toolConfig"] = json!({
            "functionCallingConfig": { "mode": "AUTO" }
        });
    }
    body
}

fn to_gemini_contents(messages: &[Message]) -> (Vec<Value>, Option<String>) {
    let mut system = None;
    let mut out = Vec::new();
    for message in messages {
        match message {
            Message::System(item) => {
                if system.is_none() {
                    system = Some(item.content.clone());
                }
            }
            Message::Developer(_) => {}
            Message::User(user) => out.push(json!({
                "role": "user",
                "parts": [{ "text": user.content }]
            })),
            Message::Assistant(assistant) => match &assistant.content {
                LanguageModelResponseContentType::Text(text) => out.push(json!({
                    "role": "model",
                    "parts": [{ "text": text }]
                })),
                LanguageModelResponseContentType::Reasoning { .. } => {}
                LanguageModelResponseContentType::ToolCall(tool_call) => out.push(json!({
                    "role": "model",
                    "parts": [{
                        "functionCall": {
                            "name": tool_call.tool.name,
                            "args": tool_call.input
                        }
                    }]
                })),
            },
            Message::Tool(tool_result) => {
                let content = match &tool_result.output {
                    Ok(value) => value.clone(),
                    Err(err) => json!({ "error": err.to_string() }),
                };
                out.push(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": tool_result.tool.name,
                            "response": { "result": content }
                        }
                    }]
                }));
            }
        }
    }
    (out, system)
}

fn sanitize_gemini_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                if matches!(
                    key.as_str(),
                    "$schema" | "additionalProperties" | "unevaluatedProperties"
                ) {
                    continue;
                }
                out.insert(key.clone(), sanitize_gemini_schema(value));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sanitize_gemini_schema).collect()),
        _ => schema.clone(),
    }
}

pub(crate) fn parse_response(payload: &Value) -> Result<AssistantTurn> {
    let parts = payload
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|candidate| candidate.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Gemini response missing candidates[0].content.parts"))?;
    let mut turn = AssistantTurn::default();
    let mut texts = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            texts.push(text.to_string());
        }
        if let Some(function_call) = part.get("functionCall") {
            let name = function_call
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("Gemini functionCall missing name"))?
                .to_string();
            let input = function_call
                .get("args")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            turn.tool_calls.push(AssistantToolCall {
                call_id: format!("gemini_call_{index}"),
                name,
                input,
            });
        }
    }
    if !texts.is_empty() {
        turn.text = Some(texts.join(""));
    }
    Ok(turn)
}

#[derive(Default)]
pub(crate) struct GeminiStreamState {
    pub(crate) text: String,
    pub(crate) tool_calls: Vec<AssistantToolCall>,
}

impl GeminiStreamState {
    pub(crate) fn finish(self) -> Result<AssistantTurn> {
        Ok(AssistantTurn {
            text: (!self.text.trim().is_empty()).then_some(self.text),
            reasoning: None,
            tool_calls: self.tool_calls,
        })
    }
}
