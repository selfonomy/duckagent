use crate::client::sse::{ensure_success_response, read_sse_events};
use crate::client::types::{AssistantToolCall, AssistantTurn, StreamUpdate};
use crate::model::{LanguageModelResponseContentType, Message, Tool};
use crate::provider::RuntimeProvider;
use anyhow::{Context, Result, anyhow};
use reqwest::blocking::Client;
use reqwest::blocking::RequestBuilder;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const ANTHROPIC_VERSION: &str = "2023-06-01";
const COMMON_BETAS: &[&str] = &[
    "interleaved-thinking-2025-05-14",
    "fine-grained-tool-streaming-2025-05-14",
];
const TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
const OAUTH_ONLY_BETAS: &[&str] = &["claude-code-20250219", "oauth-2025-04-20"];
const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.74 (external, cli)";
const KIMI_CODING_USER_AGENT: &str = "claude-code/0.1.0";

pub(crate) fn request(
    http: &Client,
    runtime: &RuntimeProvider,
    messages: &[Message],
    tools: &[Tool],
) -> Result<AssistantTurn> {
    let url = anthropic_messages_url(&runtime.base_url);
    let payload = send_request(http, runtime, &url, messages, tools, false)?
        .json::<Value>()
        .context("failed to parse anthropic response")?;
    parse_response(&payload)
}

pub(crate) fn request_streaming(
    http: &Client,
    runtime: &RuntimeProvider,
    messages: &[Message],
    tools: &[Tool],
    on_update: &mut dyn FnMut(StreamUpdate),
) -> Result<AssistantTurn> {
    let url = anthropic_messages_url(&runtime.base_url);
    let response = send_request(http, runtime, &url, messages, tools, true)?;
    let mut stream = AnthropicStreamState::default();
    read_sse_events(response, |data| {
        let chunk: Value =
            serde_json::from_str(data).context("failed to parse anthropic stream chunk")?;
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
    let (system, anthropic_messages) = to_anthropic_messages(messages, &runtime.base_url);
    let body = json!({
        "model": runtime.model,
        "max_tokens": 4096,
        "system": system,
        "messages": anthropic_messages,
        "tools": build_anthropic_tools(tools),
        "stream": stream,
    });

    apply_anthropic_auth_headers(http.post(url), runtime)
        .json(&body)
        .send()
        .with_context(|| format!("failed POST {url}"))
        .and_then(|response| ensure_success_response(url, response))
}

fn apply_anthropic_auth_headers(
    request: RequestBuilder,
    runtime: &RuntimeProvider,
) -> RequestBuilder {
    let mut request = request.header("anthropic-version", ANTHROPIC_VERSION);
    for (name, value) in anthropic_auth_headers(&runtime.base_url, &runtime.api_key) {
        request = request.header(name, value);
    }
    request
}

fn anthropic_auth_headers(base_url: &str, api_key: &str) -> Vec<(&'static str, String)> {
    let mut headers = Vec::new();
    let betas = common_betas_for_base_url(base_url);

    if is_kimi_coding_endpoint(base_url) {
        headers.push(("x-api-key", api_key.to_string()));
        headers.push(("User-Agent", KIMI_CODING_USER_AGENT.to_string()));
        push_beta_header(&mut headers, &betas);
        return headers;
    }

    if requires_bearer_auth(base_url) {
        headers.push(("Authorization", format!("Bearer {api_key}")));
        push_beta_header(&mut headers, &betas);
        return headers;
    }

    if !is_third_party_anthropic_endpoint(base_url) && is_anthropic_oauth_token(api_key) {
        let mut oauth_betas = betas;
        oauth_betas.extend(OAUTH_ONLY_BETAS.iter().copied());
        headers.push(("Authorization", format!("Bearer {api_key}")));
        headers.push(("user-agent", CLAUDE_CODE_USER_AGENT.to_string()));
        headers.push(("x-app", "cli".to_string()));
        push_beta_header(&mut headers, &oauth_betas);
        return headers;
    }

    headers.push(("x-api-key", api_key.to_string()));
    push_beta_header(&mut headers, &betas);
    headers
}

fn push_beta_header(headers: &mut Vec<(&'static str, String)>, betas: &[&str]) {
    if !betas.is_empty() {
        headers.push(("anthropic-beta", betas.join(",")));
    }
}

fn common_betas_for_base_url(base_url: &str) -> Vec<&'static str> {
    if requires_bearer_auth(base_url) {
        COMMON_BETAS
            .iter()
            .copied()
            .filter(|beta| *beta != TOOL_STREAMING_BETA)
            .collect()
    } else {
        COMMON_BETAS.to_vec()
    }
}

fn is_anthropic_oauth_token(api_key: &str) -> bool {
    if api_key.is_empty() || api_key.starts_with("sk-ant-api") {
        return false;
    }
    api_key.starts_with("sk-ant-") || api_key.starts_with("eyJ") || api_key.starts_with("cc-")
}

#[derive(Default)]
struct AnthropicToolBuffer {
    call_id: String,
    name: String,
    raw_input: String,
}

#[derive(Default)]
struct AnthropicStreamState {
    text: String,
    reasoning: String,
    tools: BTreeMap<u64, AnthropicToolBuffer>,
}

impl AnthropicStreamState {
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
            "content_block_start" => {
                let index = chunk.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(content_block) = chunk.get("content_block") {
                    match content_block
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                    {
                        "tool_use" => {
                            let entry = self.tools.entry(index).or_default();
                            entry.call_id = content_block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            entry.name = content_block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            if let Some(input) = content_block.get("input") {
                                if matches!(input, Value::Object(map) if !map.is_empty()) {
                                    entry.raw_input = input.to_string();
                                    on_update(StreamUpdate::ToolCallDelta(entry.raw_input.clone()));
                                }
                            }
                        }
                        "thinking" => {
                            if let Some(text) =
                                content_block.get("thinking").and_then(Value::as_str)
                            {
                                self.reasoning.push_str(text);
                            }
                        }
                        "redacted_thinking" => {}
                        _ => {}
                    }
                }
            }
            "content_block_delta" => {
                let index = chunk.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(delta) = chunk.get("delta") {
                    match delta
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                    {
                        "text_delta" => {
                            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                                self.text.push_str(text);
                                on_update(StreamUpdate::TextDelta(text.to_string()));
                            }
                        }
                        "thinking_delta" => {
                            if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                                self.reasoning.push_str(text);
                            }
                        }
                        "input_json_delta" => {
                            let entry = self.tools.entry(index).or_default();
                            if let Some(partial_json) =
                                delta.get("partial_json").and_then(Value::as_str)
                            {
                                entry.raw_input.push_str(partial_json);
                                on_update(StreamUpdate::ToolCallDelta(entry.raw_input.clone()));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
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
        turn.tool_calls = self
            .tools
            .into_values()
            .map(|tool| {
                let input = if tool.raw_input.trim().is_empty() {
                    Value::Object(Map::new())
                } else {
                    serde_json::from_str(&tool.raw_input)
                        .unwrap_or_else(|_| Value::String(tool.raw_input.clone()))
                };
                Ok(AssistantToolCall {
                    call_id: tool.call_id,
                    name: tool.name,
                    input,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(turn)
    }
}

fn anthropic_messages_url(base_url: &str) -> String {
    if base_url.ends_with("/messages") {
        base_url.to_string()
    } else {
        format!("{}/messages", base_url.trim_end_matches('/'))
    }
}

fn build_anthropic_tools(tools: &[Tool]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                })
            })
            .collect(),
    )
}

fn to_anthropic_messages(messages: &[Message], base_url: &str) -> (Option<String>, Vec<Value>) {
    let mut system = None;
    let mut out = Vec::new();
    let strip_thinking_signatures =
        is_third_party_anthropic_endpoint(base_url) && !is_kimi_coding_endpoint(base_url);

    for message in messages {
        match message {
            Message::System(item) => {
                if system.is_none() {
                    system = Some(item.content.clone());
                }
            }
            Message::Developer(_) => {}
            Message::User(user) => {
                out.push(json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": user.content }],
                }));
            }
            Message::Assistant(assistant) => match &assistant.content {
                LanguageModelResponseContentType::Text(text) => {
                    out.push(json!({
                        "role": "assistant",
                        "content": [{ "type": "text", "text": text }],
                    }));
                }
                LanguageModelResponseContentType::Reasoning { content } => {
                    if !strip_thinking_signatures {
                        out.push(json!({
                            "role": "assistant",
                            "content": [{ "type": "thinking", "thinking": content }],
                        }));
                    }
                }
                LanguageModelResponseContentType::ToolCall(tool_call) => {
                    out.push(json!({
                        "role": "assistant",
                        "content": [{
                            "type": "tool_use",
                            "id": tool_call.tool.id,
                            "name": tool_call.tool.name,
                            "input": tool_call.input,
                        }],
                    }));
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
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_result.tool.id,
                        "content": content,
                    }],
                }));
            }
        }
    }

    (system, out)
}

fn is_third_party_anthropic_endpoint(base_url: &str) -> bool {
    let normalized = base_url.trim().trim_end_matches('/').to_ascii_lowercase();
    !normalized.is_empty() && !normalized.contains("anthropic.com")
}

fn is_kimi_coding_endpoint(base_url: &str) -> bool {
    base_url
        .trim()
        .trim_end_matches('/')
        .to_ascii_lowercase()
        .starts_with("https://api.kimi.com/coding")
}

fn requires_bearer_auth(base_url: &str) -> bool {
    let normalized = base_url.trim().trim_end_matches('/').to_ascii_lowercase();
    normalized.starts_with("https://api.minimax.io/anthropic")
        || normalized.starts_with("https://api.minimaxi.com/anthropic")
}

pub(crate) fn parse_response(payload: &Value) -> Result<AssistantTurn> {
    let content = payload
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("anthropic response missing content"))?;
    let mut turn = AssistantTurn::default();
    let mut texts = Vec::new();
    let mut reasoning_parts = Vec::new();

    for item in content {
        match item.get("type").and_then(Value::as_str).unwrap_or_default() {
            "text" => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    texts.push(text.to_string());
                }
            }
            "thinking" => {
                if let Some(text) = item.get("thinking").and_then(Value::as_str) {
                    reasoning_parts.push(text.to_string());
                }
            }
            "redacted_thinking" => {}
            "tool_use" => {
                let call_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("anthropic tool_use missing id"))?
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("anthropic tool_use missing name"))?
                    .to_string();
                let input = item.get("input").cloned().unwrap_or(Value::Null);
                turn.tool_calls.push(AssistantToolCall {
                    call_id,
                    name,
                    input,
                });
            }
            _ => {}
        }
    }

    if !texts.is_empty() {
        turn.text = Some(texts.join(""));
    }
    if !reasoning_parts.is_empty() {
        turn.reasoning = Some(reasoning_parts.join("\n\n"));
    }
    Ok(turn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_value(headers: &[(&'static str, String)], name: &str) -> Option<String> {
        headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }

    #[test]
    fn empty_content_end_turn_is_valid() -> Result<()> {
        let turn = parse_response(&json!({"content": [], "stop_reason": "end_turn"}))?;
        assert!(turn.text.is_none());
        assert!(turn.tool_calls.is_empty());
        Ok(())
    }

    #[test]
    fn third_party_endpoint_strips_thinking_replay() {
        let messages = vec![Message::Assistant(crate::model::AssistantMessage::new(
            LanguageModelResponseContentType::Reasoning {
                content: "secret thinking".to_string(),
            },
            None,
        ))];
        let (_, serialized) = to_anthropic_messages(&messages, "https://api.minimax.io/anthropic");
        assert!(serialized.is_empty());
    }

    #[test]
    fn regular_anthropic_api_key_uses_x_api_key() {
        let headers = anthropic_auth_headers("https://api.anthropic.com/v1", "sk-ant-api-example");

        assert_eq!(
            header_value(&headers, "x-api-key"),
            Some("sk-ant-api-example".to_string())
        );
        assert_eq!(header_value(&headers, "Authorization"), None);
    }

    #[test]
    fn direct_anthropic_oauth_token_uses_bearer_and_claude_identity() {
        let headers = anthropic_auth_headers("https://api.anthropic.com/v1", "sk-ant-oat-example");

        assert_eq!(
            header_value(&headers, "Authorization"),
            Some("Bearer sk-ant-oat-example".to_string())
        );
        assert_eq!(
            header_value(&headers, "user-agent"),
            Some(CLAUDE_CODE_USER_AGENT.to_string())
        );
        assert_eq!(header_value(&headers, "x-app"), Some("cli".to_string()));
        assert!(
            header_value(&headers, "anthropic-beta")
                .unwrap()
                .contains("oauth-2025-04-20")
        );
    }

    #[test]
    fn third_party_endpoint_does_not_treat_anthropic_shaped_key_as_oauth() {
        let headers = anthropic_auth_headers("https://example.com/anthropic", "sk-ant-oat-proxy");

        assert_eq!(
            header_value(&headers, "x-api-key"),
            Some("sk-ant-oat-proxy".to_string())
        );
        assert_eq!(header_value(&headers, "Authorization"), None);
    }

    #[test]
    fn minimax_anthropic_endpoint_uses_bearer_without_tool_streaming_beta() {
        let headers = anthropic_auth_headers("https://api.minimax.io/anthropic", "minimax-key");

        assert_eq!(
            header_value(&headers, "Authorization"),
            Some("Bearer minimax-key".to_string())
        );
        let beta = header_value(&headers, "anthropic-beta").unwrap();
        assert!(beta.contains("interleaved-thinking-2025-05-14"));
        assert!(!beta.contains(TOOL_STREAMING_BETA));
    }

    #[test]
    fn kimi_coding_endpoint_uses_claude_code_user_agent() {
        let headers = anthropic_auth_headers("https://api.kimi.com/coding/v1", "kimi-key");

        assert_eq!(
            header_value(&headers, "x-api-key"),
            Some("kimi-key".to_string())
        );
        assert_eq!(
            header_value(&headers, "User-Agent"),
            Some(KIMI_CODING_USER_AGENT.to_string())
        );
    }

    #[test]
    fn kimi_coding_preserves_unsigned_reasoning_replay() {
        let messages = vec![Message::Assistant(crate::model::AssistantMessage::new(
            LanguageModelResponseContentType::Reasoning {
                content: "server-side thinking".to_string(),
            },
            None,
        ))];
        let (_, serialized) = to_anthropic_messages(&messages, "https://api.kimi.com/coding/v1");
        assert_eq!(serialized.len(), 1);
        assert_eq!(serialized[0]["content"][0]["type"], "thinking");
    }
}
