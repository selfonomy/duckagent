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

const CHANNEL: &str = "tlon";
const DEFAULT_SEND_ENDPOINT: &str = "/send";
const TEXT_LIMIT: usize = 8_000;

#[derive(Clone)]
pub(in crate::gateway) struct TlonAdapter {
    bridge_base: Option<String>,
    token: Option<String>,
    webhook_secret: Option<String>,
    default_ship: Option<String>,
    default_channel: Option<String>,
    allowed_conversations: HashSet<String>,
    allowed_senders: HashSet<String>,
    max_download_bytes: u64,
    send_endpoint: String,
    client: Client,
    seen_message_ids: Arc<Mutex<VecDeque<String>>>,
}

impl TlonAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .context("failed to build Tlon bridge HTTP client")?;
        Ok(Self {
            bridge_base: config.api_base.clone(),
            token: credentials.token.clone().or(credentials.api_key.clone()),
            webhook_secret: credentials
                .webhook_secret
                .clone()
                .or_else(|| credentials.signing_secret.clone()),
            default_ship: config
                .extra
                .get("ship")
                .cloned()
                .or_else(|| credentials.extra.get("ship").cloned()),
            default_channel: config
                .extra
                .get("default_channel")
                .cloned()
                .or_else(|| credentials.extra.get("default_channel").cloned()),
            allowed_conversations: config
                .allowed_chats
                .iter()
                .map(|value| normalize_id(value))
                .collect(),
            allowed_senders: config
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

    fn handle_tlon_event(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        if !self.verify_webhook(&request) {
            return Ok(json_response(401, json!({"error": "unauthorized"})));
        }
        let value: Value =
            serde_json::from_slice(&request.body).context("failed to parse Tlon bridge JSON")?;
        let events = value["events"]
            .as_array()
            .or_else(|| value["messages"].as_array())
            .cloned()
            .unwrap_or_else(|| vec![value]);
        for event in events {
            let event = normalize_tlon_event(event);
            if let Some(input) = self.event_to_inbound(&event)? {
                inbound.submit(input)?;
            }
        }
        Ok(json_response(200, json!({"ok": true})))
    }

    fn event_to_inbound(&self, event: &Value) -> Result<Option<InboundMessageInput>> {
        let body = tlon_event_body(event);
        let conversation_id = tlon_conversation_id(body, event)
            .or_else(|| self.default_channel.clone())
            .ok_or_else(|| anyhow!("Tlon event missing ship/channel/resource conversation id"))?;
        if !allowlist_matches(&self.allowed_conversations, &conversation_id) {
            return Ok(None);
        }

        let sender_id = first_tlon_str(
            body,
            event,
            &[
                "sender_id",
                "ship",
                "sender.ship",
                "author.ship",
                "author",
                "author_ship",
                "from",
                "user_id",
            ],
        );
        if let Some(sender_id) = sender_id.as_deref() {
            if !allowlist_matches(&self.allowed_senders, sender_id) {
                return Ok(None);
            }
        }

        let message_id = first_tlon_str(
            body,
            event,
            &[
                "message_id",
                "event_id",
                "id",
                "post_id",
                "serial",
                "index",
                "time",
            ],
        );
        if let Some(message_id) = message_id.as_deref() {
            if self.is_duplicate(message_id) {
                return Ok(None);
            }
        }

        let text = tlon_text(body, event);
        let mut attachments = self.parse_attachments(body, event);
        for url in tlon_media_urls(&text) {
            attachments.push(InboundAttachmentInput {
                bytes: None,
                path: Some(url),
                filename: None,
                mime: None,
            });
        }
        if text.trim().is_empty() && attachments.is_empty() {
            return Ok(None);
        }

        Ok(Some(InboundMessageInput {
            channel: CHANNEL.to_string(),
            conversation_id,
            thread_id: first_tlon_str(
                body,
                event,
                &[
                    "thread_id",
                    "reply_to",
                    "parent_id",
                    "parent",
                    "root_id",
                    "parent.id",
                ],
            ),
            chat_type: Some(
                first_tlon_str(body, event, &["chat_type", "conversation_type", "type"])
                    .unwrap_or_else(|| "tlon".to_string()),
            ),
            sender_id,
            message_id,
            text: if text.trim().is_empty() {
                "[Tlon attachment]".to_string()
            } else {
                text
            },
            attachments,
            timestamp: first_tlon_str(body, event, &["timestamp", "created_at", "time"]),
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
                    Err(error) => eprintln!("Tlon attachment skipped: {error:#}"),
                }
            }
        }
        for value in [
            body.get("attachment"),
            body.get("file"),
            body.get("content"),
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
                    Err(error) => eprintln!("Tlon attachment skipped: {error:#}"),
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
        let response = request.send().context("Tlon attachment download failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("Tlon attachment download failed with status {status}");
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
            .context("Tlon attachment body unreadable")?;
        if self.max_download_bytes > 0 && bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "Tlon attachment exceeds max_download_bytes ({})",
                self.max_download_bytes
            );
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: first_str(attachment, &["filename", "name", "file_name"])
                .map(str::to_string)
                .or_else(|| Some("tlon-attachment.bin".to_string())),
            mime,
        })
    }

    fn verify_webhook(&self, request: &ChannelHttpRequest) -> bool {
        let Some(secret) = self.webhook_secret.as_deref() else {
            return true;
        };
        let candidate = request
            .header("x-duckagent-gateway-secret")
            .or_else(|| request.header("x-tlon-secret"))
            .or_else(|| request.header("x-urbit-secret"))
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
            .expect("tlon seen message ids mutex poisoned");
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
            .ok_or_else(|| anyhow!("tlon channel requires bridge API URL"))?;
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
        let response = request.send().context("Tlon bridge POST failed")?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            bail!("Tlon bridge POST failed with status {status}: {text}");
        }
        Ok(())
    }
}

impl ChannelAdapter for TlonAdapter {
    fn start(&self, _inbound: GatewayInboundDispatch) -> Result<()> {
        Ok(())
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        if request.method == "POST"
            && matches!(request.path.as_str(), "/tlon/events" | "/tlon/webhook")
        {
            return self.handle_tlon_event(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let conversation_id = route.key.conversation_id.as_str();
        let thread_id = route.key.thread_id.as_deref();
        let reply_to = message.reply_to.as_deref();
        for chunk in text_chunks(&message.text) {
            self.post_bridge(
                &self.send_endpoint,
                json!({
                    "channel": CHANNEL,
                    "platform": "tlon",
                    "conversation_id": conversation_id,
                    "resource": conversation_id,
                    "ship": self.default_ship.as_deref(),
                    "default_channel": self.default_channel.as_deref(),
                    "thread_id": thread_id,
                    "reply_to": reply_to,
                    "text": chunk,
                    "content": chunk,
                    "media_paths": [],
                }),
            )?;
        }
        if !message.media_paths.is_empty() {
            self.post_bridge(
                &self.send_endpoint,
                json!({
                    "channel": CHANNEL,
                    "platform": "tlon",
                    "conversation_id": conversation_id,
                    "resource": conversation_id,
                    "ship": self.default_ship.as_deref(),
                    "default_channel": self.default_channel.as_deref(),
                    "thread_id": thread_id,
                    "reply_to": reply_to,
                    "text": "",
                    "content": "",
                    "media_paths": &message.media_paths,
                    "media_mode": "tlon_upload_or_link",
                }),
            )?;
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
        self.post_bridge(
            &self.send_endpoint,
            json!({
                "channel": CHANNEL,
                "platform": "tlon",
                "conversation_id": route.key.conversation_id.as_str(),
                "resource": route.key.conversation_id.as_str(),
                "ship": self.default_ship.as_deref(),
                "default_channel": self.default_channel.as_deref(),
                "thread_id": route.key.thread_id.as_deref(),
                "text": approval_text,
                "content": approval_text,
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
) -> Result<TlonAdapter> {
    TlonAdapter::new(config, credentials)
}

fn normalize_tlon_event(event: Value) -> Value {
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

fn tlon_event_body(event: &Value) -> &Value {
    event
        .get("event")
        .or_else(|| event.get("message"))
        .or_else(|| event.get("data"))
        .or_else(|| event.get("update"))
        .or_else(|| event.get("graph-update"))
        .or_else(|| event.get("payload"))
        .filter(|value| value.is_object())
        .unwrap_or(event)
}

fn tlon_conversation_id(body: &Value, event: &Value) -> Option<String> {
    first_tlon_str(
        body,
        event,
        &[
            "conversation_id",
            "resource",
            "resource.path",
            "graph.resource",
            "graph",
            "channel",
            "channel_id",
            "room_id",
            "chat_id",
        ],
    )
    .or_else(|| {
        let ship = first_tlon_str(body, event, &["ship", "host_ship", "desk_ship"])?;
        let channel = first_tlon_str(body, event, &["name", "graph_name", "channel_name"])?;
        Some(format!("{ship}:{channel}"))
    })
}

fn tlon_text(body: &Value, event: &Value) -> String {
    if let Some(text) = first_tlon_str(body, event, &["text", "message", "body", "content"]) {
        return text;
    }
    let mut parts = Vec::new();
    tlon_collect_text(&body["content"], &mut parts);
    for item in body["contents"].as_array().into_iter().flatten() {
        tlon_collect_text(item, &mut parts);
    }
    for node in body["nodes"]
        .as_object()
        .into_iter()
        .flat_map(|nodes| nodes.values())
    {
        tlon_collect_text(node, &mut parts);
    }
    parts.join("\n")
}

fn tlon_collect_text(value: &Value, parts: &mut Vec<String>) {
    if let Some(text) = first_str(value, &["text", "body", "content", "plain"]) {
        if !text.trim().is_empty() {
            parts.push(text.to_string());
        }
    }
    for key in ["inline", "blocks", "contents", "children", "items", "story"] {
        match &value[key] {
            Value::Array(items) => {
                for item in items {
                    tlon_collect_text(item, parts);
                }
            }
            Value::Object(_) => tlon_collect_text(&value[key], parts),
            _ => {}
        }
    }
}

fn tlon_media_urls(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|token| {
            let trimmed = token.trim_matches(|ch: char| {
                matches!(
                    ch,
                    '"' | '\'' | '(' | ')' | '[' | ']' | '<' | '>' | ',' | '.'
                )
            });
            let lower = trimmed
                .split(['?', '#'])
                .next()
                .unwrap_or(trimmed)
                .to_ascii_lowercase();
            let is_media = matches!(
                lower.rsplit('.').next(),
                Some(
                    "jpg"
                        | "jpeg"
                        | "png"
                        | "gif"
                        | "webp"
                        | "mp4"
                        | "webm"
                        | "mov"
                        | "mp3"
                        | "m4a"
                        | "ogg"
                        | "wav"
                        | "pdf"
                )
            );
            (trimmed.starts_with("https://") && is_media).then(|| trimmed.to_string())
        })
        .collect()
}

fn first_tlon_str(body: &Value, event: &Value, keys: &[&str]) -> Option<String> {
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
    allowlist.is_empty() || allowlist.contains("*") || allowlist.contains(&normalize_id(value))
}

fn normalize_id(value: &str) -> String {
    let mut value = value.trim().to_ascii_lowercase();
    for prefix in [
        "tlon:",
        "urbit:",
        "graph:",
        "resource:",
        "web+urbitgraph://",
    ] {
        if let Some(stripped) = value.strip_prefix(prefix) {
            value = stripped.to_string();
        }
    }
    value.trim_start_matches('/').to_string()
}

fn endpoint_path(endpoint: &str) -> String {
    if endpoint.starts_with('/') {
        endpoint.to_string()
    } else {
        format!("/{endpoint}")
    }
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
    fn tlon_conversation_can_be_composed_from_ship_and_graph() {
        let value = json!({"ship": "~zod", "graph_name": "ops", "text": "hi"});
        assert_eq!(
            tlon_conversation_id(&value, &value),
            Some("~zod:ops".to_string())
        );
    }

    #[test]
    fn tlon_contents_array_joins_text_segments() {
        let value = json!({"contents": [{"text": "hello"}, {"text": "world"}]});
        assert_eq!(tlon_text(&value, &value), "hello\nworld");
    }
}
