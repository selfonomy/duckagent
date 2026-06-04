use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, InboundAttachmentInput,
    InboundMessageInput, OutboundMessage, TypingEvent,
};
use super::websocket::{
    ChannelWebSocket, is_transient_read_error, read_json_message as read_ws_json_message,
    send_json_message as send_ws_json_message, set_read_timeout,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use regex::{Regex, RegexBuilder};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tungstenite::connect;

const DINGTALK_EVENTS_PATH: &str = "/dingtalk/events";
const DINGTALK_GATEWAY_OPEN_URL: &str = "https://api.dingtalk.com/v1.0/gateway/connections/open";
const DINGTALK_BOT_CALLBACK_TOPIC: &str = "/v1.0/im/bot/messages/get";
const DINGTALK_TEXT_LIMIT: usize = 20_000;
const DEDUP_LIMIT: usize = 1_000;
const SESSION_WEBHOOK_LIMIT: usize = 500;
const DINGTALK_RECONNECT_BACKOFF: &[u64] = &[2, 5, 10, 30, 60];

#[derive(Clone)]
pub(in crate::gateway) struct DingTalkAdapter {
    transport: String,
    client_id: Option<String>,
    client_secret: Option<String>,
    signing_secret: Option<String>,
    allowed_users: HashSet<String>,
    allowed_chats: HashSet<String>,
    require_mention: bool,
    free_response_chats: HashSet<String>,
    mention_patterns: Vec<Regex>,
    max_download_bytes: u64,
    default_session_webhook: Option<String>,
    client: Client,
    session_webhooks: Arc<Mutex<HashMap<String, (String, i64)>>>,
    seen_message_ids: Arc<Mutex<VecDeque<String>>>,
}

#[derive(Debug, Clone)]
struct DingTalkInboundEvent {
    message_id: String,
    conversation_id: String,
    conversation_type: String,
    sender_id: String,
    sender_staff_id: String,
    text: String,
    is_in_at_list: bool,
    session_webhook: Option<String>,
    session_webhook_expired_time: i64,
    media_refs: Vec<DingTalkMediaRef>,
    timestamp: Option<String>,
}

#[derive(Debug, Clone)]
struct DingTalkMediaRef {
    url: Option<String>,
    download_code: Option<String>,
    mime: String,
    filename: String,
}

#[derive(Debug, Deserialize)]
struct DingTalkGatewayConnection {
    endpoint: String,
    ticket: String,
}

impl DingTalkAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let transport = config
            .transport
            .as_deref()
            .unwrap_or("stream")
            .trim()
            .to_ascii_lowercase();
        let client_id = credentials
            .app_id
            .as_deref()
            .or_else(|| credentials.extra.get("client_id").map(String::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let client_secret = credentials
            .app_secret
            .as_deref()
            .or(credentials.client_secret.as_deref())
            .or_else(|| credentials.extra.get("client_secret").map(String::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let signing_secret = credentials
            .signing_secret
            .as_deref()
            .or(credentials.webhook_secret.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build DingTalk HTTP client")?;
        let mut allowed_users = config
            .allowed_users
            .iter()
            .map(|value| value.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        allowed_users.extend(
            parse_extra_list(config.extra.get("allowed_users"))
                .map(|value| value.to_ascii_lowercase()),
        );
        let mut allowed_chats = config.allowed_chats.iter().cloned().collect::<HashSet<_>>();
        allowed_chats.extend(parse_extra_list(config.extra.get("allowed_chats")));
        Ok(Self {
            transport,
            client_id,
            client_secret,
            signing_secret,
            allowed_users,
            allowed_chats,
            require_mention: parse_bool(config.extra.get("require_mention"), true),
            free_response_chats: parse_extra_list(config.extra.get("free_response_chats"))
                .collect(),
            mention_patterns: parse_mention_patterns(config.extra.get("mention_patterns"))?,
            max_download_bytes: config.media.max_download_bytes,
            default_session_webhook: credentials
                .extra
                .get("session_webhook")
                .or_else(|| config.extra.get("session_webhook"))
                .cloned(),
            client,
            session_webhooks: Arc::new(Mutex::new(HashMap::new())),
            seen_message_ids: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    fn register_stream_connection(&self) -> Result<DingTalkGatewayConnection> {
        let client_id = self
            .client_id
            .as_deref()
            .ok_or_else(|| anyhow!("dingtalk stream transport requires client id/app key"))?;
        let client_secret = self.client_secret.as_deref().ok_or_else(|| {
            anyhow!("dingtalk stream transport requires client secret/app secret")
        })?;
        let response = self
            .client
            .post(DINGTALK_GATEWAY_OPEN_URL)
            .json(&json!({
                "clientId": client_id,
                "clientSecret": client_secret,
                "subscriptions": [{
                    "type": "CALLBACK",
                    "topic": DINGTALK_BOT_CALLBACK_TOPIC
                }]
            }))
            .send()
            .context("DingTalk stream connection registration failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .context("DingTalk stream registration returned invalid JSON")?;
        if !status.is_success() {
            bail!("DingTalk stream registration failed with status {status}: {value}");
        }
        serde_json::from_value(value)
            .context("DingTalk stream registration missing endpoint/ticket")
    }

    fn stream_loop(self, inbound: GatewayInboundDispatch) {
        let mut attempt = 0usize;
        loop {
            match self.consume_stream_once(&inbound) {
                Ok(()) => attempt = 0,
                Err(error) => eprintln!("dingtalk stream disconnected: {error:#}"),
            }
            let sleep = DINGTALK_RECONNECT_BACKOFF
                .get(attempt)
                .copied()
                .unwrap_or(*DINGTALK_RECONNECT_BACKOFF.last().unwrap_or(&60));
            attempt = attempt.saturating_add(1);
            thread::sleep(Duration::from_secs(sleep));
        }
    }

    fn consume_stream_once(&self, inbound: &GatewayInboundDispatch) -> Result<()> {
        let connection = self.register_stream_connection()?;
        let ws_url = format!(
            "{}?ticket={}",
            connection.endpoint.trim_end_matches('?'),
            connection.ticket
        );
        let (mut socket, _) = connect(ws_url.as_str())
            .with_context(|| format!("DingTalk stream websocket connect: {ws_url}"))?;
        set_read_timeout(&mut socket, Duration::from_secs(45));
        loop {
            match read_ws_json_message(
                &mut socket,
                "DingTalk stream websocket read failed",
                "DingTalk stream websocket returned invalid JSON",
            ) {
                Ok(Some(frame)) => self.handle_stream_frame(&mut socket, &frame, inbound)?,
                Ok(None) => {}
                Err(error) if is_transient_read_error(&error) => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn handle_stream_frame(
        &self,
        socket: &mut ChannelWebSocket,
        frame: &Value,
        inbound: &GatewayInboundDispatch,
    ) -> Result<()> {
        let frame_type = frame
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match frame_type {
            "SYSTEM" => self.ack_stream_frame(socket, frame),
            "EVENT" | "CALLBACK" => {
                let Some(data) = dingtalk_stream_frame_data(frame) else {
                    self.ack_stream_frame(socket, frame)?;
                    return Ok(());
                };
                self.submit_event_value(&data, inbound)?;
                self.ack_stream_frame(socket, frame)
            }
            _ => Ok(()),
        }
    }

    fn ack_stream_frame(&self, socket: &mut ChannelWebSocket, frame: &Value) -> Result<()> {
        let message_id = frame
            .get("headers")
            .and_then(|headers| headers.get("messageId"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        send_ws_json_message(
            socket,
            &json!({
                "code": 200,
                "headers": {
                    "contentType": "application/json",
                    "messageId": message_id
                },
                "message": "OK",
                "data": ""
            }),
            "DingTalk stream ack send failed",
        )
    }

    fn handle_event(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        self.verify_signature(&request)?;
        let value: Value = serde_json::from_slice(&request.body)
            .context("failed to parse DingTalk callback JSON")?;
        if let Some(challenge) = value["challenge"].as_str() {
            return Ok(json_response(200, json!({"challenge": challenge})));
        }
        let mut accepted = 0usize;
        for item in dingtalk_event_items(&value) {
            if self.submit_event_value(item, &inbound)? {
                accepted += 1;
            }
        }
        Ok(json_response(
            200,
            json!({"status": "accepted", "accepted": accepted}),
        ))
    }

    fn submit_event_value(&self, item: &Value, inbound: &GatewayInboundDispatch) -> Result<bool> {
        let event = parse_dingtalk_event(item)?;
        if self.is_duplicate(&event.message_id) {
            return Ok(false);
        }
        if !self.is_user_allowed(&event) || !self.should_process_message(&event) {
            return Ok(false);
        }
        self.remember_session_webhook(&event);
        let (attachments, media_notes) = self.resolve_media_refs(&event.media_refs)?;
        let chat_type = if event.conversation_type == "2" {
            "group"
        } else {
            "dm"
        };
        let mut text = if event.text.trim().is_empty() && !media_notes.is_empty() {
            media_notes.join("\n")
        } else if media_notes.is_empty() {
            event.text.clone()
        } else {
            format!("{}\n\n{}", event.text.trim(), media_notes.join("\n"))
                .trim()
                .to_string()
        };
        if chat_type == "group" && event.is_in_at_list {
            text = strip_dingtalk_leading_mention(&text);
        }
        inbound.submit(InboundMessageInput {
            channel: "dingtalk".to_string(),
            conversation_id: event.conversation_id,
            thread_id: None,
            chat_type: Some(chat_type.to_string()),
            sender_id: Some(
                event
                    .sender_staff_id
                    .clone()
                    .if_empty(event.sender_id.clone()),
            ),
            message_id: Some(event.message_id),
            text,
            attachments,
            timestamp: event.timestamp,
        })?;
        Ok(true)
    }

    fn verify_signature(&self, request: &ChannelHttpRequest) -> Result<()> {
        let Some(secret) = self.signing_secret.as_deref() else {
            return Ok(());
        };
        let timestamp = request
            .query
            .get("timestamp")
            .map(String::as_str)
            .or_else(|| request.header("timestamp"))
            .or_else(|| request.header("x-dingtalk-timestamp"))
            .ok_or_else(|| anyhow!("DingTalk callback missing timestamp"))?;
        let actual = request
            .query
            .get("sign")
            .map(String::as_str)
            .or_else(|| request.header("sign"))
            .or_else(|| request.header("x-dingtalk-signature"))
            .ok_or_else(|| anyhow!("DingTalk callback missing sign"))?;
        let payload = format!("{timestamp}\n{secret}");
        let expected = base64::engine::general_purpose::STANDARD
            .encode(hmac_sha256(secret.as_bytes(), payload.as_bytes()));
        let actual = percent_decode(actual);
        if constant_time_eq(expected.as_bytes(), actual.as_bytes()) {
            Ok(())
        } else {
            bail!("DingTalk callback signature mismatch")
        }
    }

    fn is_duplicate(&self, message_id: &str) -> bool {
        if message_id.is_empty() {
            return false;
        }
        let mut guard = self
            .seen_message_ids
            .lock()
            .expect("dingtalk dedup mutex poisoned");
        if guard.iter().any(|seen| seen == message_id) {
            return true;
        }
        guard.push_back(message_id.to_string());
        while guard.len() > DEDUP_LIMIT {
            guard.pop_front();
        }
        false
    }

    fn is_user_allowed(&self, event: &DingTalkInboundEvent) -> bool {
        if self.allowed_users.is_empty() || self.allowed_users.contains("*") {
            return true;
        }
        let candidates = [
            event.sender_id.to_ascii_lowercase(),
            event.sender_staff_id.to_ascii_lowercase(),
        ];
        candidates
            .iter()
            .any(|candidate| !candidate.is_empty() && self.allowed_users.contains(candidate))
    }

    fn should_process_message(&self, event: &DingTalkInboundEvent) -> bool {
        let is_group = event.conversation_type == "2";
        if !is_group {
            return true;
        }
        if !self.allowed_chats.is_empty() && !self.allowed_chats.contains(&event.conversation_id) {
            return false;
        }
        if self.free_response_chats.contains(&event.conversation_id) {
            return true;
        }
        if !self.require_mention {
            return true;
        }
        if event.is_in_at_list || event.text.trim().starts_with('/') {
            return true;
        }
        self.mention_patterns
            .iter()
            .any(|pattern| pattern.is_match(&event.text))
    }

    fn remember_session_webhook(&self, event: &DingTalkInboundEvent) {
        let Some(webhook) = event
            .session_webhook
            .as_deref()
            .filter(|value| is_dingtalk_session_webhook(value))
        else {
            return;
        };
        let mut webhooks = self
            .session_webhooks
            .lock()
            .expect("dingtalk webhook cache mutex poisoned");
        if webhooks.len() >= SESSION_WEBHOOK_LIMIT {
            if let Some(key) = webhooks.keys().next().cloned() {
                webhooks.remove(&key);
            }
        }
        webhooks.insert(
            event.conversation_id.clone(),
            (webhook.to_string(), event.session_webhook_expired_time),
        );
    }

    fn resolve_media_refs(
        &self,
        refs: &[DingTalkMediaRef],
    ) -> Result<(Vec<InboundAttachmentInput>, Vec<String>)> {
        let mut attachments = Vec::new();
        let mut notes = Vec::new();
        for media in refs {
            if let Some(url) = media.url.as_deref().filter(|url| url.starts_with("http")) {
                let response = self
                    .client
                    .get(url)
                    .send()
                    .with_context(|| format!("DingTalk media download failed: {url}"))?;
                let status = response.status();
                if !status.is_success() {
                    bail!("DingTalk media download failed with status {status}: {url}");
                }
                let bytes = response.bytes()?;
                if bytes.len() as u64 > self.max_download_bytes {
                    bail!("DingTalk attachment exceeds configured max_download_bytes: {url}");
                }
                attachments.push(InboundAttachmentInput {
                    bytes: Some(bytes.to_vec()),
                    path: None,
                    filename: Some(media.filename.clone()),
                    mime: Some(media.mime.clone()),
                });
            } else if let Some(code) = media.download_code.as_deref() {
                notes.push(format!(
                    "[DingTalk Media]\ndownload_code: {code}\nmime: {}\nfilename: {}",
                    media.mime, media.filename
                ));
            }
        }
        Ok((attachments, notes))
    }

    fn get_session_webhook(&self, chat_id: &str) -> Option<String> {
        if let Some(default) = self.default_session_webhook.as_ref() {
            return Some(default.clone());
        }
        let mut webhooks = self
            .session_webhooks
            .lock()
            .expect("dingtalk webhook cache mutex poisoned");
        let Some((url, expired_time)) = webhooks.get(chat_id).cloned() else {
            return None;
        };
        if expired_time > 0 {
            let safety_margin_ms = 5 * 60 * 1000;
            if now_millis() + safety_margin_ms >= expired_time {
                webhooks.remove(chat_id);
                return None;
            }
        }
        Some(url)
    }

    fn post_markdown(&self, route: &GatewayRoute, text: &str) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let webhook = self
            .get_session_webhook(&route.key.conversation_id)
            .ok_or_else(|| {
                anyhow!(
                    "No DingTalk session_webhook available. Reply must follow an incoming message."
                )
            })?;
        let response = self
            .client
            .post(&webhook)
            .json(&json!({
                "msgtype": "markdown",
                "markdown": {
                    "title": "DuckAgent",
                    "text": normalize_dingtalk_markdown(text),
                }
            }))
            .send()
            .context("DingTalk session webhook POST failed")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            bail!("DingTalk session webhook failed with status {status}: {body}");
        }
        Ok(())
    }

    fn send_media_path(
        &self,
        route: &GatewayRoute,
        path: &str,
        caption: Option<&str>,
    ) -> Result<()> {
        if path.starts_with("http://") || path.starts_with("https://") {
            let text = if is_image_path(path) {
                caption
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| format!("{value}\n\n![image]({path})"))
                    .unwrap_or_else(|| format!("![image]({path})"))
            } else {
                caption
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| format!("{value}\n{path}"))
                    .unwrap_or_else(|| path.to_string())
            };
            return self.post_markdown(route, &text);
        }
        bail!(
            "DingTalk session webhook cannot upload local file attachments directly: {}",
            path
        )
    }
}

impl ChannelAdapter for DingTalkAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        if matches!(
            self.transport.as_str(),
            "stream" | "stream_mode" | "websocket"
        ) {
            let adapter = self.clone();
            thread::spawn(move || adapter.stream_loop(inbound));
        }
        Ok(())
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        if request.method == "POST" && request.path == DINGTALK_EVENTS_PATH {
            return self.handle_event(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let chunks = dingtalk_text_chunks(&message.text);
        let mut text_sent = false;
        for chunk in chunks {
            self.post_markdown(route, &chunk)?;
            text_sent = true;
        }
        let mut caption = (!text_sent)
            .then_some(message.text.trim())
            .filter(|value| !value.is_empty());
        for media_path in message.media_paths {
            self.send_media_path(route, &media_path, caption)?;
            caption = None;
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
        self.post_markdown(
            route,
            &format!(
                "{}\n\nCommands:\n\n`/approve {} once`\n\n`/approve {} session`\n\n`/approve {} always`\n\n`/deny {}`",
                prompt.message, prompt.id, prompt.id, prompt.id, prompt.id
            ),
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

fn parse_dingtalk_event(value: &Value) -> Result<DingTalkInboundEvent> {
    let value = dingtalk_event_body(value);
    let message_id = string_at(value, &["messageId", "message_id", "msgId", "msg_id", "id"])
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    let conversation_id = string_at(
        value,
        &[
            "conversationId",
            "conversation_id",
            "conversation_id",
            "chatId",
            "chat_id",
            "openConversationId",
            "open_conversation_id",
        ],
    )
    .or_else(|| string_at(&value["conversation"], &["id"]))
    .or_else(|| string_at(value, &["senderId", "sender_id"]))
    .ok_or_else(|| anyhow!("DingTalk event missing conversation id"))?;
    let conversation_type = string_at(value, &["conversationType", "conversation_type"])
        .unwrap_or_else(|| {
            if value["conversationType"].as_i64() == Some(2)
                || value["conversation_type"].as_i64() == Some(2)
                || value["isGroup"].as_bool().unwrap_or(false)
                || value["chatType"]
                    .as_str()
                    .is_some_and(|value| value.eq_ignore_ascii_case("group"))
                || value["chat_type"]
                    .as_str()
                    .is_some_and(|value| value.eq_ignore_ascii_case("group"))
            {
                "2".to_string()
            } else {
                "1".to_string()
            }
        });
    let sender_id =
        string_at(value, &["senderId", "sender_id", "fromUserId", "userId"]).unwrap_or_default();
    let sender_staff_id =
        string_at(value, &["senderStaffId", "sender_staff_id", "staffId"]).unwrap_or_default();
    let text = extract_dingtalk_text(value);
    let media_refs = extract_dingtalk_media_refs(value);
    Ok(DingTalkInboundEvent {
        message_id,
        conversation_id,
        conversation_type,
        sender_id,
        sender_staff_id,
        text,
        is_in_at_list: dingtalk_is_at_bot(value),
        session_webhook: string_at(
            value,
            &[
                "sessionWebhook",
                "session_webhook",
                "session_webhook_url",
                "webhook",
            ],
        ),
        session_webhook_expired_time: value["sessionWebhookExpiredTime"]
            .as_i64()
            .or_else(|| value["session_webhook_expired_time"].as_i64())
            .or_else(|| {
                value["sessionWebhookExpiredTime"]
                    .as_str()
                    .and_then(|value| value.parse().ok())
            })
            .or_else(|| {
                value["session_webhook_expired_time"]
                    .as_str()
                    .and_then(|value| value.parse().ok())
            })
            .unwrap_or_default(),
        media_refs,
        timestamp: string_at(value, &["createAt", "create_at", "timestamp"]),
    })
}

fn dingtalk_event_items(value: &Value) -> Vec<&Value> {
    for path in [
        "/events",
        "/messages",
        "/data/events",
        "/data/messages",
        "/payload/events",
        "/payload/messages",
    ] {
        if let Some(items) = value.pointer(path).and_then(Value::as_array) {
            return items.iter().collect();
        }
    }
    vec![value]
}

fn dingtalk_stream_frame_data(frame: &Value) -> Option<Value> {
    match frame.get("data") {
        Some(Value::String(raw)) => serde_json::from_str(raw).ok(),
        Some(Value::Object(_)) => frame.get("data").cloned(),
        _ => None,
    }
}

fn dingtalk_event_body(mut value: &Value) -> &Value {
    for _ in 0..4 {
        let Some(next) = value
            .get("data")
            .or_else(|| value.get("payload"))
            .or_else(|| value.get("event"))
            .or_else(|| value.get("message"))
            .or_else(|| value.get("chatbotMessage"))
        else {
            break;
        };
        if !next.is_object() {
            break;
        }
        value = next;
    }
    value
}

fn extract_dingtalk_text(value: &Value) -> String {
    let text = &value["text"];
    if let Some(content) = text["content"].as_str() {
        return content.trim().to_string();
    }
    if let Some(content) = text.as_str() {
        return content.trim().to_string();
    }
    if let Some(content) = value["content"].as_str() {
        return content.trim().to_string();
    }
    let rich = value
        .get("richTextContent")
        .or_else(|| value.get("rich_text_content"))
        .or_else(|| value.get("richText"))
        .or_else(|| value.get("rich_text"));
    let Some(rich) = rich else {
        return String::new();
    };
    let items = rich
        .get("richTextList")
        .or_else(|| rich.get("rich_text_list"))
        .and_then(Value::as_array)
        .or_else(|| rich.as_array());
    items
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item["text"]
                        .as_str()
                        .or_else(|| item["content"].as_str())
                        .or_else(|| item["text"]["content"].as_str())
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn extract_dingtalk_media_refs(value: &Value) -> Vec<DingTalkMediaRef> {
    let mut out = Vec::new();
    for node in [
        value.get("imageContent"),
        value.get("image_content"),
        value.get("fileContent"),
        value.get("file_content"),
        value.get("audioContent"),
        value.get("audio_content"),
        value.get("videoContent"),
        value.get("video_content"),
        value.get("picture"),
        value.get("audio"),
        value.get("video"),
        value.get("file"),
        value.get("attachment"),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(media) = media_ref_from_node(node) {
            out.push(media);
        }
    }
    let rich = value
        .get("richTextContent")
        .or_else(|| value.get("rich_text_content"))
        .or_else(|| value.get("richText"))
        .or_else(|| value.get("rich_text"));
    if let Some(items) = rich
        .and_then(|rich| {
            rich.get("richTextList")
                .or_else(|| rich.get("rich_text_list"))
        })
        .and_then(Value::as_array)
        .or_else(|| rich.and_then(Value::as_array))
    {
        for item in items {
            if let Some(media) = media_ref_from_node(item) {
                out.push(media);
            }
        }
    }
    for key in ["attachments", "files", "media"] {
        if let Some(items) = value[key].as_array() {
            for item in items {
                if let Some(media) = media_ref_from_node(item) {
                    out.push(media);
                }
            }
        }
    }
    out
}

fn media_ref_from_node(node: &Value) -> Option<DingTalkMediaRef> {
    let url = string_at(
        node,
        &[
            "downloadUrl",
            "download_url",
            "mediaUrl",
            "media_url",
            "url",
            "mediaId",
            "media_id",
        ],
    );
    let download_code = string_at(
        node,
        &[
            "downloadCode",
            "download_code",
            "pictureDownloadCode",
            "picture_download_code",
            "mediaDownloadCode",
            "media_download_code",
        ],
    );
    if url.is_none() && download_code.is_none() {
        return None;
    }
    let filename = string_at(node, &["fileName", "file_name", "name"])
        .or_else(|| {
            url.as_deref()
                .and_then(|url| url.split('/').next_back().map(str::to_string))
        })
        .unwrap_or_else(|| "dingtalk-attachment".to_string());
    let node_type = string_at(node, &["type", "msgType", "messageType"]).unwrap_or_default();
    Some(DingTalkMediaRef {
        url,
        download_code,
        mime: infer_dingtalk_mime(&filename, &node_type),
        filename,
    })
}

fn dingtalk_is_at_bot(value: &Value) -> bool {
    if value["isInAtList"]
        .as_bool()
        .or_else(|| value["is_in_at_list"].as_bool())
        .unwrap_or(false)
    {
        return true;
    }
    for key in ["atUsers", "at_users", "mentions", "mentioned_users"] {
        if value[key].as_array().is_some_and(|items| !items.is_empty()) {
            return true;
        }
    }
    false
}

fn strip_dingtalk_leading_mention(text: &str) -> String {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('@') {
        return text.trim().to_string();
    }
    trimmed
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim().to_string())
        .unwrap_or_default()
}

fn string_at(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value[*key].as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn parse_bool(value: Option<&String>, default: bool) -> bool {
    value
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}

fn parse_extra_list(value: Option<&String>) -> impl Iterator<Item = String> + '_ {
    value
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn parse_mention_patterns(raw: Option<&String>) -> Result<Vec<Regex>> {
    let Some(raw) = raw
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(Vec::new());
    };
    let patterns = if raw.starts_with('[') {
        serde_json::from_str::<Vec<String>>(raw).unwrap_or_else(|_| vec![raw.to_string()])
    } else {
        raw.split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect()
    };
    patterns
        .into_iter()
        .map(|pattern| {
            RegexBuilder::new(&pattern)
                .case_insensitive(true)
                .build()
                .with_context(|| format!("invalid DingTalk mention pattern `{pattern}`"))
        })
        .collect()
}

fn dingtalk_text_chunks(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if trimmed.chars().count() <= DINGTALK_TEXT_LIMIT {
        return vec![trimmed.to_string()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in trimmed.chars() {
        if current.chars().count() >= DINGTALK_TEXT_LIMIT {
            out.push(current.clone());
            current.clear();
        }
        current.push(ch);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn normalize_dingtalk_markdown(text: &str) -> String {
    text.chars().take(DINGTALK_TEXT_LIMIT).collect()
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn infer_dingtalk_mime(filename: &str, node_type: &str) -> String {
    match Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "png" => "image/png".to_string(),
        "gif" => "image/gif".to_string(),
        "mp3" => "audio/mpeg".to_string(),
        "ogg" => "audio/ogg".to_string(),
        "mp4" => "video/mp4".to_string(),
        "pdf" => "application/pdf".to_string(),
        _ if node_type.eq_ignore_ascii_case("picture") => "image/jpeg".to_string(),
        _ if node_type.eq_ignore_ascii_case("voice") => "audio/mpeg".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

fn is_image_path(path: &str) -> bool {
    matches!(
        Path::new(path)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "webp"
    )
}

fn is_dingtalk_session_webhook(value: &str) -> bool {
    value.starts_with("https://api.dingtalk.com/")
        || value.starts_with("https://oapi.dingtalk.com/")
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut key_block = [0u8; 64];
    if key.len() > 64 {
        let digest = Sha256::digest(key);
        key_block[..digest.len()].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut o_key_pad = [0x5c_u8; 64];
    let mut i_key_pad = [0x36_u8; 64];
    for index in 0..64 {
        o_key_pad[index] ^= key_block[index];
        i_key_pad[index] ^= key_block[index];
    }
    let mut inner = Sha256::new();
    inner.update(i_key_pad);
    inner.update(message);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(o_key_pad);
    outer.update(inner_hash);
    outer.finalize().to_vec()
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

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push((hi << 4) | lo);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
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

fn json_response(status: u16, value: Value) -> ChannelHttpResponse {
    ChannelHttpResponse {
        status,
        content_type: "application/json",
        body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"error\":\"json\"}".to_vec()),
    }
}

trait IfEmpty {
    fn if_empty(self, fallback: String) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, fallback: String) -> String {
        if self.is_empty() { fallback } else { self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(extra: &[(&str, &str)]) -> DingTalkAdapter {
        let mut config = GatewayChannelConfig {
            ..Default::default()
        };
        for (key, value) in extra {
            config
                .extra
                .insert((*key).to_string(), (*value).to_string());
        }
        DingTalkAdapter::new(
            &config,
            &GatewayCredentialEntry {
                channel: "dingtalk".to_string(),
                ..Default::default()
            },
        )
        .expect("adapter")
    }

    #[test]
    fn extracts_text_from_dict_and_rich_text() {
        assert_eq!(
            extract_dingtalk_text(&json!({"text": {"content": "  hello  "}})),
            "hello"
        );
        assert_eq!(
            extract_dingtalk_text(&json!({
                "richTextContent": {"richTextList": [{"text": "a"}, {"content": "b"}]}
            })),
            "a b"
        );
    }

    #[test]
    fn group_messages_require_mention_when_configured() -> Result<()> {
        let adapter = adapter(&[("require_mention", "true")]);
        let base = parse_dingtalk_event(&json!({
            "messageId": "m1",
            "conversationId": "cid",
            "conversationType": "2",
            "senderId": "u1",
            "text": {"content": "hello"}
        }))?;
        assert!(!adapter.should_process_message(&base));
        let mut mentioned = base.clone();
        mentioned.is_in_at_list = true;
        assert!(adapter.should_process_message(&mentioned));
        Ok(())
    }

    #[test]
    fn mention_patterns_wake_group_messages() -> Result<()> {
        let adapter = adapter(&[
            ("require_mention", "true"),
            ("mention_patterns", r"^\s*duck"),
        ]);
        let event = parse_dingtalk_event(&json!({
            "messageId": "m1",
            "conversationId": "cid",
            "conversationType": "2",
            "senderId": "u1",
            "text": {"content": "duck please check this"}
        }))?;
        assert!(adapter.should_process_message(&event));
        Ok(())
    }

    #[test]
    fn dingtalk_signature_matches_official_shape() -> Result<()> {
        let adapter = DingTalkAdapter::new(
            &GatewayChannelConfig::default(),
            &GatewayCredentialEntry {
                channel: "dingtalk".to_string(),
                signing_secret: Some("secret".to_string()),
                ..Default::default()
            },
        )?;
        let timestamp = "1710000000000";
        let payload = format!("{timestamp}\nsecret");
        let sign = base64::engine::general_purpose::STANDARD
            .encode(hmac_sha256(b"secret", payload.as_bytes()));
        adapter.verify_signature(&ChannelHttpRequest {
            method: "POST".to_string(),
            path: DINGTALK_EVENTS_PATH.to_string(),
            query: [
                ("timestamp".to_string(), timestamp.to_string()),
                ("sign".to_string(), sign),
            ]
            .into_iter()
            .collect(),
            headers: Vec::new(),
            body: b"{}".to_vec(),
        })?;
        Ok(())
    }

    #[test]
    fn media_refs_extract_download_code() -> Result<()> {
        let event = parse_dingtalk_event(&json!({
            "messageId": "m1",
            "conversationId": "cid",
            "senderId": "u1",
            "imageContent": {"downloadCode": "code1", "fileName": "a.png"}
        }))?;
        assert_eq!(event.media_refs.len(), 1);
        assert_eq!(event.media_refs[0].download_code.as_deref(), Some("code1"));
        assert_eq!(event.media_refs[0].mime, "image/png");
        Ok(())
    }
}
