use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, InboundMessageInput,
    OutboundMessage, TypingEvent,
};
use super::websocket::{
    ChannelWebSocket, is_transient_read_error, read_json_message, send_json_message,
    set_read_timeout,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::connect;

const HOMEASSISTANT_TEXT_LIMIT: usize = 8_000;
const HOMEASSISTANT_RECONNECT_BACKOFFS: [u64; 4] = [5, 10, 30, 60];

#[derive(Clone)]
pub(in crate::gateway) struct HomeAssistantAdapter {
    api_base: String,
    token: String,
    notify_service: String,
    webhook_secret: Option<String>,
    allowed_sources: HashSet<String>,
    watch_domains: HashSet<String>,
    watch_entities: HashSet<String>,
    ignore_entities: HashSet<String>,
    watch_all: bool,
    cooldown_seconds: u64,
    insecure_webhook: bool,
    command_webhook_enabled: bool,
    client: Client,
    last_event_times: Arc<Mutex<HashMap<String, Instant>>>,
    seen_event_ids: Arc<Mutex<VecDeque<String>>>,
}

impl HomeAssistantAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let api_base = config
            .api_base
            .clone()
            .ok_or_else(|| anyhow!("homeassistant gateway config requires api_base"))?;
        let token = credentials
            .token
            .as_deref()
            .or(credentials.api_key.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("homeassistant gateway credentials require access token"))?
            .to_string();
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build Home Assistant HTTP client")?;
        Ok(Self {
            api_base,
            token,
            notify_service: config
                .extra
                .get("notify_service")
                .cloned()
                .unwrap_or_else(|| "notify.notify".to_string()),
            webhook_secret: credentials.webhook_secret.clone(),
            allowed_sources: config.allowed_chats.iter().cloned().collect(),
            watch_domains: parse_csv_extra(config.extra.get("watch_domains")),
            watch_entities: parse_csv_extra(config.extra.get("watch_entities")),
            ignore_entities: parse_csv_extra(config.extra.get("ignore_entities")),
            watch_all: config
                .extra
                .get("watch_all")
                .is_some_and(|value| matches!(value.as_str(), "true" | "1" | "yes")),
            cooldown_seconds: config
                .extra
                .get("cooldown_seconds")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(30),
            insecure_webhook: config
                .extra
                .get("insecure_webhook")
                .is_some_and(|value| matches!(value.as_str(), "true" | "1" | "yes")),
            command_webhook_enabled: config
                .extra
                .get("command_webhook_enabled")
                .is_some_and(|value| matches!(value.as_str(), "true" | "1" | "yes"))
                || credentials.webhook_secret.is_some()
                || config
                    .extra
                    .get("insecure_webhook")
                    .is_some_and(|value| matches!(value.as_str(), "true" | "1" | "yes")),
            client,
            last_event_times: Arc::new(Mutex::new(HashMap::new())),
            seen_event_ids: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    fn websocket_loop(self, inbound: GatewayInboundDispatch) {
        let mut backoff_index = 0usize;
        loop {
            match self.websocket_run_once(&inbound) {
                Ok(()) => backoff_index = 0,
                Err(error) => {
                    eprintln!("Home Assistant websocket disconnected: {error:#}");
                    let delay = HOMEASSISTANT_RECONNECT_BACKOFFS
                        [backoff_index.min(HOMEASSISTANT_RECONNECT_BACKOFFS.len() - 1)];
                    backoff_index =
                        (backoff_index + 1).min(HOMEASSISTANT_RECONNECT_BACKOFFS.len() - 1);
                    thread::sleep(Duration::from_secs(delay));
                }
            }
        }
    }

    fn websocket_run_once(&self, inbound: &GatewayInboundDispatch) -> Result<()> {
        let ws_url = self.websocket_url();
        let (mut socket, _) = connect(ws_url.as_str())
            .with_context(|| format!("Home Assistant websocket connect failed: {ws_url}"))?;
        set_read_timeout(&mut socket, Duration::from_secs(60));

        let auth_required =
            read_required_ws_json(&mut socket, "Home Assistant auth_required read failed")?;
        if auth_required["type"].as_str() != Some("auth_required") {
            bail!("Home Assistant websocket expected auth_required: {auth_required}");
        }

        send_json_message(
            &mut socket,
            &json!({"type": "auth", "access_token": self.token.clone()}),
            "Home Assistant auth send failed",
        )?;
        let auth_response =
            read_required_ws_json(&mut socket, "Home Assistant auth response read failed")?;
        if auth_response["type"].as_str() != Some("auth_ok") {
            bail!("Home Assistant websocket auth failed: {auth_response}");
        }

        send_json_message(
            &mut socket,
            &json!({"id": 1, "type": "subscribe_events", "event_type": "state_changed"}),
            "Home Assistant subscribe send failed",
        )?;
        let subscribe_response =
            read_required_ws_json(&mut socket, "Home Assistant subscribe response read failed")?;
        if !subscribe_response["success"].as_bool().unwrap_or(false) {
            bail!("Home Assistant state_changed subscription failed: {subscribe_response}");
        }

        loop {
            match read_json_message(
                &mut socket,
                "Home Assistant websocket read failed",
                "Home Assistant websocket JSON invalid",
            ) {
                Ok(Some(value)) => self.handle_ws_message(&value, inbound)?,
                Ok(None) => {}
                Err(error) if is_transient_read_error(&error) => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn handle_ws_message(&self, value: &Value, inbound: &GatewayInboundDispatch) -> Result<()> {
        if value["type"].as_str() != Some("event") {
            return Ok(());
        }
        let event = value.get("event").unwrap_or(value);
        if let Some(input) = self.state_changed_to_inbound(event)? {
            inbound.submit(input)?;
        }
        Ok(())
    }

    fn websocket_url(&self) -> String {
        let base = self.api_base.trim_end_matches('/');
        let ws_base = if base.starts_with("https://") {
            base.replacen("https://", "wss://", 1)
        } else if base.starts_with("http://") {
            base.replacen("http://", "ws://", 1)
        } else if base.starts_with("ws://") || base.starts_with("wss://") {
            base.to_string()
        } else {
            format!("ws://{base}")
        };
        format!("{}/api/websocket", ws_base.trim_end_matches('/'))
    }

    fn handle_event(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        if !self.verify_webhook(&request) {
            return Ok(json_response(401, json!({"error": "unauthorized"})));
        }
        let value: Value = serde_json::from_slice(&request.body)
            .context("failed to parse Home Assistant event JSON")?;
        if let Some(input) = self.state_changed_to_inbound(&value)? {
            inbound.submit(input)?;
            return Ok(json_response(200, json!({"ok": true})));
        }
        let body = homeassistant_event_body(&value);
        let source = first_str(
            body,
            &[
                "conversation_id",
                "entity_id",
                "source",
                "user_id",
                "context.user_id",
            ],
        )
        .unwrap_or("homeassistant");
        if !self.allowed_sources.is_empty() && !self.allowed_sources.contains(source) {
            return Ok(json_response(200, json!({"ok": true, "ignored": true})));
        }
        let event_id = first_str(body, &["event_id", "id", "context.id"]);
        if event_id.is_some_and(|event_id| self.is_duplicate(event_id)) {
            return Ok(json_response(200, json!({"ok": true, "ignored": true})));
        }
        let text = first_str(body, &["text", "message", "command", "sentence"])
            .unwrap_or_default()
            .to_string();
        if text.trim().is_empty() {
            return Ok(json_response(200, json!({"ok": true, "ignored": true})));
        }
        inbound.submit(InboundMessageInput {
            channel: "homeassistant".to_string(),
            conversation_id: source.to_string(),
            thread_id: first_str(body, &["thread_id", "context_id", "context.id"])
                .map(str::to_string),
            chat_type: Some("automation".to_string()),
            sender_id: first_str(body, &["user_id", "source", "context.user_id"])
                .map(str::to_string),
            message_id: event_id.map(str::to_string),
            text,
            attachments: Vec::new(),
            timestamp: first_str(body, &["timestamp", "time_fired"]).map(str::to_string),
        })?;
        Ok(json_response(200, json!({"ok": true})))
    }

    fn state_changed_to_inbound(&self, value: &Value) -> Result<Option<InboundMessageInput>> {
        let event = if value.get("event_type").is_some() && value.get("data").is_some() {
            value
        } else {
            homeassistant_event_body(value)
        };
        let event_type = first_str(value, &["event_type", "type"])
            .or_else(|| first_str(event, &["event_type", "type"]))
            .unwrap_or_default();
        let data = event.get("data").unwrap_or(event);
        let entity_id = first_str(data, &["entity_id"]).unwrap_or_default();
        if event_type != "state_changed" && entity_id.is_empty() {
            return Ok(None);
        }
        if entity_id.is_empty() || self.ignore_entities.contains(entity_id) {
            return Ok(None);
        }
        if !self.entity_is_watched(entity_id) {
            return Ok(None);
        }
        if self.cooldown_active(entity_id) {
            return Ok(None);
        }
        let event_id = first_str(event, &["event_id", "id", "context.id"])
            .or_else(|| first_str(data, &["event_id", "id", "context.id"]));
        if event_id.is_some_and(|event_id| self.is_duplicate(event_id)) {
            return Ok(None);
        }
        let old_state = data.get("old_state").unwrap_or(&Value::Null);
        let new_state = data.get("new_state").unwrap_or(data);
        let text = format_state_change(entity_id, old_state, new_state)
            .or_else(|| first_str(data, &["message", "text"]).map(str::to_string))
            .unwrap_or_default();
        if text.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(InboundMessageInput {
            channel: "homeassistant".to_string(),
            conversation_id: entity_id.to_string(),
            thread_id: first_str(event, &["context.id"])
                .or_else(|| first_str(data, &["context.id"]))
                .map(str::to_string),
            chat_type: Some("automation".to_string()),
            sender_id: first_str(event, &["context.user_id"])
                .or_else(|| first_str(data, &["context.user_id"]))
                .or_else(|| Some("homeassistant"))
                .map(str::to_string),
            message_id: event_id.map(str::to_string),
            text,
            attachments: Vec::new(),
            timestamp: first_str(event, &["time_fired", "timestamp"])
                .or_else(|| first_str(data, &["time_fired", "timestamp"]))
                .map(str::to_string),
        }))
    }

    fn entity_is_watched(&self, entity_id: &str) -> bool {
        if !self.allowed_sources.is_empty() {
            return self.allowed_sources.contains(entity_id);
        }
        if self.watch_all || self.watch_entities.contains(entity_id) {
            return true;
        }
        let domain = entity_id
            .split_once('.')
            .map(|(domain, _)| domain)
            .unwrap_or(entity_id);
        !self.watch_domains.is_empty() && self.watch_domains.contains(domain)
    }

    fn cooldown_active(&self, entity_id: &str) -> bool {
        if self.cooldown_seconds == 0 {
            return false;
        }
        let now = Instant::now();
        let mut last = self
            .last_event_times
            .lock()
            .expect("homeassistant cooldown mutex poisoned");
        if let Some(previous) = last.get(entity_id) {
            if now.duration_since(*previous).as_secs() < self.cooldown_seconds {
                return true;
            }
        }
        last.insert(entity_id.to_string(), now);
        false
    }

    fn is_duplicate(&self, event_id: &str) -> bool {
        if event_id.trim().is_empty() {
            return false;
        }
        let mut seen = self
            .seen_event_ids
            .lock()
            .expect("homeassistant seen event ids mutex poisoned");
        if seen.iter().any(|existing| existing == event_id) {
            return true;
        }
        seen.push_back(event_id.to_string());
        while seen.len() > 1000 {
            seen.pop_front();
        }
        false
    }

    fn verify_webhook(&self, request: &ChannelHttpRequest) -> bool {
        let Some(secret) = self.webhook_secret.as_deref() else {
            return self.insecure_webhook;
        };
        request
            .header("x-duckagent-gateway-secret")
            .or_else(|| request.header("x-homeassistant-secret"))
            .or_else(|| request.query.get("secret").map(String::as_str))
            .is_some_and(|value| constant_time_eq(value.as_bytes(), secret.as_bytes()))
    }

    fn send_notify(&self, text: &str) -> Result<()> {
        let (domain, service) = self.notify_service.split_once('.').ok_or_else(|| {
            anyhow!("homeassistant notify_service must look like notify.mobile_app")
        })?;
        let response = self
            .client
            .post(format!(
                "{}/api/services/{}/{}",
                self.api_base.trim_end_matches('/'),
                domain,
                service
            ))
            .bearer_auth(&self.token)
            .json(&json!({"message": text, "title": "DuckAgent"}))
            .send()
            .context("Home Assistant notify send failed")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            bail!("Home Assistant notify send failed with status {status}: {body}");
        }
        Ok(())
    }
}

impl ChannelAdapter for HomeAssistantAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        let adapter = self.clone();
        thread::Builder::new()
            .name("homeassistant-websocket".to_string())
            .spawn(move || adapter.websocket_loop(inbound))
            .context("failed to spawn Home Assistant websocket thread")?;
        Ok(())
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        if self.command_webhook_enabled
            && request.method == "POST"
            && matches!(
                request.path.as_str(),
                "/homeassistant/events" | "/homeassistant/webhook"
            )
        {
            return self.handle_event(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, _route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let mut text = message.text;
        for media in message.media_paths {
            if media.starts_with("http://") || media.starts_with("https://") {
                text.push('\n');
                text.push_str(&media);
            } else {
                bail!(
                    "Home Assistant local MEDIA requires a public URL or notify attachment bridge: {media}"
                );
            }
        }
        for chunk in text_chunks(&text) {
            self.send_notify(&chunk)?;
        }
        Ok(())
    }

    fn send_typing(&self, _route: &GatewayRoute, _event: TypingEvent) -> Result<()> {
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        let approval_id = prompt.id.clone();
        let approval_message = prompt.message.clone();
        self.send_message(
            route,
            OutboundMessage {
                text: format!(
                    "{}\n\nCommands:\n/approve {} once\n/approve {} session\n/approve {} always\n/deny {}",
                    approval_message,
                    approval_id.as_str(),
                    approval_id.as_str(),
                    approval_id.as_str(),
                    approval_id.as_str()
                ),
                media_paths: Vec::new(),
                reply_to: None,
                approval_prompt: Some(prompt),
                typing_event: None,
            },
        )
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: false,
            typing: false,
            approval_prompt: true,
        }
    }
}

fn read_required_ws_json(socket: &mut ChannelWebSocket, context: &'static str) -> Result<Value> {
    read_json_message(socket, context, "Home Assistant websocket JSON invalid")?
        .ok_or_else(|| anyhow!("Home Assistant websocket produced no JSON message"))
}

fn first_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        value[*key].as_str().or_else(|| {
            let pointer = format!("/{}", key.replace('.', "/"));
            value.pointer(&pointer).and_then(Value::as_str)
        })
    })
}

fn homeassistant_event_body(value: &Value) -> &Value {
    value
        .get("event")
        .or_else(|| value.get("payload"))
        .or_else(|| value.get("data"))
        .filter(|value| value.is_object())
        .unwrap_or(value)
}

fn parse_csv_extra(value: Option<&String>) -> HashSet<String> {
    value
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn format_state_change(entity_id: &str, old_state: &Value, new_state: &Value) -> Option<String> {
    let old_value = old_state["state"].as_str().unwrap_or("unknown");
    let new_value = new_state["state"].as_str().unwrap_or("unknown");
    if old_value == new_value {
        return None;
    }
    let friendly = new_state
        .pointer("/attributes/friendly_name")
        .and_then(Value::as_str)
        .unwrap_or(entity_id);
    let domain = entity_id
        .split_once('.')
        .map(|(domain, _)| domain)
        .unwrap_or("");
    let text = match domain {
        "climate" => {
            let current = new_state
                .pointer("/attributes/current_temperature")
                .and_then(|value| {
                    value
                        .as_f64()
                        .map(|value| value.to_string())
                        .or_else(|| value.as_str().map(str::to_string))
                })
                .unwrap_or_else(|| "?".to_string());
            let target = new_state
                .pointer("/attributes/temperature")
                .and_then(|value| {
                    value
                        .as_f64()
                        .map(|value| value.to_string())
                        .or_else(|| value.as_str().map(str::to_string))
                })
                .unwrap_or_else(|| "?".to_string());
            format!(
                "[Home Assistant] {friendly}: HVAC mode changed from '{old_value}' to '{new_value}' (current: {current}, target: {target})"
            )
        }
        "sensor" => {
            let unit = new_state
                .pointer("/attributes/unit_of_measurement")
                .and_then(Value::as_str)
                .unwrap_or_default();
            format!(
                "[Home Assistant] {friendly}: changed from {old_value}{unit} to {new_value}{unit}"
            )
        }
        "binary_sensor" => format!(
            "[Home Assistant] {friendly}: {} (was {})",
            if new_value == "on" {
                "triggered"
            } else {
                "cleared"
            },
            if old_value == "on" {
                "triggered"
            } else {
                "cleared"
            }
        ),
        "light" | "switch" | "fan" => {
            format!(
                "[Home Assistant] {friendly}: turned {}",
                if new_value == "on" { "on" } else { "off" }
            )
        }
        "alarm_control_panel" => {
            format!(
                "[Home Assistant] {friendly}: alarm state changed from '{old_value}' to '{new_value}'"
            )
        }
        _ => format!(
            "[Home Assistant] {friendly} ({entity_id}): changed from '{old_value}' to '{new_value}'"
        ),
    };
    Some(text)
}

fn text_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if current.len() + character.len_utf8() > HOMEASSISTANT_TEXT_LIMIT && !current.is_empty() {
            chunks.push(current);
            current = String::new();
        }
        current.push(character);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0u8, |acc, (left, right)| acc | (left ^ right))
        == 0
}

fn json_response(status: u16, value: Value) -> ChannelHttpResponse {
    ChannelHttpResponse {
        status,
        content_type: "application/json",
        body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homeassistant_chunks_long_text() {
        assert_eq!(
            text_chunks(&"x".repeat(HOMEASSISTANT_TEXT_LIMIT + 1)).len(),
            2
        );
    }

    #[test]
    fn homeassistant_constant_time_compare() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"other"));
    }
}
