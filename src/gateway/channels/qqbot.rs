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
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, connect};

const QQBOT_CHANNEL: &str = "qqbot";
const QQ_API_BASE: &str = "https://api.sgroup.qq.com";
const QQ_AUTH_URL: &str = "https://bots.qq.com/app/getAppAccessToken";
const DEFAULT_SEND_ENDPOINT: &str = "/send";
const DEFAULT_TYPING_ENDPOINT: &str = "/typing";
const DEFAULT_INTERACTION_ACK_ENDPOINT: &str = "/interactions/ack";
const QQ_GATEWAY_READ_TIMEOUT: Duration = Duration::from_millis(500);
const QQ_GATEWAY_RECONNECT_DELAY: Duration = Duration::from_secs(5);
const QQ_DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(41_250);
const QQ_GATEWAY_INTENTS: u64 = (1 << 25) | (1 << 30);
const QQ_TEXT_LIMIT: usize = 4_000;
const QQ_MSG_TYPE_TEXT: i64 = 0;
const QQ_MSG_TYPE_MARKDOWN: i64 = 2;
const QQ_MSG_TYPE_MEDIA: i64 = 7;
const QQ_MSG_TYPE_INPUT_NOTIFY: i64 = 6;

#[derive(Clone)]
pub(in crate::gateway) struct QqBotAdapter {
    bridge_base: Option<String>,
    token: Option<String>,
    webhook_secret: Option<String>,
    allowed_users: HashSet<String>,
    allowed_chats: HashSet<String>,
    dm_policy: String,
    group_policy: String,
    markdown_support: bool,
    max_download_bytes: u64,
    send_endpoint: String,
    typing_endpoint: Option<String>,
    interaction_ack_endpoint: Option<String>,
    client: Client,
    seen_message_ids: Arc<Mutex<VecDeque<String>>>,
    last_message_ids: Arc<Mutex<HashMap<String, String>>>,
    direct_gateway: bool,
    app_id: Option<String>,
    app_secret: Option<String>,
    api_base: String,
    auth_url: String,
    gateway_url: Option<String>,
    session_id: Arc<Mutex<Option<String>>>,
    last_sequence: Arc<Mutex<Option<i64>>>,
}

impl QqBotAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .context("failed to build QQBot bridge HTTP client")?;
        let transport = config.transport.as_deref().unwrap_or("qqbot_bridge");
        let direct_gateway = matches!(
            transport,
            "direct_gateway" | "gateway" | "websocket" | "qq_gateway"
        );
        let api_base = config
            .extra
            .get("api_base")
            .cloned()
            .or_else(|| config.api_base.clone())
            .unwrap_or_else(|| QQ_API_BASE.to_string());
        let auth_url = config
            .extra
            .get("auth_url")
            .cloned()
            .unwrap_or_else(|| QQ_AUTH_URL.to_string());
        let gateway_url = config.extra.get("gateway_url").cloned();
        Ok(Self {
            bridge_base: config.api_base.clone(),
            token: credentials.token.clone().or(credentials.api_key.clone()),
            webhook_secret: credentials
                .webhook_secret
                .clone()
                .or_else(|| credentials.signing_secret.clone()),
            allowed_users: config
                .allowed_users
                .iter()
                .map(|value| normalize_allow_entry(value))
                .collect(),
            allowed_chats: config
                .allowed_chats
                .iter()
                .map(|value| normalize_allow_entry(value))
                .collect(),
            dm_policy: config
                .extra
                .get("dm_policy")
                .map(|value| value.trim().to_ascii_lowercase())
                .unwrap_or_else(|| "open".to_string()),
            group_policy: config
                .extra
                .get("group_policy")
                .map(|value| value.trim().to_ascii_lowercase())
                .unwrap_or_else(|| "open".to_string()),
            markdown_support: config
                .extra
                .get("markdown_support")
                .is_none_or(|value| value.trim() != "false"),
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
            interaction_ack_endpoint: config
                .extra
                .get("interaction_ack_endpoint")
                .cloned()
                .or_else(|| Some(DEFAULT_INTERACTION_ACK_ENDPOINT.to_string())),
            client,
            seen_message_ids: Arc::new(Mutex::new(VecDeque::new())),
            last_message_ids: Arc::new(Mutex::new(HashMap::new())),
            direct_gateway,
            app_id: credentials
                .app_id
                .clone()
                .or_else(|| config.extra.get("app_id").cloned()),
            app_secret: credentials
                .app_secret
                .clone()
                .or_else(|| config.extra.get("app_secret").cloned()),
            api_base,
            auth_url,
            gateway_url,
            session_id: Arc::new(Mutex::new(None)),
            last_sequence: Arc::new(Mutex::new(None)),
        })
    }

    fn start_direct_gateway(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        let app_id = self
            .app_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("qqbot direct_gateway requires app_id"))?;
        let app_secret = self
            .app_secret
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("qqbot direct_gateway requires app_secret"))?;
        if app_id.chars().any(char::is_whitespace) {
            bail!("qqbot app_id must not contain whitespace");
        }
        if app_secret.trim().is_empty() {
            bail!("qqbot app_secret is required");
        }
        let adapter = self.clone();
        thread::Builder::new()
            .name("gateway-qqbot-gateway".to_string())
            .spawn(move || adapter.run_direct_gateway_loop(inbound))
            .context("failed to start QQ Bot Gateway thread")?;
        Ok(())
    }

    fn run_direct_gateway_loop(self, inbound: GatewayInboundDispatch) {
        loop {
            if let Err(error) = self.run_direct_gateway_session(&inbound) {
                eprintln!("QQ Bot Gateway session ended: {error:#}");
            }
            thread::sleep(QQ_GATEWAY_RECONNECT_DELAY);
        }
    }

    fn run_direct_gateway_session(&self, inbound: &GatewayInboundDispatch) -> Result<()> {
        let token = self.fetch_access_token()?;
        let gateway_url = self.gateway_url.clone().unwrap_or_else(|| {
            self.fetch_gateway_url(&token)
                .unwrap_or_else(|_| "wss://api.sgroup.qq.com/websocket".to_string())
        });
        let (mut socket, _) = connect(gateway_url.as_str())
            .with_context(|| format!("failed to connect QQ Bot Gateway {gateway_url}"))?;
        set_qq_ws_read_timeout(&mut socket);
        let hello = read_qq_gateway_json(&mut socket, Duration::from_secs(10))?;
        let mut heartbeat_interval = hello["d"]["heartbeat_interval"]
            .as_u64()
            .map(Duration::from_millis)
            .unwrap_or(QQ_DEFAULT_HEARTBEAT_INTERVAL);
        if heartbeat_interval.is_zero() {
            heartbeat_interval = QQ_DEFAULT_HEARTBEAT_INTERVAL;
        }
        self.identify_or_resume(&mut socket, &token)?;
        let mut last_heartbeat = Instant::now();
        loop {
            if last_heartbeat.elapsed() >= heartbeat_interval {
                self.send_gateway_heartbeat(&mut socket)?;
                last_heartbeat = Instant::now();
            }
            match socket.read() {
                Ok(Message::Text(text)) => {
                    self.handle_gateway_event(inbound, text.as_str())?;
                }
                Ok(Message::Binary(bytes)) => {
                    if let Ok(text) = std::str::from_utf8(bytes.as_ref()) {
                        self.handle_gateway_event(inbound, text)?;
                    }
                }
                Ok(Message::Ping(bytes)) => {
                    socket
                        .send(Message::Pong(bytes))
                        .context("failed to send QQ Bot Gateway pong")?;
                }
                Ok(Message::Close(frame)) => bail!("QQ Bot Gateway closed: {frame:?}"),
                Ok(_) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(error) => return Err(error).context("QQ Bot Gateway read failed"),
            }
        }
    }

    fn fetch_access_token(&self) -> Result<String> {
        let app_id = self
            .app_id
            .as_deref()
            .ok_or_else(|| anyhow!("qqbot direct_gateway requires app_id"))?;
        let app_secret = self
            .app_secret
            .as_deref()
            .ok_or_else(|| anyhow!("qqbot direct_gateway requires app_secret"))?;
        let response = self
            .client
            .post(&self.auth_url)
            .json(&json!({
                "appId": app_id,
                "clientSecret": app_secret,
            }))
            .send()
            .context("QQ Bot token request failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .with_context(|| format!("QQ Bot token response was not JSON: {status}"))?;
        if !status.is_success() {
            bail!("QQ Bot token request failed with status {status}: {value}");
        }
        first_str(&value, &["access_token", "token"])
            .map(str::to_string)
            .ok_or_else(|| anyhow!("QQ Bot token response missing access_token"))
    }

    fn fetch_gateway_url(&self, token: &str) -> Result<String> {
        let response = self
            .client
            .get(format!("{}/gateway", self.api_base.trim_end_matches('/')))
            .header("Authorization", format!("QQBot {token}"))
            .send()
            .context("QQ Bot gateway URL request failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .with_context(|| format!("QQ Bot gateway URL response was not JSON: {status}"))?;
        if !status.is_success() {
            bail!("QQ Bot gateway URL request failed with status {status}: {value}");
        }
        first_str(&value, &["url"])
            .map(str::to_string)
            .ok_or_else(|| anyhow!("QQ Bot gateway URL response missing url"))
    }

    fn identify_or_resume(
        &self,
        socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
        token: &str,
    ) -> Result<()> {
        let session_id = self
            .session_id
            .lock()
            .expect("qqbot session id mutex poisoned")
            .clone();
        let sequence = *self
            .last_sequence
            .lock()
            .expect("qqbot sequence mutex poisoned");
        let payload = if let (Some(session_id), Some(sequence)) = (session_id, sequence) {
            json!({
                "op": 6,
                "d": {
                    "token": format!("QQBot {token}"),
                    "session_id": session_id,
                    "seq": sequence,
                }
            })
        } else {
            json!({
                "op": 2,
                "d": {
                    "token": format!("QQBot {token}"),
                    "intents": QQ_GATEWAY_INTENTS,
                    "properties": {
                        "os": std::env::consts::OS,
                        "browser": "duckagent",
                        "device": "duckagent",
                    }
                }
            })
        };
        socket
            .send(Message::Text(payload.to_string().into()))
            .context("failed to identify QQ Bot Gateway")
    }

    fn send_gateway_heartbeat(
        &self,
        socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    ) -> Result<()> {
        let sequence = *self
            .last_sequence
            .lock()
            .expect("qqbot sequence mutex poisoned");
        let payload = json!({
            "op": 1,
            "d": sequence,
        });
        socket
            .send(Message::Text(payload.to_string().into()))
            .context("failed to send QQ Bot Gateway heartbeat")
    }

    fn handle_gateway_event(&self, inbound: &GatewayInboundDispatch, text: &str) -> Result<()> {
        let event: Value =
            serde_json::from_str(text).context("failed to parse QQ Bot Gateway frame")?;
        if let Some(sequence) = event["s"].as_i64() {
            *self
                .last_sequence
                .lock()
                .expect("qqbot sequence mutex poisoned") = Some(sequence);
        }
        match event["op"].as_i64().unwrap_or(-1) {
            0 => {
                let event_type = first_str(&event, &["t"]).unwrap_or_default();
                if matches!(event_type, "READY" | "RESUMED") {
                    if let Some(session_id) = event["d"]["session_id"].as_str() {
                        *self
                            .session_id
                            .lock()
                            .expect("qqbot session id mutex poisoned") =
                            Some(session_id.to_string());
                    }
                    return Ok(());
                }
                if let Some(input) = self.event_to_inbound(&event)? {
                    inbound.submit(input)?;
                }
            }
            1 => {}
            7 => bail!("QQ Bot Gateway requested reconnect"),
            9 => {
                *self
                    .session_id
                    .lock()
                    .expect("qqbot session id mutex poisoned") = None;
                *self
                    .last_sequence
                    .lock()
                    .expect("qqbot sequence mutex poisoned") = None;
                bail!("QQ Bot Gateway invalid session")
            }
            10 | 11 => {}
            _ => {}
        }
        Ok(())
    }

    fn send_direct_text(&self, conversation_id: &str, text: &str) -> Result<()> {
        let token = self.fetch_access_token()?;
        let (scope, target_id) = qq_rest_target(conversation_id)?;
        let url = format!(
            "{}/v2/{}/{}/messages",
            self.api_base.trim_end_matches('/'),
            scope,
            target_id
        );
        for chunk in text_chunks(text) {
            let rendered = qq_outbound_text(&chunk, false);
            let mut body = json!({
                "msg_type": QQ_MSG_TYPE_TEXT,
                "content": rendered,
                "msg_seq": next_qq_msg_seq(),
            });
            if let Some(message_id) = self.last_message_id(conversation_id) {
                body["msg_id"] = json!(message_id);
            }
            let response = self
                .client
                .post(&url)
                .header("Authorization", format!("QQBot {token}"))
                .json(&body)
                .send()
                .context("QQ Bot direct REST send failed")?;
            let status = response.status();
            if !status.is_success() {
                let text = response.text().unwrap_or_default();
                bail!("QQ Bot direct REST send failed with status {status}: {text}");
            }
        }
        Ok(())
    }

    fn handle_qq_event(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        if !self.verify_webhook(&request) {
            return Ok(json_response(401, json!({"error": "unauthorized"})));
        }
        let value: Value =
            serde_json::from_slice(&request.body).context("failed to parse QQBot event JSON")?;
        let events = value["events"]
            .as_array()
            .or_else(|| value["messages"].as_array())
            .cloned()
            .unwrap_or_else(|| vec![value]);
        for event in &events {
            if let Some(input) = self.event_to_inbound(event)? {
                inbound.submit(input)?;
            }
        }
        Ok(json_response(200, json!({"ok": true})))
    }

    fn event_to_inbound(&self, event: &Value) -> Result<Option<InboundMessageInput>> {
        let body = qq_event_body(event);
        let is_interaction = qq_is_interaction_event(event, body);
        if !is_interaction && !qq_is_message_event(qq_event_type(event, body), body) {
            return Ok(None);
        }
        if is_interaction {
            if let Err(error) = self.acknowledge_interaction(event, body) {
                eprintln!("QQBot interaction ack skipped: {error:#}");
            }
        }
        let Some(conversation_id) = qq_conversation_id(body) else {
            if is_interaction {
                return Ok(None);
            }
            return Err(anyhow!(
                "QQBot event missing openid, group_openid, or channel_id"
            ));
        };
        let chat_type = qq_chat_type(body, &conversation_id);
        let sender_id = qq_sender_id(body);
        if !self.is_allowed(&conversation_id, sender_id.as_deref(), &chat_type) {
            return Ok(None);
        }
        let message_id = qq_message_id(event, body);
        if let Some(message_id) = message_id.as_deref() {
            if self.is_duplicate(message_id) {
                return Ok(None);
            }
        }
        if let Some(message_id) = message_id.as_deref() {
            self.remember_last_message_id(&conversation_id, message_id);
        }
        if is_interaction {
            let Some(command) = qq_interaction_command(body) else {
                return Ok(None);
            };
            return Ok(Some(InboundMessageInput {
                channel: QQBOT_CHANNEL.to_string(),
                conversation_id,
                thread_id: first_str(body, &["thread_id", "reply_to", "message_reference"])
                    .map(str::to_string),
                chat_type: Some(chat_type),
                sender_id,
                message_id,
                text: command,
                attachments: Vec::new(),
                timestamp: first_str(body, &["timestamp", "created_at", "time"])
                    .map(str::to_string),
            }));
        }
        let mut text = qq_message_text(body).unwrap_or_default();
        if matches!(chat_type.as_str(), "group" | "guild") {
            text = strip_at_mention(&text);
        }
        let attachments = self.parse_attachments(body);
        let asr_text = qq_asr_text(body);
        if let Some(asr_text) = asr_text {
            if text.trim().is_empty() {
                text = asr_text;
            } else {
                text.push_str("\n[QQ voice transcript]\n");
                text.push_str(&asr_text);
            }
        }
        if text.trim().is_empty() && attachments.is_empty() {
            return Ok(None);
        }
        Ok(Some(InboundMessageInput {
            channel: QQBOT_CHANNEL.to_string(),
            conversation_id,
            thread_id: first_str(body, &["thread_id", "reply_to", "message_reference"])
                .map(str::to_string),
            chat_type: Some(chat_type),
            sender_id,
            message_id,
            text: if text.trim().is_empty() {
                "[QQBot attachment]".to_string()
            } else {
                text
            },
            attachments,
            timestamp: first_str(body, &["timestamp", "created_at", "time"]).map(str::to_string),
        }))
    }

    fn parse_attachments(&self, event: &Value) -> Vec<InboundAttachmentInput> {
        let mut out = Vec::new();
        for attachment in qq_attachment_values(event) {
            if let Some(input) = qq_attachment_from_value(attachment) {
                out.push(input);
                continue;
            }
            if let Some(url) = first_str(
                attachment,
                &[
                    "url",
                    "download_url",
                    "media_url",
                    "file_url",
                    "image_url",
                    "voice_url",
                    "voice_wav_url",
                    "cdn_url",
                ],
            ) {
                match self.download_attachment(url, attachment) {
                    Ok(input) => out.push(input),
                    Err(error) => eprintln!("QQBot attachment skipped: {error:#}"),
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
        let response = request.send().context("QQBot attachment download failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("QQBot attachment download failed with status {status}");
        }
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or(value).to_string())
            .or_else(|| {
                first_str(
                    attachment,
                    &["mime", "mime_type", "content_type", "contentType"],
                )
                .map(str::to_string)
            });
        let bytes = response
            .bytes()
            .context("QQBot attachment body unreadable")?;
        if self.max_download_bytes > 0 && bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "QQBot attachment exceeds max_download_bytes ({})",
                self.max_download_bytes
            );
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: first_str(attachment, &["filename", "name", "file_name"])
                .map(str::to_string)
                .or_else(|| Some("qqbot-attachment.bin".to_string())),
            mime,
        })
    }

    fn is_allowed(&self, conversation_id: &str, sender_id: Option<&str>, chat_type: &str) -> bool {
        let normalized_chat = normalize_allow_entry(conversation_id);
        if !self.allowed_chats.is_empty()
            && !self.allowed_chats.contains("*")
            && !self.allowed_chats.contains(&normalized_chat)
        {
            return false;
        }
        let policy = if matches!(chat_type, "group" | "guild") {
            &self.group_policy
        } else {
            &self.dm_policy
        };
        if policy == "disabled" {
            return false;
        }
        if policy == "allowlist" {
            if !self.allowed_chats.is_empty() && self.allowed_chats.contains(&normalized_chat) {
                return true;
            }
            let Some(sender_id) = sender_id else {
                return false;
            };
            let normalized_sender = normalize_allow_entry(sender_id);
            return self.allowed_users.contains("*")
                || self.allowed_users.contains(&normalized_sender);
        }
        if let Some(sender_id) = sender_id {
            let normalized_sender = normalize_allow_entry(sender_id);
            self.allowed_users.is_empty()
                || self.allowed_users.contains("*")
                || self.allowed_users.contains(&normalized_sender)
        } else {
            self.allowed_users.is_empty()
        }
    }

    fn verify_webhook(&self, request: &ChannelHttpRequest) -> bool {
        let Some(secret) = self.webhook_secret.as_deref() else {
            return true;
        };
        let candidate = request
            .header("x-duckagent-gateway-secret")
            .or_else(|| request.header("x-qqbot-secret"))
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
            .expect("qqbot seen message ids mutex poisoned");
        if seen.iter().any(|existing| existing == message_id) {
            return true;
        }
        seen.push_back(message_id.to_string());
        while seen.len() > 1000 {
            seen.pop_front();
        }
        false
    }

    fn remember_last_message_id(&self, conversation_id: &str, message_id: &str) {
        if message_id.trim().is_empty() {
            return;
        }
        let mut last = self
            .last_message_ids
            .lock()
            .expect("qqbot last message ids mutex poisoned");
        last.insert(conversation_id.to_string(), message_id.to_string());
    }

    fn last_message_id(&self, conversation_id: &str) -> Option<String> {
        self.last_message_ids
            .lock()
            .expect("qqbot last message ids mutex poisoned")
            .get(conversation_id)
            .cloned()
    }

    fn acknowledge_interaction(&self, event: &Value, body: &Value) -> Result<()> {
        let Some(endpoint) = self.interaction_ack_endpoint.as_deref() else {
            return Ok(());
        };
        let Some(interaction_id) = qq_interaction_id(event, body) else {
            return Ok(());
        };
        self.post_bridge(
            endpoint,
            json!({
                "channel": QQBOT_CHANNEL,
                "interaction_id": interaction_id,
                "event_id": qq_message_id(event, body),
                "code": 0,
            }),
        )
    }

    fn post_bridge(&self, endpoint: &str, body: Value) -> Result<()> {
        let bridge_base = self
            .bridge_base
            .as_deref()
            .ok_or_else(|| anyhow!("qqbot channel requires bridge API URL"))?;
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
        let response = request.send().context("QQBot bridge POST failed")?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            bail!("QQBot bridge POST failed with status {status}: {text}");
        }
        Ok(())
    }
}

impl ChannelAdapter for QqBotAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        if self.direct_gateway {
            self.start_direct_gateway(inbound)?;
        }
        Ok(())
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        if !self.direct_gateway
            && request.method == "POST"
            && matches!(request.path.as_str(), "/qqbot/events" | "/qqbot/webhook")
        {
            return self.handle_qq_event(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        if self.direct_gateway {
            if !message.media_paths.is_empty() {
                bail!(
                    "QQ Bot direct_gateway text delivery is configured; media upload requires qqbot_bridge transport"
                );
            }
            return self.send_direct_text(&route.key.conversation_id, &message.text);
        }
        let conversation_id = route.key.conversation_id.as_str();
        let thread_id = route.key.thread_id.as_deref();
        let reply_to = message.reply_to.as_deref();
        let chat_type = qq_route_chat_type(conversation_id);
        for chunk in text_chunks(&message.text) {
            let rendered = qq_outbound_text(&chunk, self.markdown_support);
            let mut body = json!({
                "channel": QQBOT_CHANNEL,
                "conversation_id": conversation_id,
                "chat_type": chat_type,
                "thread_id": thread_id,
                "reply_to": reply_to,
                "text": rendered.clone(),
                "content": rendered.clone(),
                "format": if self.markdown_support { "markdown" } else { "text" },
                "msg_type": if self.markdown_support { QQ_MSG_TYPE_MARKDOWN } else { QQ_MSG_TYPE_TEXT },
                "media_paths": [],
            });
            if self.markdown_support {
                body["markdown"] = json!({"content": rendered});
            }
            self.post_bridge(&self.send_endpoint, body)?;
        }
        if !message.media_paths.is_empty() {
            self.post_bridge(
                &self.send_endpoint,
                json!({
                    "channel": QQBOT_CHANNEL,
                    "conversation_id": conversation_id,
                    "chat_type": chat_type,
                    "thread_id": thread_id,
                    "reply_to": reply_to,
                    "text": "",
                    "msg_type": QQ_MSG_TYPE_MEDIA,
                    "media_paths": &message.media_paths,
                    "media_mode": "qqbot_upload_or_url",
                }),
            )?;
        }
        Ok(())
    }

    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()> {
        if self.direct_gateway {
            let _ = (route, event);
            return Ok(());
        }
        let Some(endpoint) = self.typing_endpoint.as_deref() else {
            return Ok(());
        };
        let last_message_id = self.last_message_id(&route.key.conversation_id);
        self.post_bridge(
            endpoint,
            json!({
                "channel": QQBOT_CHANNEL,
                "conversation_id": route.key.conversation_id.as_str(),
                "chat_type": qq_route_chat_type(&route.key.conversation_id),
                "active": event.active,
                "reason": event.reason,
                "message_id": last_message_id.clone(),
                "msg_type": QQ_MSG_TYPE_INPUT_NOTIFY,
                "input_notify": {
                    "msg_id": last_message_id,
                    "duration_seconds": 60,
                },
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
        let rendered = qq_outbound_text(&approval_text, self.markdown_support);
        let mut body = json!({
            "channel": QQBOT_CHANNEL,
            "conversation_id": route.key.conversation_id.as_str(),
            "chat_type": qq_route_chat_type(&route.key.conversation_id),
            "thread_id": route.key.thread_id.as_deref(),
            "text": rendered.clone(),
            "content": rendered.clone(),
            "format": if self.markdown_support { "markdown" } else { "text" },
            "msg_type": if self.markdown_support { QQ_MSG_TYPE_MARKDOWN } else { QQ_MSG_TYPE_TEXT },
            "keyboard": qq_approval_keyboard(approval_id.as_str()),
        });
        if self.markdown_support {
            body["markdown"] = json!({"content": rendered});
        }
        if self.direct_gateway {
            return self.send_direct_text(&route.key.conversation_id, &approval_text);
        }
        self.post_bridge(&self.send_endpoint, body)
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: !self.direct_gateway,
            typing: !self.direct_gateway,
            approval_prompt: true,
        }
    }
}

pub(in crate::gateway::channels) fn new_adapter(
    config: &GatewayChannelConfig,
    credentials: &GatewayCredentialEntry,
) -> Result<QqBotAdapter> {
    QqBotAdapter::new(config, credentials)
}

fn qq_event_body(event: &Value) -> &Value {
    event
        .get("d")
        .or_else(|| event.get("data"))
        .or_else(|| event.get("payload"))
        .unwrap_or(event)
}

fn qq_event_type<'a>(event: &'a Value, body: &'a Value) -> Option<&'a str> {
    first_str(event, &["t", "event_type", "type"])
        .or_else(|| first_str(body, &["t", "event_type", "type"]))
}

fn qq_is_message_event(event_type: Option<&str>, body: &Value) -> bool {
    if let Some(event_type) = event_type {
        let normalized = event_type.trim().to_ascii_uppercase();
        if matches!(
            normalized.as_str(),
            "READY" | "RESUMED" | "RECONNECT" | "HEARTBEAT" | "HEARTBEAT_ACK"
        ) {
            return false;
        }
        if matches!(
            normalized.as_str(),
            "C2C_MESSAGE_CREATE"
                | "GROUP_AT_MESSAGE_CREATE"
                | "GROUP_MESSAGE_CREATE"
                | "DIRECT_MESSAGE_CREATE"
                | "GUILD_MESSAGE_CREATE"
                | "GUILD_AT_MESSAGE_CREATE"
                | "MESSAGE_CREATE"
        ) {
            return true;
        }
        if normalized.contains("MESSAGE_CREATE") {
            return true;
        }
    }
    qq_message_like(body)
}

fn qq_message_like(body: &Value) -> bool {
    qq_conversation_id(body).is_some()
        && (qq_message_text(body).is_some()
            || !qq_attachment_values(body).is_empty()
            || qq_asr_text(body).is_some())
}

fn qq_conversation_id(event: &Value) -> Option<String> {
    first_str(event, &["conversation_id", "chat_id"])
        .map(str::to_string)
        .or_else(|| {
            first_str(event, &["group_openid", "group_id"]).map(|value| format!("group:{value}"))
        })
        .or_else(|| first_str(event, &["channel_id"]).map(|value| format!("guild:{value}")))
        .or_else(|| first_str(event, &["guild_id"]).map(|value| format!("dm:{value}")))
        .or_else(|| {
            first_str(event, &["user_openid", "openid", "author_id"])
                .map(|value| format!("c2c:{value}"))
        })
}

fn qq_chat_type(event: &Value, conversation_id: &str) -> String {
    first_str(event, &["chat_type", "conversation_type", "type", "scene"])
        .map(|value| match value {
            "group" | "GROUP" | "group_at" | "GROUP_AT_MESSAGE_CREATE" | "GROUP_MESSAGE_CREATE" => {
                "group"
            }
            "guild" | "GUILD" | "channel" | "GUILD_MESSAGE_CREATE" | "GUILD_AT_MESSAGE_CREATE" => {
                "guild"
            }
            "dm" | "DIRECT" | "direct" | "DIRECT_MESSAGE_CREATE" => "dm",
            "c2c" | "C2C" | "C2C_MESSAGE_CREATE" => "c2c",
            _ => qq_route_chat_type(conversation_id),
        })
        .unwrap_or_else(|| qq_route_chat_type(conversation_id))
        .to_string()
}

fn qq_route_chat_type(conversation_id: &str) -> &'static str {
    if conversation_id.starts_with("group:") {
        "group"
    } else if conversation_id.starts_with("guild:") {
        "guild"
    } else if conversation_id.starts_with("dm:") {
        "dm"
    } else {
        "c2c"
    }
}

fn qq_sender_id(event: &Value) -> Option<String> {
    first_str(
        event,
        &[
            "sender_id",
            "user_id",
            "user_openid",
            "member_openid",
            "openid",
            "author_id",
        ],
    )
    .map(str::to_string)
    .or_else(|| event["author"]["id"].as_str().map(str::to_string))
    .or_else(|| {
        event["author"]["member_openid"]
            .as_str()
            .map(str::to_string)
    })
    .or_else(|| event["author"]["user_openid"].as_str().map(str::to_string))
    .or_else(|| event["member"]["user"]["id"].as_str().map(str::to_string))
}

fn qq_asr_text(event: &Value) -> Option<String> {
    for attachment in qq_attachment_values(event) {
        if let Some(text) = first_str(attachment, &["asr_refer_text", "transcript", "text"]) {
            if !text.trim().is_empty() {
                return Some(text.trim().to_string());
            }
        }
    }
    None
}

fn qq_is_interaction_event(event: &Value, body: &Value) -> bool {
    matches!(
        first_str(event, &["t", "event_type", "type"])
            .or_else(|| first_str(body, &["t", "event_type", "type"])),
        Some("INTERACTION_CREATE" | "interaction_create" | "interaction")
    ) || qq_button_data(body).is_some()
}

fn qq_button_data(body: &Value) -> Option<&str> {
    first_str(body, &["button_data", "buttonData"])
        .or_else(|| body.pointer("/data/button_data").and_then(Value::as_str))
        .or_else(|| body.pointer("/data/buttonData").and_then(Value::as_str))
        .or_else(|| {
            body.pointer("/data/resolved/button_data")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            body.pointer("/data/resolved/buttonData")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            body.pointer("/resolved/button_data")
                .and_then(Value::as_str)
        })
        .or_else(|| body.pointer("/resolved/buttonData").and_then(Value::as_str))
}

fn qq_interaction_id<'a>(event: &'a Value, body: &'a Value) -> Option<&'a str> {
    first_str(event, &["interaction_id", "id"])
        .or_else(|| first_str(body, &["interaction_id", "id"]))
        .or_else(|| body.pointer("/data/id").and_then(Value::as_str))
}

fn qq_interaction_command(body: &Value) -> Option<String> {
    let data = qq_button_data(body)?.trim();
    if data.starts_with("/approve ") || data.starts_with("/deny ") {
        return Some(data.to_string());
    }
    let rest = data.strip_prefix("approve:")?;
    let (approval_id, decision) = rest.rsplit_once(':')?;
    let command = match decision {
        "allow-once" | "once" => format!("/approve {approval_id} once"),
        "allow-always" | "always" => format!("/approve {approval_id} always"),
        "deny" => format!("/deny {approval_id}"),
        _ => return None,
    };
    Some(command)
}

fn qq_attachment_from_value(value: &Value) -> Option<InboundAttachmentInput> {
    if let Some(path) = first_str(value, &["path", "file_path", "local_path"]) {
        return Some(InboundAttachmentInput {
            bytes: None,
            path: Some(path.to_string()),
            filename: first_str(value, &["filename", "name", "file_name"]).map(str::to_string),
            mime: first_str(value, &["mime", "mime_type", "content_type", "contentType"])
                .map(str::to_string),
        });
    }
    if let Some(bytes) = first_str(value, &["bytes_base64", "base64"]) {
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(bytes) {
            return Some(InboundAttachmentInput {
                bytes: Some(decoded),
                path: None,
                filename: first_str(value, &["filename", "name", "file_name"]).map(str::to_string),
                mime: first_str(value, &["mime", "mime_type", "content_type", "contentType"])
                    .map(str::to_string),
            });
        }
    }
    None
}

fn qq_attachment_values(value: &Value) -> Vec<&Value> {
    let mut values = Vec::new();
    for key in ["attachments", "media", "files"] {
        if let Some(array) = value[key].as_array() {
            values.extend(array.iter());
        }
    }
    for key in ["attachment", "file", "image", "voice", "video"] {
        if value[key].is_object() {
            values.push(&value[key]);
        }
    }
    if let Some(elements) = value["msg_elements"].as_array() {
        for element in elements {
            for key in ["attachments", "media", "files"] {
                if let Some(array) = element[key].as_array() {
                    values.extend(array.iter());
                }
            }
            for key in ["attachment", "file", "image", "voice", "video"] {
                if element[key].is_object() {
                    values.push(&element[key]);
                }
            }
        }
    }
    values
}

fn qq_message_text(value: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(text) = first_str(value, &["content", "text", "message", "body"]) {
        if !text.trim().is_empty() {
            parts.push(text.trim().to_string());
        }
    }
    if let Some(elements) = value["msg_elements"].as_array() {
        for element in elements {
            if let Some(text) = first_str(element, &["content", "text", "message"]) {
                if !text.trim().is_empty() {
                    parts.push(text.trim().to_string());
                }
            }
            if let Some(text) = element["text"]["content"].as_str() {
                if !text.trim().is_empty() {
                    parts.push(text.trim().to_string());
                }
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn qq_message_id(event: &Value, body: &Value) -> Option<String> {
    first_str(body, &["message_id", "msg_id", "id", "event_id", "seq"])
        .or_else(|| first_str(event, &["message_id", "msg_id", "id", "event_id", "seq"]))
        .map(str::to_string)
}

fn strip_at_mention(content: &str) -> String {
    let trimmed = content.trim();
    if !trimmed.starts_with('@') {
        return trimmed.to_string();
    }
    trimmed
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim().to_string())
        .unwrap_or_default()
}

fn qq_outbound_text(text: &str, markdown_support: bool) -> String {
    if markdown_support {
        text.to_string()
    } else {
        strip_basic_markdown(text)
    }
}

fn strip_basic_markdown(text: &str) -> String {
    text.replace("```", "")
        .replace("**", "")
        .replace('`', "")
        .replace('*', "")
}

fn qq_approval_keyboard(approval_id: &str) -> Value {
    json!({
        "content": {
            "rows": [
                {
                    "buttons": [
                        qq_keyboard_button(
                            "allow_once",
                            "Once",
                            "Allowed",
                            format!("approve:{approval_id}:allow-once"),
                            1,
                        ),
                        qq_keyboard_button(
                            "allow_always",
                            "Always",
                            "Allowed",
                            format!("approve:{approval_id}:allow-always"),
                            1,
                        ),
                        qq_keyboard_button(
                            "deny",
                            "Deny",
                            "Denied",
                            format!("approve:{approval_id}:deny"),
                            0,
                        ),
                    ]
                }
            ]
        }
    })
}

fn qq_keyboard_button(
    id: &str,
    label: &str,
    visited_label: &str,
    data: String,
    style: i64,
) -> Value {
    json!({
        "id": id,
        "render_data": {
            "label": label,
            "visited_label": visited_label,
            "style": style,
        },
        "action": {
            "type": 1,
            "data": data,
            "permission": {"type": 2},
            "click_limit": 1,
        },
        "group_id": "approval",
    })
}

fn normalize_allow_entry(value: &str) -> String {
    let mut value = value.trim().to_ascii_lowercase();
    for prefix in ["qqbot:", "user:", "group:", "guild:", "dm:", "c2c:"] {
        if let Some(stripped) = value.strip_prefix(prefix) {
            value = stripped.to_string();
        }
    }
    value
}

fn first_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| value[*key].as_str())
}

fn endpoint_path(endpoint: &str) -> String {
    if endpoint.starts_with('/') {
        endpoint.to_string()
    } else {
        format!("/{endpoint}")
    }
}

fn set_qq_ws_read_timeout(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>) {
    let timeout = Some(QQ_GATEWAY_READ_TIMEOUT);
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => {
            let _ = stream.set_read_timeout(timeout);
        }
        MaybeTlsStream::Rustls(stream) => {
            let _ = stream.get_mut().set_read_timeout(timeout);
        }
        _ => {}
    }
}

fn read_qq_gateway_json(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    timeout: Duration,
) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            bail!("QQ Bot Gateway hello timed out");
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                return serde_json::from_str(text.as_str())
                    .context("failed to parse QQ Bot Gateway text frame");
            }
            Ok(Message::Binary(bytes)) => {
                return serde_json::from_slice(bytes.as_ref())
                    .context("failed to parse QQ Bot Gateway binary frame");
            }
            Ok(Message::Ping(bytes)) => {
                socket
                    .send(Message::Pong(bytes))
                    .context("failed to send QQ Bot Gateway hello pong")?;
            }
            Ok(Message::Close(frame)) => bail!("QQ Bot Gateway closed before hello: {frame:?}"),
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => return Err(error).context("QQ Bot Gateway hello read failed"),
        }
    }
}

fn qq_rest_target(conversation_id: &str) -> Result<(&'static str, String)> {
    if let Some(group_id) = conversation_id.strip_prefix("group:") {
        return Ok(("groups", group_id.to_string()));
    }
    if let Some(user_id) = conversation_id
        .strip_prefix("c2c:")
        .or_else(|| conversation_id.strip_prefix("direct:"))
        .or_else(|| conversation_id.strip_prefix("user:"))
    {
        return Ok(("users", user_id.to_string()));
    }
    if !conversation_id.contains(':') {
        return Ok(("users", conversation_id.to_string()));
    }
    bail!("QQ Bot direct REST currently supports c2c:<openid> and group:<group_openid> routes")
}

fn next_qq_msg_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

fn text_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if current.len() + character.len_utf8() > QQ_TEXT_LIMIT && !current.is_empty() {
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
    fn qqbot_group_conversation_is_prefixed() {
        assert_eq!(
            qq_conversation_id(&json!({"group_openid": "g1"})).as_deref(),
            Some("group:g1")
        );
    }

    #[test]
    fn qqbot_strip_at_mention_removes_first_token() {
        assert_eq!(strip_at_mention("@BotUser hello there"), "hello there");
    }

    #[test]
    fn qqbot_interaction_button_data_maps_to_approval_command() {
        let value = json!({
            "data": {
                "resolved": {
                    "button_data": "approve:abc123:allow-once"
                }
            }
        });
        assert_eq!(
            qq_interaction_command(&value).as_deref(),
            Some("/approve abc123 once")
        );
    }

    #[test]
    fn qqbot_adapter_supports_media_and_typing() -> Result<()> {
        let adapter = new_adapter(
            &GatewayChannelConfig::default(),
            &GatewayCredentialEntry {
                channel: "qqbot".to_string(),
                ..Default::default()
            },
        )?;
        let capabilities = adapter.capabilities();
        assert!(capabilities.media);
        assert!(capabilities.typing);
        Ok(())
    }
}
