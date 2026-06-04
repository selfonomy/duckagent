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

const ZALO_CHANNEL: &str = "zalo";
const ZALOUSER_CHANNEL: &str = "zalouser";
const DEFAULT_SEND_ENDPOINT: &str = "/send";
const DEFAULT_TYPING_ENDPOINT: &str = "/typing";
const TEXT_LIMIT: usize = 2_000;

#[derive(Clone, Copy)]
enum ZaloMode {
    OfficialAccount,
    UserSession,
}

#[derive(Clone)]
pub(in crate::gateway) struct ZaloAdapter {
    mode: ZaloMode,
    channel: &'static str,
    platform: &'static str,
    bridge_base: Option<String>,
    token: Option<String>,
    webhook_secret: Option<String>,
    app_id: Option<String>,
    account_id: Option<String>,
    allowed_chats: HashSet<String>,
    allowed_users: HashSet<String>,
    max_download_bytes: u64,
    send_endpoint: String,
    typing_endpoint: Option<String>,
    client: Client,
    seen_message_ids: Arc<Mutex<VecDeque<String>>>,
}

impl ZaloAdapter {
    fn new_for_channel(
        mode: ZaloMode,
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .context("failed to build Zalo bridge HTTP client")?;
        let (channel, platform, account_key) = match mode {
            ZaloMode::OfficialAccount => (ZALO_CHANNEL, "zalo_oa", "oa_id"),
            ZaloMode::UserSession => (ZALOUSER_CHANNEL, "zalo_user", "session_id"),
        };
        Ok(Self {
            mode,
            channel,
            platform,
            bridge_base: config.api_base.clone(),
            token: credentials.token.clone().or(credentials.api_key.clone()),
            webhook_secret: credentials
                .webhook_secret
                .clone()
                .or_else(|| credentials.signing_secret.clone()),
            app_id: config
                .extra
                .get("app_id")
                .cloned()
                .or_else(|| credentials.app_id.clone())
                .or_else(|| credentials.extra.get("app_id").cloned()),
            account_id: config
                .extra
                .get(account_key)
                .cloned()
                .or_else(|| credentials.extra.get(account_key).cloned()),
            allowed_chats: config
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
            typing_endpoint: config
                .extra
                .get("typing_endpoint")
                .cloned()
                .or_else(|| Some(DEFAULT_TYPING_ENDPOINT.to_string())),
            client,
            seen_message_ids: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    fn handle_zalo_event(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        if !self.verify_webhook(&request) {
            return Ok(json_response(401, json!({"error": "unauthorized"})));
        }
        let value = parse_zalo_request(&request)?;
        let events = value["events"]
            .as_array()
            .or_else(|| value["messages"].as_array())
            .cloned()
            .unwrap_or_else(|| vec![value]);
        for event in events {
            let event = normalize_zalo_event(event);
            if let Some(input) = self.event_to_inbound(&event)? {
                inbound.submit(input)?;
            }
        }
        Ok(json_response(200, json!({"ok": true})))
    }

    fn event_to_inbound(&self, event: &Value) -> Result<Option<InboundMessageInput>> {
        let body = zalo_event_body(event);
        let message = body.get("message").filter(|value| value.is_object());
        let conversation_id = first_zalo_str(
            body,
            event,
            &[
                "conversation_id",
                "chat_id",
                "group_id",
                "group.id",
                "thread_id",
                "recipient_id",
                "recipient.id",
                "receiver.id",
                "to",
                "message.chat_id",
                "message.group_id",
                "message.thread_id",
            ],
        )
        .or_else(|| {
            message
                .and_then(|value| {
                    first_str(
                        value,
                        &["conversation_id", "chat_id", "group_id", "thread_id"],
                    )
                })
                .map(str::to_string)
        })
        .or_else(|| nested_str(body, "sender", "id").map(str::to_string))
        .or_else(|| nested_str(event, "sender", "id").map(str::to_string))
        .ok_or_else(|| anyhow!("{} event missing chat/user/group id", self.channel))?;
        if !allowlist_matches(&self.allowed_chats, &conversation_id) {
            return Ok(None);
        }

        let sender_id = first_zalo_str(
            body,
            event,
            &[
                "sender_id",
                "sender.id",
                "user_id",
                "user.id",
                "from",
                "from.id",
                "author_id",
                "uid",
            ],
        )
        .or_else(|| nested_str(body, "sender", "id").map(str::to_string))
        .or_else(|| nested_str(event, "sender", "id").map(str::to_string));
        if let Some(sender_id) = sender_id.as_deref() {
            if self
                .account_id
                .as_deref()
                .is_some_and(|account_id| normalize_id(account_id) == normalize_id(sender_id))
                || self
                    .app_id
                    .as_deref()
                    .is_some_and(|app_id| normalize_id(app_id) == normalize_id(sender_id))
            {
                return Ok(None);
            }
            if !allowlist_matches(&self.allowed_users, sender_id) {
                return Ok(None);
            }
        }

        let message_id = first_zalo_str(body, event, &["message_id", "event_id", "id", "msg_id"])
            .or_else(|| {
                message
                    .and_then(|value| first_str(value, &["msg_id", "message_id", "id"]))
                    .map(str::to_string)
            });
        if let Some(message_id) = message_id.as_deref() {
            if self.is_duplicate(message_id) {
                return Ok(None);
            }
        }

        let text = zalo_text(body, event);
        let mut attachments = self.parse_attachments(body, event, message);
        for url in zalo_media_urls(&text) {
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
            channel: self.channel.to_string(),
            conversation_id: conversation_id.clone(),
            thread_id: first_zalo_str(
                body,
                event,
                &[
                    "thread_id",
                    "reply_to",
                    "parent_id",
                    "message.quote_msg_id",
                    "message.quote_message_id",
                ],
            ),
            chat_type: Some(zalo_chat_type(body, &conversation_id, self.mode)),
            sender_id,
            message_id,
            text: if text.trim().is_empty() {
                format!("[{} attachment]", self.channel)
            } else {
                text
            },
            attachments,
            timestamp: first_zalo_str(
                body,
                event,
                &["timestamp", "created_at", "time", "message.timestamp"],
            ),
        }))
    }

    fn parse_attachments(
        &self,
        body: &Value,
        event: &Value,
        message: Option<&Value>,
    ) -> Vec<InboundAttachmentInput> {
        let mut out = Vec::new();
        for source in [Some(body), Some(event), message].into_iter().flatten() {
            for attachment in source["attachments"]
                .as_array()
                .or_else(|| source["media"].as_array())
                .or_else(|| source["files"].as_array())
                .into_iter()
                .flatten()
            {
                if let Some(input) = attachment_from_value(attachment) {
                    out.push(input);
                    continue;
                }
                if let Some(url) =
                    first_str(attachment, &["url", "download_url", "media_url", "href"])
                {
                    match self.download_attachment(url, attachment) {
                        Ok(input) => out.push(input),
                        Err(error) => eprintln!("{} attachment skipped: {error:#}", self.channel),
                    }
                }
            }
        }
        for value in [
            body.get("attachment"),
            body.get("file"),
            body.get("image"),
            body.get("audio"),
            body.get("video"),
            body.get("document"),
            event.get("attachment"),
            event.get("file"),
            message.and_then(|message| message.get("attachment")),
            message.and_then(|message| message.get("file")),
            message.and_then(|message| message.get("image")),
            message.and_then(|message| message.get("audio")),
            message.and_then(|message| message.get("video")),
            message.and_then(|message| message.get("document")),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(input) = attachment_from_value(value) {
                out.push(input);
                continue;
            }
            if let Some(url) =
                first_str(value, &["url", "download_url", "media_url", "href", "src"])
            {
                match self.download_attachment(url, value) {
                    Ok(input) => out.push(input),
                    Err(error) => eprintln!("{} attachment skipped: {error:#}", self.channel),
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
        let response = request.send().context("Zalo attachment download failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("Zalo attachment download failed with status {status}");
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
            .context("Zalo attachment body unreadable")?;
        if self.max_download_bytes > 0 && bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "{} attachment exceeds max_download_bytes ({})",
                self.channel,
                self.max_download_bytes
            );
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: first_str(attachment, &["filename", "name", "file_name"])
                .map(str::to_string)
                .or_else(|| Some(format!("{}-attachment.bin", self.channel))),
            mime,
        })
    }

    fn verify_webhook(&self, request: &ChannelHttpRequest) -> bool {
        let Some(secret) = self.webhook_secret.as_deref() else {
            return true;
        };
        let channel_header = if self.channel == ZALO_CHANNEL {
            "x-zalo-secret"
        } else {
            "x-zalouser-secret"
        };
        let candidate = request
            .header("x-duckagent-gateway-secret")
            .or_else(|| request.header(channel_header))
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
            .expect("zalo seen message ids mutex poisoned");
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
            .ok_or_else(|| anyhow!("{} channel requires bridge API URL", self.channel))?;
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
        let response = request.send().context("Zalo bridge POST failed")?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            bail!("Zalo bridge POST failed with status {status}: {text}");
        }
        Ok(())
    }
}

impl ChannelAdapter for ZaloAdapter {
    fn start(&self, _inbound: GatewayInboundDispatch) -> Result<()> {
        Ok(())
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        let expected_paths = if self.channel == ZALO_CHANNEL {
            ["/zalo/events", "/zalo/webhook"]
        } else {
            ["/zalouser/events", "/zalouser/webhook"]
        };
        if request.method == "POST" && expected_paths.iter().any(|path| request.path == *path) {
            return self.handle_zalo_event(request, inbound).map(Some);
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
                    "channel": self.channel,
                    "platform": self.platform,
                    "app_id": self.app_id.as_deref(),
                    "account_id": self.account_id.as_deref(),
                    "conversation_id": conversation_id,
                    "recipient_id": conversation_id,
                    "chat_type": route_chat_type(conversation_id),
                    "thread_id": thread_id,
                    "reply_to": reply_to,
                    "text": chunk,
                    "message": chunk,
                    "media_paths": [],
                }),
            )?;
        }
        if !message.media_paths.is_empty() {
            self.post_bridge(
                &self.send_endpoint,
                json!({
                    "channel": self.channel,
                    "platform": self.platform,
                    "app_id": self.app_id.as_deref(),
                    "account_id": self.account_id.as_deref(),
                    "conversation_id": conversation_id,
                    "recipient_id": conversation_id,
                    "chat_type": route_chat_type(conversation_id),
                    "thread_id": thread_id,
                    "reply_to": reply_to,
                    "text": "",
                    "message": "",
                    "media_paths": &message.media_paths,
                    "media_mode": "zalo_upload_or_link",
                }),
            )?;
        }
        Ok(())
    }

    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()> {
        let Some(endpoint) = self.typing_endpoint.as_deref() else {
            return Ok(());
        };
        self.post_bridge(
            endpoint,
            json!({
                "channel": self.channel,
                "platform": self.platform,
                "conversation_id": route.key.conversation_id.as_str(),
                "recipient_id": route.key.conversation_id.as_str(),
                "thread_id": route.key.thread_id.as_deref(),
                "active": event.active,
                "reason": event.reason,
            }),
        )
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
                "channel": self.channel,
                "platform": self.platform,
                "app_id": self.app_id.as_deref(),
                "account_id": self.account_id.as_deref(),
                "conversation_id": route.key.conversation_id.as_str(),
                "recipient_id": route.key.conversation_id.as_str(),
                "chat_type": route_chat_type(&route.key.conversation_id),
                "thread_id": route.key.thread_id.as_deref(),
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
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: true,
            typing: self.typing_endpoint.is_some(),
            approval_prompt: true,
        }
    }
}

pub(in crate::gateway::channels) fn new_adapter(
    config: &GatewayChannelConfig,
    credentials: &GatewayCredentialEntry,
) -> Result<ZaloAdapter> {
    ZaloAdapter::new_for_channel(ZaloMode::OfficialAccount, config, credentials)
}

pub(in crate::gateway::channels) fn new_user_adapter(
    config: &GatewayChannelConfig,
    credentials: &GatewayCredentialEntry,
) -> Result<ZaloAdapter> {
    ZaloAdapter::new_for_channel(ZaloMode::UserSession, config, credentials)
}

fn parse_zalo_request(request: &ChannelHttpRequest) -> Result<Value> {
    if let Ok(value) = serde_json::from_slice(&request.body) {
        return Ok(value);
    }
    let body = std::str::from_utf8(&request.body).context("Zalo request is not UTF-8")?;
    let mut form = serde_json::Map::new();
    for pair in body.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = percent_decode(parts.next().unwrap_or_default());
        let value = percent_decode(parts.next().unwrap_or_default());
        if matches!(key.as_str(), "payload" | "data" | "event")
            && value.trim_start().starts_with('{')
        {
            return serde_json::from_str(&value)
                .with_context(|| format!("failed to parse Zalo form {key} JSON"));
        }
        if !key.is_empty() {
            form.insert(key, Value::String(value));
        }
    }
    if form.is_empty() {
        bail!("failed to parse Zalo request as JSON or form body")
    }
    Ok(Value::Object(form))
}

fn normalize_zalo_event(event: Value) -> Value {
    let Some(payload) = event
        .get("payload")
        .and_then(Value::as_str)
        .or_else(|| event.get("data").and_then(Value::as_str))
    else {
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

fn zalo_media_urls(text: &str) -> Vec<String> {
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

fn zalo_event_body(event: &Value) -> &Value {
    event
        .get("event")
        .or_else(|| event.get("data"))
        .or_else(|| event.get("payload"))
        .filter(|value| value.is_object())
        .unwrap_or(event)
}

fn zalo_text(body: &Value, event: &Value) -> String {
    first_zalo_str(body, event, &["text", "body", "content"])
        .or_else(|| {
            body.get("message")
                .filter(|value| value.is_object())
                .and_then(|value| first_str(value, &["text", "body", "content"]))
                .map(str::to_string)
        })
        .or_else(|| {
            event
                .get("message")
                .filter(|value| value.is_object())
                .and_then(|value| first_str(value, &["text", "body", "content"]))
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn zalo_chat_type(body: &Value, conversation_id: &str, mode: ZaloMode) -> String {
    if first_str(body, &["group_id"]).is_some() || conversation_id.starts_with("group:") {
        return "group".to_string();
    }
    match mode {
        ZaloMode::OfficialAccount => "oa".to_string(),
        ZaloMode::UserSession => "user".to_string(),
    }
}

fn route_chat_type(conversation_id: &str) -> &'static str {
    let lower = conversation_id.to_ascii_lowercase();
    if lower.starts_with("group:")
        || lower.starts_with("group_")
        || lower.starts_with("gid")
        || lower.starts_with("c_")
    {
        "group"
    } else {
        "direct"
    }
}

fn first_zalo_str(body: &Value, event: &Value, keys: &[&str]) -> Option<String> {
    first_str(body, keys)
        .or_else(|| first_str(event, keys))
        .map(str::to_string)
}

fn nested_str<'a>(value: &'a Value, object_key: &str, field_key: &str) -> Option<&'a str> {
    value
        .get(object_key)
        .and_then(Value::as_object)
        .and_then(|object| object.get(field_key))
        .and_then(Value::as_str)
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
        "zalo:",
        "user:",
        "group:",
        "oa:",
        "session:",
        "conversation:",
    ] {
        if let Some(stripped) = value.strip_prefix(prefix) {
            value = stripped.to_string();
        }
    }
    value
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
    fn zalo_oa_routes_sender_id_as_conversation() -> Result<()> {
        let adapter = ZaloAdapter::new_for_channel(
            ZaloMode::OfficialAccount,
            &GatewayChannelConfig::default(),
            &GatewayCredentialEntry {
                channel: ZALO_CHANNEL.to_string(),
                ..Default::default()
            },
        )?;
        let input = adapter
            .event_to_inbound(&json!({
                "sender": {"id": "u1"},
                "message": {"msg_id": "m1", "text": "hi"}
            }))?
            .expect("zalo event should parse");
        assert_eq!(input.conversation_id, "u1");
        assert_eq!(input.chat_type.as_deref(), Some("oa"));
        Ok(())
    }

    #[test]
    fn zalouser_uses_user_chat_type() -> Result<()> {
        let adapter = ZaloAdapter::new_for_channel(
            ZaloMode::UserSession,
            &GatewayChannelConfig::default(),
            &GatewayCredentialEntry {
                channel: ZALOUSER_CHANNEL.to_string(),
                ..Default::default()
            },
        )?;
        let input = adapter
            .event_to_inbound(&json!({
                "sender": {"id": "u1"},
                "message": {"msg_id": "m1", "text": "hi"}
            }))?
            .expect("zalouser event should parse");
        assert_eq!(input.channel, ZALOUSER_CHANNEL);
        assert_eq!(input.chat_type.as_deref(), Some("user"));
        Ok(())
    }
}
