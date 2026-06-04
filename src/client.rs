mod anthropic;
mod bedrock;
mod codex;
mod copilot_acp;
mod gemini;
mod gemini_cloudcode;
mod openai_compat;
mod sse;
mod types;

use crate::auth::resolve_provider_credentials;
use crate::model::{
    AssistantMessage, LanguageModelResponseContentType, Message, Messages, ModelToolError, Tool,
    ToolCallInfo, ToolResultInfo,
};
use crate::provider::{ApiMode, RuntimeProvider};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::path::Path;
use std::thread;
use std::time::Duration;

use types::AssistantTurn;
pub use types::{ModelResponse, StreamUpdate};

const MODEL_RETRY_COUNT: usize = 5;
const MODEL_RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct ModelClient {
    runtime: RuntimeProvider,
    http: Client,
}

impl ModelClient {
    pub fn from_runtime(runtime: RuntimeProvider) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self { runtime, http })
    }

    pub fn runtime(&self) -> &RuntimeProvider {
        &self.runtime
    }

    pub fn generate_title(&self, system_prompt: &str, user_input: &str) -> Result<String> {
        let messages = vec![
            Message::System(system_prompt.into()),
            Message::User(user_input.into()),
        ];
        let response = self.generate_with_tools_internal(
            messages,
            Vec::new(),
            false,
            None::<&mut dyn FnMut(StreamUpdate)>,
        )?;
        response
            .final_text
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| anyhow!("title generation returned empty response"))
    }

    pub fn analyze_image_file(&self, path: &Path, mime: &str, question: &str) -> Result<String> {
        let runtime = self.runtime_for_request(&self.runtime)?;
        if runtime.api_mode == ApiMode::CodexResponses {
            return codex::request_image_analysis(&self.http, &runtime, path, mime, question);
        }
        if runtime.api_mode != ApiMode::ChatCompletions {
            bail!(
                "vision_analyze is not supported for api_mode {:?}; supported modes are ChatCompletions and CodexResponses",
                runtime.api_mode
            );
        }
        let bytes = std::fs::read(path)
            .with_context(|| format!("failed to read image for vision: {}", path.display()))?;
        let encoded = BASE64_STANDARD.encode(bytes);
        let data_url = format!("data:{mime};base64,{encoded}");
        let url = chat_completions_url(&runtime.base_url);
        let body = json!({
            "model": runtime.model,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": question },
                    { "type": "image_url", "image_url": { "url": data_url } }
                ]
            }],
            "stream": false
        });
        let response: Value = self
            .http
            .post(&url)
            .bearer_auth(&runtime.api_key)
            .json(&body)
            .send()
            .with_context(|| format!("failed POST {url}"))?
            .error_for_status()
            .with_context(|| format!("vision request returned error status: {url}"))?
            .json()
            .context("failed to parse vision response JSON")?;
        response
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("vision response did not contain message content"))
    }

    pub fn generate_with_tools(
        &self,
        messages: Messages,
        tools: Vec<Tool>,
    ) -> Result<ModelResponse> {
        self.generate_with_tools_internal(
            messages,
            tools,
            false,
            None::<&mut dyn FnMut(StreamUpdate)>,
        )
    }

    pub fn generate_streaming_with_tools_single_step<F>(
        &self,
        messages: Messages,
        tools: Vec<Tool>,
        on_update: F,
    ) -> Result<ModelResponse>
    where
        F: FnMut(StreamUpdate),
    {
        let mut on_update = on_update;
        self.generate_with_tools_internal(messages, tools, true, Some(&mut on_update))
    }

    #[allow(dead_code)]
    pub fn generate_streaming_with_tools<F>(
        &self,
        messages: Messages,
        tools: Vec<Tool>,
        on_update: F,
    ) -> Result<ModelResponse>
    where
        F: FnMut(StreamUpdate),
    {
        let mut on_update = on_update;
        self.generate_with_tools_internal(messages, tools, false, Some(&mut on_update))
    }

    fn generate_with_tools_internal(
        &self,
        mut messages: Messages,
        tools: Vec<Tool>,
        stop_after_first_step: bool,
        mut on_update: Option<&mut dyn FnMut(StreamUpdate)>,
    ) -> Result<ModelResponse> {
        let mut steps = Vec::new();
        let mut tool_turn_texts = Vec::new();

        loop {
            let assistant = self.request_once_with_fallback(&messages, &tools, &mut on_update)?;
            let mut step = Vec::new();
            let mut reasoning_message_for_replay = None;

            if let Some(reasoning) = assistant.reasoning.clone() {
                let reasoning_message = Message::Assistant(AssistantMessage::new(
                    LanguageModelResponseContentType::Reasoning {
                        content: reasoning.clone(),
                    },
                    None,
                ));
                step.push(reasoning_message.clone());
                reasoning_message_for_replay = Some(reasoning_message);
            }

            if !assistant.tool_calls.is_empty() {
                let assistant_text = assistant
                    .text
                    .clone()
                    .filter(|text| !text.trim().is_empty());
                if let Some(text) = assistant_text.clone() {
                    emit_update(&mut on_update, StreamUpdate::ToolTurnText(text.clone()));
                    tool_turn_texts.push(text);
                }
                if let Some(reasoning_message) = reasoning_message_for_replay.take() {
                    messages.push(reasoning_message);
                }

                let tool_calls = assistant.tool_calls;
                let mut tool_call_infos = Vec::with_capacity(tool_calls.len());
                for tool_call in &tool_calls {
                    let mut tool_call_info = ToolCallInfo::new(tool_call.name.clone());
                    tool_call_info.id(tool_call.call_id.clone());
                    tool_call_info.input(tool_call.input.clone());
                    if let Some(text) = assistant_text.clone() {
                        tool_call_info.content(text);
                    }
                    tool_call_infos.push(tool_call_info);
                }

                // One provider response with tool calls must be replayed as one
                // assistant turn. DeepSeek thinking mode requires the same
                // assistant item to contain content + reasoning_content + all
                // tool_calls on later requests.
                for tool_call_info in &tool_call_infos {
                    step.push(Message::Assistant(AssistantMessage::new(
                        LanguageModelResponseContentType::ToolCall(tool_call_info.clone()),
                        None,
                    )));
                    messages.push(Message::Assistant(AssistantMessage::new(
                        LanguageModelResponseContentType::ToolCall(tool_call_info.clone()),
                        None,
                    )));
                }

                for tool_call in tool_calls {
                    let tool_output = execute_tool_call_or_error(&tool_call, &tools);

                    let mut tool_result = ToolResultInfo::new(tool_call.name.clone());
                    tool_result.id(tool_call.call_id.clone());
                    match &tool_output {
                        Ok(value) => tool_result.output(value.clone()),
                        Err(err) => tool_result.output = Err(err.clone()),
                    }
                    step.push(Message::Tool(tool_result.clone()));
                    messages.push(Message::Tool(tool_result));
                }

                steps.push(step);
                if stop_after_first_step {
                    return Ok(ModelResponse {
                        steps,
                        tool_turn_texts,
                        final_text: None,
                    });
                }
                continue;
            }

            if let Some(text) = assistant.text.clone() {
                return Ok(ModelResponse {
                    steps: if step.is_empty() {
                        steps
                    } else {
                        let mut steps_with_final = steps;
                        steps_with_final.push(step);
                        steps_with_final
                    },
                    tool_turn_texts,
                    final_text: Some(text),
                });
            }

            if stop_after_first_step {
                return Ok(ModelResponse {
                    steps,
                    tool_turn_texts,
                    final_text: None,
                });
            }

            bail!("model returned neither text nor tool calls");
        }
    }

    fn request_once_with_fallback(
        &self,
        messages: &[Message],
        tools: &[Tool],
        on_update: &mut Option<&mut dyn FnMut(StreamUpdate)>,
    ) -> Result<AssistantTurn> {
        let candidates = crate::model_config::request_candidate_runtimes(&self.runtime)
            .unwrap_or_else(|_| vec![self.runtime.clone()]);
        let mut failures = Vec::new();

        for (model_index, candidate) in candidates.iter().enumerate() {
            if model_index > 0 {
                emit_status(
                    on_update,
                    format!("Trying fallback model {}.", runtime_label(candidate)),
                );
            }
            for attempt in 0..=MODEL_RETRY_COUNT {
                let mut emitted_visible_stream = false;
                let result = if let Some(callback) = on_update.as_mut() {
                    let mut tracking_callback = |update: StreamUpdate| {
                        if matches!(
                            update,
                            StreamUpdate::TextDelta(_) | StreamUpdate::ToolCallDelta(_)
                        ) {
                            emitted_visible_stream = true;
                        }
                        callback(update);
                    };
                    self.request_once_streaming_with_runtime(
                        candidate,
                        messages,
                        tools,
                        &mut tracking_callback,
                    )
                } else {
                    self.request_once_with_runtime(candidate, messages, tools)
                };

                match result {
                    Ok(turn) => return Ok(turn),
                    Err(error) => {
                        let label = runtime_label(candidate);
                        let error_text = format!("{error:#}");
                        if emitted_visible_stream {
                            return Err(error).with_context(|| {
                                format!("{label} failed after streaming had already started")
                            });
                        }
                        if is_non_retryable_model_error(&error) {
                            failures.push(format!("{label}: {error_text}"));
                            break;
                        }
                        if attempt < MODEL_RETRY_COUNT {
                            emit_status(
                                on_update,
                                format!(
                                    "Retrying model {label} ({}/{MODEL_RETRY_COUNT})...",
                                    attempt + 1
                                ),
                            );
                            thread::sleep(MODEL_RETRY_DELAY);
                        } else {
                            failures.push(format!("{label}: {error_text}"));
                        }
                    }
                }
            }
        }

        emit_status(
            on_update,
            "Model request failed after trying all configured models.".to_string(),
        );
        bail!(
            "model request failed after trying all configured models:\n{}",
            failures.join("\n")
        )
    }

    fn request_once_with_runtime(
        &self,
        runtime: &RuntimeProvider,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<AssistantTurn> {
        let runtime = self.runtime_for_request(runtime)?;
        match runtime.api_mode {
            ApiMode::ChatCompletions => {
                openai_compat::request(&self.http, &runtime, messages, tools)
            }
            ApiMode::AnthropicMessages => anthropic::request(&self.http, &runtime, messages, tools),
            ApiMode::CodexResponses => codex::request(&self.http, &runtime, messages, tools),
            ApiMode::GeminiNative => gemini::request(&self.http, &runtime, messages, tools),
            ApiMode::GeminiCloudcode => {
                gemini_cloudcode::request(&self.http, &runtime, messages, tools)
            }
            ApiMode::BedrockConverse => bedrock::request(&runtime.model, messages, tools),
            ApiMode::CopilotAcp => copilot_acp::request(messages, tools),
        }
    }

    fn request_once_streaming_with_runtime(
        &self,
        runtime: &RuntimeProvider,
        messages: &[Message],
        tools: &[Tool],
        on_update: &mut dyn FnMut(StreamUpdate),
    ) -> Result<AssistantTurn> {
        let runtime = self.runtime_for_request(runtime)?;
        match runtime.api_mode {
            ApiMode::ChatCompletions => {
                openai_compat::request_streaming(&self.http, &runtime, messages, tools, on_update)
            }
            ApiMode::AnthropicMessages => {
                anthropic::request_streaming(&self.http, &runtime, messages, tools, on_update)
            }
            ApiMode::CodexResponses => {
                codex::request_streaming(&self.http, &runtime, messages, tools, on_update)
            }
            ApiMode::GeminiNative => {
                gemini::request_streaming(&self.http, &runtime, messages, tools, on_update)
            }
            ApiMode::GeminiCloudcode => gemini_cloudcode::request_streaming(
                &self.http, &runtime, messages, tools, on_update,
            ),
            ApiMode::BedrockConverse => {
                let turn = bedrock::request(&runtime.model, messages, tools)?;
                if let Some(text) = &turn.text {
                    on_update(StreamUpdate::TextDelta(text.clone()));
                }
                Ok(turn)
            }
            ApiMode::CopilotAcp => {
                let turn = copilot_acp::request(messages, tools)?;
                if let Some(text) = &turn.text {
                    on_update(StreamUpdate::TextDelta(text.clone()));
                }
                Ok(turn)
            }
        }
    }

    fn runtime_for_request(&self, runtime: &RuntimeProvider) -> Result<RuntimeProvider> {
        let mut runtime = runtime.clone();
        match resolve_provider_credentials(runtime.provider, true) {
            Ok(Some(credentials)) => {
                runtime.api_key = credentials.as_api_key();
                if let Some(base_url) = credentials.base_url {
                    runtime.base_url = base_url;
                }
                runtime.account_id = credentials.project_id;
            }
            Ok(None) => {}
            Err(error)
                if !runtime.api_key.trim().is_empty()
                    && error.to_string().contains("is not logged in") => {}
            Err(error) => return Err(error),
        }
        Ok(runtime)
    }
}

fn emit_status(callback: &mut Option<&mut dyn FnMut(StreamUpdate)>, status: String) {
    emit_update(callback, StreamUpdate::Status(status));
}

fn emit_update(callback: &mut Option<&mut dyn FnMut(StreamUpdate)>, update: StreamUpdate) {
    if let Some(callback) = callback.as_mut() {
        callback(update);
    }
}

fn runtime_label(runtime: &RuntimeProvider) -> String {
    format!("{}/{}", runtime.provider.as_str(), runtime.model)
}

fn is_non_retryable_model_error(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    if text.contains("http status 401")
        || text.contains("http status 403")
        || text.contains("unauthorized")
        || text.contains("forbidden")
        || text.contains("permission denied")
        || text.contains("invalid api key")
        || text.contains("invalid key")
        || text.contains("api key is invalid")
        || text.contains("not logged in")
        || text.contains("model not found")
        || text.contains("unknown model")
        || text.contains("does not exist")
        || text.contains("quota")
        || text.contains("insufficient_quota")
        || text.contains("billing")
        || text.contains("balance")
        || text.contains("credit")
        || text.contains("context length")
        || text.contains("maximum context")
        || text.contains("too many tokens")
    {
        return true;
    }

    text.contains("http status 404")
        || (text.contains("http status 429")
            && (text.contains("quota")
                || text.contains("billing")
                || text.contains("balance")
                || text.contains("credit")))
}

fn execute_tool_call_or_error(
    tool_call: &crate::client::types::AssistantToolCall,
    tools: &[Tool],
) -> std::result::Result<Value, ModelToolError> {
    if let Some(tool) = tools
        .iter()
        .find(|candidate| candidate.name == tool_call.name)
    {
        return (tool.execute)(tool_call.input.clone())
            .map(Value::String)
            .map_err(ModelToolError);
    }

    let available_tools = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(ModelToolError(format!(
        "tool not found: {}. Available native tools: {}",
        tool_call.name,
        if available_tools.is_empty() {
            "(none)"
        } else {
            &available_tools
        }
    )))
}

fn chat_completions_url(base_url: &str) -> String {
    if base_url.ends_with("/chat/completions") {
        base_url.to_string()
    } else {
        format!("{}/chat/completions", base_url.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::types::AssistantToolCall;
    use crate::model::{Tool, ToolExecute};
    use crate::provider::{ProviderKind, RuntimeProvider};
    use serde_json::json;

    #[test]
    fn client_accepts_chat_runtime() {
        let client = ModelClient::from_runtime(RuntimeProvider {
            model_id: None,
            provider: ProviderKind::OpenAi,
            model: "model".to_string(),
            base_url: "https://example.com/v1".to_string(),
            api_key: "key".to_string(),
            api_mode: ApiMode::ChatCompletions,
            source: "test".to_string(),
            account_id: None,
        });
        assert!(client.is_ok());
    }

    #[test]
    fn unknown_tool_call_returns_tool_error_instead_of_failing_turn() {
        let tool_call = AssistantToolCall {
            call_id: "call_1".to_string(),
            name: "process_start".to_string(),
            input: json!({"command": "ls"}),
        };
        let available = vec![
            Tool::builder()
                .name("call_capability")
                .description("call capability")
                .input_schema(schemars::schema_for!(String))
                .execute(ToolExecute::new(Box::new(|_| Ok("ok".to_string()))))
                .build()
                .expect("tool should build"),
        ];

        let err = execute_tool_call_or_error(&tool_call, &available)
            .expect_err("unknown tool should be returned as tool error");
        assert!(err.0.contains("tool not found: process_start"));
        assert!(err.0.contains("call_capability"));
    }

    #[test]
    fn model_error_classification_skips_retry_for_auth_and_quota() {
        assert!(is_non_retryable_model_error(&anyhow!(
            "provider returned error: HTTP status 401 (Unauthorized)"
        )));
        assert!(is_non_retryable_model_error(&anyhow!(
            "HTTP status 429 body: insufficient_quota"
        )));
        assert!(!is_non_retryable_model_error(&anyhow!(
            "provider returned error: HTTP status 503 (Service Unavailable)"
        )));
    }
}
