use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, InboundAttachmentInput,
    InboundMessageInput, OutboundMessage, TypingEvent,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CHANNEL: &str = "synology-chat";
const DEFAULT_TRANSPORT: &str = "synology_chat_webhook";
const DEFAULT_SEND_ENDPOINT: &str = "/send";
const TEXT_LIMIT: usize = 4_000;

#[derive(Clone)]
pub(in crate::gateway) struct SynologyChatAdapter {
    transport: String,
    incoming_webhook_url: Option<String>,
    bridge_base: Option<String>,
    token: Option<String>,
    webhook_secret: Option<String>,
    bot_username: Option<String>,
    allowed_channels: HashSet<String>,
    allowed_users: HashSet<String>,
    max_download_bytes: u64,
    send_endpoint: String,
    client: Client,
    seen_message_ids: Arc<Mutex<VecDeque<String>>>,
}

impl SynologyChatAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .context("failed to build Synology Chat HTTP client")?;
        let transport = config
            .transport
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_TRANSPORT)
            .to_string();
        let uses_bridge = is_synology_bridge_transport(&transport);
        Ok(Self {
            transport,
            incoming_webhook_url: config
                .extra
                .get("incoming_webhook_url")
                .cloned()
                .or_else(|| config.extra.get("webhook_url").cloned())
                .or_else(|| credentials.extra.get("incoming_webhook_url").cloned())
                .or_else(|| credentials.extra.get("webhook_url").cloned())
                .or_else(|| {
                    if uses_bridge {
                        None
                    } else {
                        config.api_base.clone()
                    }
                }),
            bridge_base: if uses_bridge {
                config.api_base.clone()
            } else {
                None
            },
            token: credentials.token.clone().or(credentials.api_key.clone()),
            webhook_secret: credentials
                .webhook_secret
                .clone()
                .or_else(|| credentials.signing_secret.clone()),
            bot_username: config
                .extra
                .get("bot_username")
                .cloned()
                .or_else(|| credentials.extra.get("bot_username").cloned()),
            allowed_channels: config
                .allowed_chats
                .iter()
                .map(|value| normalize_id(value))
                .collect(),
            allowed_users: config
                .allowed_users
                .iter()
                .map(|value| normalize_id(value))
                .collect(),
            max_download_bytes: config.media.max_download_bytes,
            send_endpoint: config
                .extra
                .get("send_endpoint")
                .cloned()
                .unwrap_or_else(|| DEFAULT_SEND_ENDPOINT.to_string()),
            client,
            seen_message_ids: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    fn uses_bridge_transport(&self) -> bool {
        is_synology_bridge_transport(&self.transport)
    }

    fn handle_synology_event(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        if !self.verify_webhook(&request) {
            return Ok(json_response(401, json!({"error": "unauthorized"})));
        }
        let value = parse_synology_request(&request)?;
        let events = value["events"]
            .as_array()
            .or_else(|| value["messages"].as_array())
            .cloned()
            .unwrap_or_else(|| vec![value]);
        for event in events {
            let event = normalize_synology_event(event);
            if let Some(input) = self.event_to_inbound(&event)? {
                inbound.submit(input)?;
            }
        }
        Ok(json_response(200, json!({"ok": true})))
    }

    fn event_to_inbound(&self, event: &Value) -> Result<Option<InboundMessageInput>> {
        let body = synology_event_body(event);
        let conversation_id = first_synology_str(
            body,
            event,
            &[
                "conversation_id",
                "channel_id",
                "channel",
                "room_id",
                "chat_id",
                "to",
            ],
        )
        .or_else(|| {
            first_synology_str(body, event, &["channel_name", "room_name"])
                .map(|value| format!("channel:{value}"))
        })
        .ok_or_else(|| anyhow!("Synology Chat event missing channel/conversation id"))?;
        if !allowlist_matches(&self.allowed_channels, &conversation_id) {
            return Ok(None);
        }

        let sender_id = first_synology_str(
            body,
            event,
            &[
                "sender_id",
                "user_id",
                "account",
                "username",
                "from",
                "author_id",
            ],
        );
        if let Some(sender_id) = sender_id.as_deref() {
            if self
                .bot_username
                .as_deref()
                .is_some_and(|bot| normalize_id(bot) == normalize_id(sender_id))
            {
                return Ok(None);
            }
            if !allowlist_matches(&self.allowed_users, sender_id) {
                return Ok(None);
            }
        }

        let message_id =
            first_synology_str(body, event, &["message_id", "event_id", "id", "post_id"]);
        if let Some(message_id) = message_id.as_deref() {
            if self.is_duplicate(message_id) {
                return Ok(None);
            }
        }

        let mut text = first_synology_str(body, event, &["text", "message", "body", "content"])
            .unwrap_or_default();
        if text.trim().is_empty() {
            text = first_synology_str(body, event, &["attachments_text", "file_text"])
                .unwrap_or_default();
        }
        let attachments = self.parse_attachments(body, event);
        if text.trim().is_empty() && attachments.is_empty() {
            return Ok(None);
        }

        Ok(Some(InboundMessageInput {
            channel: CHANNEL.to_string(),
            conversation_id,
            thread_id: first_synology_str(
                body,
                event,
                &["thread_id", "parent_id", "reply_to", "root_id"],
            ),
            chat_type: Some(
                first_synology_str(body, event, &["chat_type", "conversation_type", "type"])
                    .unwrap_or_else(|| "channel".to_string()),
            ),
            sender_id,
            message_id,
            text: if text.trim().is_empty() {
                "[Synology Chat attachment]".to_string()
            } else {
                text
            },
            attachments,
            timestamp: first_synology_str(body, event, &["timestamp", "created_at", "time"]),
        }))
    }

    fn parse_attachments(&self, body: &Value, event: &Value) -> Vec<InboundAttachmentInput> {
        let mut out = Vec::new();
        for attachment in body["attachments"]
            .as_array()
            .or_else(|| body["media"].as_array())
            .or_else(|| body["files"].as_array())
            .or_else(|| event["attachments"].as_array())
            .or_else(|| event["media"].as_array())
            .or_else(|| event["files"].as_array())
            .into_iter()
            .flatten()
        {
            if let Some(input) = attachment_from_value(attachment) {
                out.push(input);
                continue;
            }
            if let Some(url) = first_str(attachment, &["url", "download_url", "media_url"]) {
                match self.download_attachment(url, attachment) {
                    Ok(input) => out.push(input),
                    Err(error) => eprintln!("Synology Chat attachment skipped: {error:#}"),
                }
            }
        }
        for value in [
            body.get("attachment"),
            body.get("file"),
            event.get("attachment"),
            event.get("file"),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(input) = attachment_from_value(value) {
                out.push(input);
                continue;
            }
            if let Some(url) = first_str(value, &["url", "download_url", "media_url"]) {
                match self.download_attachment(url, value) {
                    Ok(input) => out.push(input),
                    Err(error) => eprintln!("Synology Chat attachment skipped: {error:#}"),
                }
            }
        }
        out
    }

    fn download_attachment(&self, url: &str, attachment: &Value) -> Result<InboundAttachmentInput> {
        let mut request = self.client.get(url);
        if let Some(token) = self.token.as_deref() {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .context("Synology Chat attachment download failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("Synology Chat attachment download failed with status {status}");
        }
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or(value).to_string())
            .or_else(|| {
                first_str(attachment, &["mime", "mime_type", "content_type"]).map(str::to_string)
            });
        let bytes = response
            .bytes()
            .context("Synology Chat attachment body unreadable")?;
        if self.max_download_bytes > 0 && bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "Synology Chat attachment exceeds max_download_bytes ({})",
                self.max_download_bytes
            );
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: first_str(attachment, &["filename", "name", "file_name"])
                .map(str::to_string)
                .or_else(|| Some("synology-chat-attachment.bin".to_string())),
            mime,
        })
    }

    fn verify_webhook(&self, request: &ChannelHttpRequest) -> bool {
        let Some(secret) = self.webhook_secret.as_deref() else {
            return true;
        };
        let candidate = request
            .header("x-duckagent-gateway-secret")
            .or_else(|| request.header("x-synology-chat-secret"))
            .or_else(|| request.header("x-synology-secret"))
            .or_else(|| request.query.get("secret").map(String::as_str));
        candidate.is_some_and(|value| constant_time_eq(value.as_bytes(), secret.as_bytes()))
    }

    fn is_duplicate(&self, message_id: &str) -> bool {
        if message_id.trim().is_empty() {
            return false;
        }
        let mut seen = self
            .seen_message_ids
            .lock()
            .expect("synology chat seen message ids mutex poisoned");
        if seen.iter().any(|existing| existing == message_id) {
            return true;
        }
        seen.push_back(message_id.to_string());
        while seen.len() > 1000 {
            seen.pop_front();
        }
        false
    }

    fn post_bridge(&self, endpoint: &str, body: Value) -> Result<()> {
        let bridge_base = self
            .bridge_base
            .as_deref()
            .ok_or_else(|| anyhow!("synology-chat channel requires bridge API URL"))?;
        let mut request = self
            .client
            .post(format!(
                "{}{}",
                bridge_base.trim_end_matches('/'),
                endpoint_path(endpoint)
            ))
            .json(&body);
        if let Some(token) = self.token.as_deref() {
            request = request.bearer_auth(token);
        }
        let response = request.send().context("Synology Chat bridge POST failed")?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            bail!("Synology Chat bridge POST failed with status {status}: {text}");
        }
        Ok(())
    }

    fn post_incoming_webhook(&self, body: Value) -> Result<()> {
        let webhook_url = self
            .incoming_webhook_url
            .as_deref()
            .ok_or_else(|| anyhow!("synology-chat channel requires incoming webhook URL"))?;
        let payload =
            serde_json::to_string(&body).context("failed to serialize Synology Chat payload")?;
        let response = self
            .client
            .post(webhook_url)
            .form(&[("payload", payload)])
            .send()
            .context("Synology Chat incoming webhook POST failed")?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            bail!("Synology Chat incoming webhook POST failed with status {status}: {text}");
        }
        Ok(())
    }
}

impl ChannelAdapter for SynologyChatAdapter {
    fn start(&self, _inbound: GatewayInboundDispatch) -> Result<()> {
        Ok(())
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        if request.method == "POST"
            && matches!(
                request.path.as_str(),
                "/synology-chat/events" | "/synology-chat/webhook"
            )
        {
            return self.handle_synology_event(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let conversation_id = route.key.conversation_id.as_str();
        let thread_id = route.key.thread_id.as_deref();
        let reply_to = message.reply_to.as_deref();
        for chunk in text_chunks(&message.text) {
            if self.uses_bridge_transport() {
                self.post_bridge(
                    &self.send_endpoint,
                    json!({
                        "channel": CHANNEL,
                        "platform": "synology_chat",
                        "conversation_id": conversation_id,
                        "channel_id": conversation_id,
                        "thread_id": thread_id,
                        "reply_to": reply_to,
                        "username": self.bot_username.as_deref(),
                        "text": chunk,
                        "message": chunk,
                        "media_paths": [],
                    }),
                )?;
            } else {
                self.post_incoming_webhook(json!({ "text": chunk }))?;
            }
        }
        if !message.media_paths.is_empty() {
            if self.uses_bridge_transport() {
                self.post_bridge(
                    &self.send_endpoint,
                    json!({
                        "channel": CHANNEL,
                        "platform": "synology_chat",
                        "conversation_id": conversation_id,
                        "channel_id": conversation_id,
                        "thread_id": thread_id,
                        "reply_to": reply_to,
                        "username": self.bot_username.as_deref(),
                        "text": "",
                        "message": "",
                        "media_paths": &message.media_paths,
                        "media_mode": "synology_chat_upload_or_link",
                    }),
                )?;
            } else {
                let media_text = format!(
                    "Generated media files:\n{}",
                    message
                        .media_paths
                        .iter()
                        .map(|path| format!("- {path}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                self.post_incoming_webhook(json!({ "text": media_text }))?;
            }
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
        let approval_text = format!(
            "{}\n\nCommands:\n/approve {} once\n/approve {} session\n/approve {} always\n/deny {}",
            prompt.message,
            approval_id.as_str(),
            approval_id.as_str(),
            approval_id.as_str(),
            approval_id.as_str()
        );
        if self.uses_bridge_transport() {
            self.post_bridge(
                &self.send_endpoint,
                json!({
                    "channel": CHANNEL,
                    "platform": "synology_chat",
                    "conversation_id": route.key.conversation_id.as_str(),
                    "channel_id": route.key.conversation_id.as_str(),
                    "thread_id": route.key.thread_id.as_deref(),
                    "username": self.bot_username.as_deref(),
                    "text": approval_text,
                    "message": approval_text,
                    "approval": {
                        "id": approval_id.as_str(),
                        "commands": [
                            format!("/approve {} once", approval_id.as_str()),
                            format!("/approve {} session", approval_id.as_str()),
                            format!("/approve {} always", approval_id.as_str()),
                            format!("/deny {}", approval_id.as_str())
                        ]
                    }
                }),
            )
        } else {
            self.post_incoming_webhook(json!({ "text": approval_text }))
        }
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: true,
            typing: false,
            approval_prompt: true,
        }
    }
}

pub(in crate::gateway::channels) fn new_adapter(
    config: &GatewayChannelConfig,
    credentials: &GatewayCredentialEntry,
) -> Result<SynologyChatAdapter> {
    SynologyChatAdapter::new(config, credentials)
}

fn parse_synology_request(request: &ChannelHttpRequest) -> Result<Value> {
    if let Ok(value) = serde_json::from_slice(&request.body) {
        return Ok(value);
    }
    let body = std::str::from_utf8(&request.body).context("Synology Chat request is not UTF-8")?;
    let mut form = serde_json::Map::new();
    for pair in body.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = percent_decode(parts.next().unwrap_or_default());
        let value = percent_decode(parts.next().unwrap_or_default());
        if key == "payload" {
            return serde_json::from_str(&value)
                .context("failed to parse Synology Chat form payload JSON");
        }
        if !key.is_empty() {
            form.insert(key, Value::String(value));
        }
    }
    if form.is_empty() {
        bail!("failed to parse Synology Chat request as JSON or form payload")
    }
    Ok(Value::Object(form))
}

fn normalize_synology_event(event: Value) -> Value {
    let Some(payload) = event.get("payload").and_then(Value::as_str) else {
        return event;
    };
    let Ok(parsed_payload) = serde_json::from_str::<Value>(payload) else {
        return event;
    };
    let (mut outer, mut inner) = match (event, parsed_payload) {
        (Value::Object(outer), Value::Object(inner)) => (outer, inner),
        (_, parsed_payload) => return parsed_payload,
    };
    outer.remove("payload");
    for (key, value) in outer {
        inner.entry(key).or_insert(value);
    }
    Value::Object(inner)
}

fn synology_event_body(event: &Value) -> &Value {
    event
        .get("event")
        .or_else(|| event.get("message"))
        .or_else(|| event.get("data"))
        .or_else(|| event.get("payload"))
        .filter(|value| value.is_object())
        .unwrap_or(event)
}

fn first_synology_str(body: &Value, event: &Value, keys: &[&str]) -> Option<String> {
    first_str(body, keys)
        .or_else(|| first_str(event, keys))
        .map(str::to_string)
}

fn attachment_from_value(value: &Value) -> Option<InboundAttachmentInput> {
    if let Some(path) = first_str(value, &["path", "file_path", "local_path"]) {
        return Some(InboundAttachmentInput {
            bytes: None,
            path: Some(path.to_string()),
            filename: first_str(value, &["filename", "name", "file_name"]).map(str::to_string),
            mime: first_str(value, &["mime", "mime_type", "content_type"]).map(str::to_string),
        });
    }
    if let Some(bytes) = first_str(value, &["bytes_base64", "base64"]) {
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(bytes) {
            return Some(InboundAttachmentInput {
                bytes: Some(decoded),
                path: None,
                filename: first_str(value, &["filename", "name", "file_name"]).map(str::to_string),
                mime: first_str(value, &["mime", "mime_type", "content_type"]).map(str::to_string),
            });
        }
    }
    None
}

fn first_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        value[*key].as_str().or_else(|| {
            let pointer = format!("/{}", key.replace('.', "/"));
            value.pointer(&pointer).and_then(Value::as_str)
        })
    })
}

fn allowlist_matches(allowlist: &HashSet<String>, value: &str) -> bool {
    allowlist.is_empty()
        || allowlist.contains("*")
        || allowlist.contains(&normalize_id(value))
        || value
            .strip_prefix("channel:")
            .is_some_and(|stripped| allowlist.contains(&normalize_id(stripped)))
}

fn normalize_id(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn endpoint_path(endpoint: &str) -> String {
    if endpoint.starts_with('/') {
        endpoint.to_string()
    } else {
        format!("/{endpoint}")
    }
}

fn is_synology_bridge_transport(transport: &str) -> bool {
    matches!(
        transport,
        "synology_chat_bridge" | "bridge" | "webhook_bridge" | "http_bridge"
    )
}

fn text_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if current.len() + character.len_utf8() > TEXT_LIMIT && !current.is_empty() {
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

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                if let (Some(left), Some(right)) =
                    (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
                {
                    out.push((left << 4) | right);
                    index += 3;
                } else {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
            value => {
                out.push(value);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
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
    fn synology_form_payload_is_decoded() -> Result<()> {
        let request = ChannelHttpRequest {
            method: "POST".to_string(),
            path: "/synology-chat/events".to_string(),
            query: Default::default(),
            headers: Default::default(),
            body: br#"payload=%7B%22channel_id%22%3A%22c1%22%2C%22text%22%3A%22hi%22%7D"#.to_vec(),
        };
        let value = parse_synology_request(&request)?;
        assert_eq!(value["channel_id"].as_str(), Some("c1"));
        assert_eq!(value["text"].as_str(), Some("hi"));
        Ok(())
    }

    #[test]
    fn synology_channel_name_fallback_is_namespaced() -> Result<()> {
        let adapter = SynologyChatAdapter::new(
            &GatewayChannelConfig::default(),
            &GatewayCredentialEntry {
                channel: CHANNEL.to_string(),
                ..Default::default()
            },
        )?;
        let input = adapter
            .event_to_inbound(&json!({"channel_name": "ops", "username": "alice", "text": "hi"}))?
            .expect("event should parse");
        assert_eq!(input.conversation_id, "channel:ops");
        assert_eq!(input.sender_id.as_deref(), Some("alice"));
        Ok(())
    }
}
