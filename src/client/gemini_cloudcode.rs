use crate::client::gemini::{GeminiStreamState, build_gemini_body, parse_response};
use crate::client::sse::{ensure_success_response, read_sse_events};
use crate::client::types::{AssistantTurn, StreamUpdate};
use crate::model::{Message, Tool};
use crate::provider::{ProviderKind, RuntimeProvider};
use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::env;

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
    let project_id = runtime
        .account_id
        .clone()
        .or_else(|| {
            crate::auth::resolve_provider_credentials(ProviderKind::GoogleGeminiCli, false)
                .ok()
                .flatten()
                .and_then(|credentials| credentials.project_id)
        })
        .or_else(google_project_id_from_env)
        .unwrap_or_default();
    let inner = build_gemini_body(messages, tools);
    let body = json!({
        "project": project_id,
        "model": runtime.model,
        "user_prompt_id": uuid::Uuid::now_v7().to_string(),
        "request": inner,
    });
    let url = if stream {
        format!(
            "{}/v1internal:streamGenerateContent?alt=sse",
            runtime.base_url.trim_end_matches('/')
        )
    } else {
        format!(
            "{}/v1internal:generateContent",
            runtime.base_url.trim_end_matches('/')
        )
    };
    let response = http
        .post(&url)
        .bearer_auth(&runtime.api_key)
        .json(&body)
        .send()
        .with_context(|| format!("failed POST {url}"))
        .and_then(|response| ensure_success_response(&url, response))?;
    if stream {
        let mut state = GeminiStreamState::default();
        let mut on_update = on_update;
        read_sse_events(response, |data| {
            let payload: Value =
                serde_json::from_str(data).context("failed to parse Code Assist SSE chunk")?;
            let chunk = payload.get("response").unwrap_or(&payload);
            let turn = parse_response(chunk)?;
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
        let payload: Value = response
            .json()
            .context("failed to parse Code Assist response")?;
        parse_response(payload.get("response").unwrap_or(&payload))
    }
}

fn google_project_id_from_env() -> Option<String> {
    [
        "HERMES_GEMINI_PROJECT_ID",
        "GOOGLE_CLOUD_PROJECT",
        "GOOGLE_CLOUD_PROJECT_ID",
    ]
    .into_iter()
    .find_map(|key| env::var(key).ok().filter(|value| !value.trim().is_empty()))
}
