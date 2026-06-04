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
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, connect};

const YUANBAO_CHANNEL: &str = "yuanbao";
const DEFAULT_SEND_ENDPOINT: &str = "/send";
const DEFAULT_TYPING_ENDPOINT: &str = "/typing";
const DEFAULT_WS_GATEWAY_URL: &str = "wss://bot-wss.yuanbao.tencent.com/wss/connection";
const DEFAULT_API_DOMAIN: &str = "https://bot.yuanbao.tencent.com";
const SIGN_TOKEN_PATH: &str = "/api/v5/robotLogic/sign-token";
const DIRECT_WS_READ_TIMEOUT: Duration = Duration::from_millis(500);
const DIRECT_WS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const DIRECT_WS_RECONNECT_DELAY: Duration = Duration::from_secs(5);
const YUANBAO_INSTANCE_ID: &str = "17";
const YUANBAO_TEXT_LIMIT: usize = 4_000;

#[derive(Debug)]
enum YuanbaoWsCommand {
    SendText {
        conversation_id: String,
        text: String,
    },
}

#[derive(Debug, Clone)]
struct YuanbaoSignToken {
    token: String,
    bot_id: String,
    source: String,
    route_env: String,
}

#[derive(Debug, Clone, Default)]
struct YuanbaoConnHead {
    cmd_type: u64,
    cmd: String,
    seq_no: u64,
    msg_id: String,
    module: String,
    need_ack: bool,
    status: u64,
}

#[derive(Debug, Clone)]
struct YuanbaoConnMsg {
    head: YuanbaoConnHead,
    data: Vec<u8>,
}

#[derive(Debug, Clone)]
enum ProtoValue {
    Varint(u64),
    Len(Vec<u8>),
    Fixed32(Vec<u8>),
    Fixed64(Vec<u8>),
}

#[derive(Clone)]
pub(in crate::gateway) struct YuanbaoAdapter {
    bridge_base: Option<String>,
    token: Option<String>,
    webhook_secret: Option<String>,
    allowed_users: HashSet<String>,
    allowed_chats: HashSet<String>,
    dm_policy: String,
    group_policy: String,
    bot_accounts: HashSet<String>,
    max_download_bytes: u64,
    send_endpoint: String,
    typing_endpoint: Option<String>,
    client: Client,
    seen_message_ids: Arc<Mutex<VecDeque<String>>>,
    direct_websocket: bool,
    app_id: Option<String>,
    app_secret: Option<String>,
    bot_id: Option<String>,
    ws_url: String,
    api_domain: String,
    route_env: Option<String>,
    direct_tx: Arc<Mutex<Option<mpsc::Sender<YuanbaoWsCommand>>>>,
}

impl YuanbaoAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .context("failed to build Yuanbao bridge HTTP client")?;
        let transport = config.transport.as_deref().unwrap_or("yuanbao_bridge");
        let direct_websocket = matches!(
            transport,
            "direct_websocket" | "websocket" | "yuanbao_websocket"
        );
        let ws_url = credentials
            .extra
            .get("ws_url")
            .cloned()
            .or_else(|| config.extra.get("ws_url").cloned())
            .unwrap_or_else(|| DEFAULT_WS_GATEWAY_URL.to_string());
        let api_domain = credentials
            .extra
            .get("api_domain")
            .cloned()
            .or_else(|| config.extra.get("api_domain").cloned())
            .or_else(|| config.api_base.clone())
            .unwrap_or_else(|| DEFAULT_API_DOMAIN.to_string());
        let bot_id = credentials
            .extra
            .get("bot_id")
            .cloned()
            .or_else(|| config.extra.get("bot_id").cloned());
        let route_env = credentials
            .extra
            .get("route_env")
            .cloned()
            .or_else(|| config.extra.get("route_env").cloned());
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
                .unwrap_or_else(|| "mention".to_string()),
            bot_accounts: config
                .extra
                .get("bot_accounts")
                .map(|value| {
                    value
                        .split(',')
                        .map(normalize_allow_entry)
                        .filter(|part| !part.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
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
            direct_websocket,
            app_id: credentials
                .app_id
                .clone()
                .or_else(|| config.extra.get("app_id").cloned()),
            app_secret: credentials
                .app_secret
                .clone()
                .or_else(|| config.extra.get("app_secret").cloned()),
            bot_id,
            ws_url,
            api_domain,
            route_env,
            direct_tx: Arc::new(Mutex::new(None)),
        })
    }

    fn start_direct_websocket(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        let app_id = self
            .app_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("yuanbao direct_websocket requires app_id"))?;
        let app_secret = self
            .app_secret
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("yuanbao direct_websocket requires app_secret"))?;
        if app_id.chars().any(char::is_whitespace) {
            bail!("yuanbao app_id must not contain whitespace");
        }
        if app_secret.trim().is_empty() {
            bail!("yuanbao app_secret is required");
        }
        let mut guard = self
            .direct_tx
            .lock()
            .expect("yuanbao direct tx mutex poisoned");
        if guard.is_some() {
            return Ok(());
        }
        let (tx, rx) = mpsc::channel();
        *guard = Some(tx);
        drop(guard);
        let adapter = self.clone();
        thread::Builder::new()
            .name("gateway-yuanbao-ws".to_string())
            .spawn(move || adapter.run_direct_websocket_loop(inbound, rx))
            .context("failed to start Yuanbao WebSocket thread")?;
        Ok(())
    }

    fn run_direct_websocket_loop(
        self,
        inbound: GatewayInboundDispatch,
        rx: mpsc::Receiver<YuanbaoWsCommand>,
    ) {
        loop {
            if let Err(error) = self.run_direct_websocket_session(&inbound, &rx) {
                eprintln!("Yuanbao WebSocket session ended: {error:#}");
            }
            thread::sleep(DIRECT_WS_RECONNECT_DELAY);
        }
    }

    fn run_direct_websocket_session(
        &self,
        inbound: &GatewayInboundDispatch,
        rx: &mpsc::Receiver<YuanbaoWsCommand>,
    ) -> Result<()> {
        let token = self.fetch_sign_token()?;
        let (mut socket, _) = connect(self.ws_url.as_str())
            .with_context(|| format!("failed to connect Yuanbao WebSocket {}", self.ws_url))?;
        set_ws_read_timeout(&mut socket);
        authenticate_yuanbao_ws(&mut socket, &token)?;
        let mut last_ping = Instant::now();
        loop {
            while let Ok(command) = rx.try_recv() {
                self.write_direct_command(&mut socket, &token, command)?;
            }
            if last_ping.elapsed() >= DIRECT_WS_HEARTBEAT_INTERVAL {
                let msg_id = uuid::Uuid::now_v7().to_string();
                socket
                    .send(Message::Binary(encode_ping(&msg_id)))
                    .context("failed to send Yuanbao ping")?;
                last_ping = Instant::now();
            }
            match socket.read() {
                Ok(Message::Binary(bytes)) => {
                    self.handle_direct_ws_frame(&mut socket, inbound, bytes.as_ref())?;
                }
                Ok(Message::Ping(bytes)) => {
                    socket
                        .send(Message::Pong(bytes))
                        .context("failed to send Yuanbao pong")?;
                }
                Ok(Message::Close(frame)) => {
                    bail!("Yuanbao WebSocket closed: {frame:?}");
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(error) => return Err(error).context("Yuanbao WebSocket read failed"),
            }
        }
    }

    fn fetch_sign_token(&self) -> Result<YuanbaoSignToken> {
        let app_id = self
            .app_id
            .as_deref()
            .ok_or_else(|| anyhow!("yuanbao direct_websocket requires app_id"))?;
        let app_secret = self
            .app_secret
            .as_deref()
            .ok_or_else(|| anyhow!("yuanbao direct_websocket requires app_secret"))?;
        let nonce = format!("{:032x}", rand::random::<u128>());
        let timestamp = yuanbao_beijing_timestamp();
        let signature = hmac_sha256_hex(
            app_secret.as_bytes(),
            format!("{nonce}{timestamp}{app_id}{app_secret}").as_bytes(),
        );
        let mut request = self
            .client
            .post(format!(
                "{}{}",
                self.api_domain.trim_end_matches('/'),
                SIGN_TOKEN_PATH
            ))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("X-AppVersion", env!("CARGO_PKG_VERSION"))
            .header("X-OperationSystem", std::env::consts::OS)
            .header("X-Instance-Id", YUANBAO_INSTANCE_ID)
            .header("X-Bot-Version", env!("CARGO_PKG_VERSION"))
            .json(&json!({
                "app_key": app_id,
                "nonce": nonce,
                "signature": signature,
                "timestamp": timestamp,
            }));
        if let Some(route_env) = self.route_env.as_deref().filter(|value| !value.is_empty()) {
            request = request.header("X-Route-Env", route_env);
        }
        let response = request
            .send()
            .context("Yuanbao sign-token request failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .with_context(|| format!("Yuanbao sign-token response was not JSON: {status}"))?;
        if !status.is_success() {
            bail!("Yuanbao sign-token request failed with status {status}: {value}");
        }
        let code = value["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            bail!(
                "Yuanbao sign-token failed: code={} msg={}",
                code,
                value["msg"].as_str().unwrap_or_default()
            );
        }
        let data = value
            .get("data")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("Yuanbao sign-token response missing data object"))?;
        let data_value = Value::Object(data.clone());
        let token = first_str(&data_value, &["token", "sign_token", "access_token"])
            .ok_or_else(|| anyhow!("Yuanbao sign-token response missing token"))?
            .to_string();
        let bot_id = first_str(&data_value, &["bot_id", "uid", "account"])
            .map(str::to_string)
            .or_else(|| self.bot_id.clone())
            .unwrap_or_default();
        Ok(YuanbaoSignToken {
            token,
            bot_id,
            source: first_str(&data_value, &["source"])
                .unwrap_or("bot")
                .to_string(),
            route_env: first_str(&data_value, &["route_env"])
                .map(str::to_string)
                .or_else(|| self.route_env.clone())
                .unwrap_or_default(),
        })
    }

    fn handle_direct_ws_frame(
        &self,
        socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
        inbound: &GatewayInboundDispatch,
        bytes: &[u8],
    ) -> Result<()> {
        let message = decode_conn_msg(bytes)?;
        if message.head.cmd_type == 2 {
            if message.head.need_ack {
                socket
                    .send(Message::Binary(encode_push_ack(&message.head)))
                    .context("failed to send Yuanbao push ack")?;
            }
            if !message.data.is_empty() {
                if let Some(event) = decode_inbound_push(&message.data)? {
                    if let Some(input) = self.event_to_inbound(&event)? {
                        inbound.submit(input)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn write_direct_command(
        &self,
        socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
        token: &YuanbaoSignToken,
        command: YuanbaoWsCommand,
    ) -> Result<()> {
        match command {
            YuanbaoWsCommand::SendText {
                conversation_id,
                text,
            } => {
                for chunk in text_chunks(&text) {
                    let msg_id = uuid::Uuid::now_v7().to_string();
                    let body = yuanbao_text_body(&chunk);
                    let bytes = if let Some(group_code) = conversation_id.strip_prefix("group:") {
                        encode_send_group_message(group_code, &token.bot_id, &msg_id, body)
                    } else {
                        let to_account = conversation_id
                            .strip_prefix("direct:")
                            .unwrap_or(conversation_id.as_str());
                        encode_send_c2c_message(to_account, &token.bot_id, &msg_id, body)
                    };
                    socket
                        .send(Message::Binary(bytes))
                        .context("failed to send Yuanbao WebSocket message")?;
                }
            }
        }
        Ok(())
    }

    fn send_direct_text(&self, conversation_id: &str, text: String) -> Result<()> {
        let sender = self
            .direct_tx
            .lock()
            .expect("yuanbao direct tx mutex poisoned")
            .clone()
            .ok_or_else(|| anyhow!("Yuanbao direct WebSocket is not connected yet"))?;
        sender
            .send(YuanbaoWsCommand::SendText {
                conversation_id: conversation_id.to_string(),
                text,
            })
            .context("failed to queue Yuanbao WebSocket message")
    }

    fn handle_yuanbao_event(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        if !self.verify_webhook(&request) {
            return Ok(json_response(401, json!({"error": "unauthorized"})));
        }
        let value: Value =
            serde_json::from_slice(&request.body).context("failed to parse Yuanbao event JSON")?;
        for event in yuanbao_event_items(&value) {
            if let Some(input) = self.event_to_inbound(event)? {
                inbound.submit(input)?;
            }
        }
        Ok(json_response(200, json!({"ok": true})))
    }

    fn event_to_inbound(&self, event: &Value) -> Result<Option<InboundMessageInput>> {
        let body = yuanbao_event_body(event);
        let event_type = yuanbao_event_type(event, body);
        if !yuanbao_is_message_event(event_type, body) {
            return Ok(None);
        }
        let conversation_id = yuanbao_conversation_id(body)
            .or_else(|| yuanbao_conversation_id(event))
            .ok_or_else(|| {
                anyhow!("Yuanbao event missing conversation id, direct account, or group code")
            })?;
        let sender_id = yuanbao_sender_id(body).or_else(|| yuanbao_sender_id(event));
        if !allowlist_contains(&self.allowed_chats, &conversation_id) {
            return Ok(None);
        }
        if let Some(sender_id) = sender_id {
            let normalized_sender = normalize_allow_entry(sender_id);
            if self.bot_accounts.contains(&normalized_sender) {
                return Ok(None);
            }
            if !allowlist_contains(&self.allowed_users, sender_id) {
                return Ok(None);
            }
        }
        if !self.policy_allows(
            body,
            &conversation_id,
            sender_id,
            yuanbao_chat_type(body, &conversation_id).as_str(),
        ) {
            return Ok(None);
        }
        let message_id = yuanbao_message_id(event, body);
        if let Some(message_id) = message_id.as_deref() {
            if self.is_duplicate(message_id) {
                return Ok(None);
            }
        }
        let chat_type = yuanbao_chat_type(body, &conversation_id);
        let mut text = yuanbao_text(body).unwrap_or_default();
        let mut attachments = self.parse_attachments(body)?;
        let element_text = self.parse_elements(body, &mut attachments);
        if !element_text.is_empty() {
            if !text.trim().is_empty() {
                text.push('\n');
            }
            text.push_str(&element_text);
        }
        if chat_type == "group" {
            text = strip_yuanbao_mention(&text, &self.bot_accounts);
        }
        if text.trim().is_empty() && attachments.is_empty() {
            return Ok(None);
        }
        Ok(Some(InboundMessageInput {
            channel: YUANBAO_CHANNEL.to_string(),
            conversation_id: conversation_id.clone(),
            thread_id: first_str(body, &["thread_id", "reply_to", "parent_id"]).map(str::to_string),
            chat_type: Some(chat_type),
            sender_id: sender_id.map(str::to_string),
            message_id,
            text: if text.trim().is_empty() {
                "[Yuanbao attachment]".to_string()
            } else {
                text
            },
            attachments,
            timestamp: first_str(body, &["timestamp", "created_at", "time"]).map(str::to_string),
        }))
    }

    fn policy_allows(
        &self,
        event: &Value,
        conversation_id: &str,
        sender_id: Option<&str>,
        chat_type: &str,
    ) -> bool {
        let policy = if chat_type == "group" {
            self.group_policy.as_str()
        } else {
            self.dm_policy.as_str()
        };
        match policy {
            "disabled" => false,
            "allowlist" => {
                allowlist_contains(&self.allowed_chats, conversation_id)
                    || sender_id
                        .is_some_and(|sender| allowlist_contains(&self.allowed_users, sender))
            }
            "mention" if chat_type == "group" => yuanbao_mentions_bot(event, &self.bot_accounts),
            _ => true,
        }
    }

    fn parse_elements(
        &self,
        event: &Value,
        attachments: &mut Vec<InboundAttachmentInput>,
    ) -> String {
        let mut parts = Vec::new();
        for element in event["elements"]
            .as_array()
            .or_else(|| event["msg_elements"].as_array())
            .or_else(|| event["msg_body"].as_array())
            .into_iter()
            .flatten()
        {
            let elem_type = first_str(
                element,
                &["type", "elem_type", "msg_type", "message_type", "name"],
            )
            .unwrap_or_default();
            match elem_type {
                "text" | "TIMTextElem" => {
                    if let Some(text) = yuanbao_text(element) {
                        parts.push(text.to_string());
                    }
                }
                "sticker" | "face" | "TIMFaceElem" => {
                    let name = first_str(element, &["name", "sticker_name", "description"])
                        .map(str::to_string)
                        .or_else(|| element["msg_content"]["data"].as_str().map(str::to_string))
                        .or_else(|| {
                            yuanbao_json_content(element).and_then(|value| {
                                first_str(&value, &["data", "text", "content"]).map(str::to_string)
                            })
                        })
                        .unwrap_or_else(|| "sticker".to_string());
                    parts.push(format!("[Yuanbao sticker: {name}]"));
                }
                "image" | "file" | "voice" | "video" | "TIMImageElem" | "TIMFileElem" => {
                    if let Some(input) = attachment_from_value(element) {
                        attachments.push(input);
                    }
                    let resource_id =
                        first_str(element, &["resource_id", "res_id", "file_id", "uuid"])
                            .unwrap_or_default();
                    if !resource_id.is_empty() {
                        let kind = if elem_type.contains("Image") {
                            "image"
                        } else {
                            elem_type
                        };
                        parts.push(format!("[{kind}|ybres:{resource_id}]"));
                    }
                }
                _ => {}
            }
        }
        parts.join("\n")
    }

    fn parse_attachments(&self, event: &Value) -> Result<Vec<InboundAttachmentInput>> {
        let mut out = Vec::new();
        for attachment in yuanbao_attachment_values(event) {
            if let Some(input) = attachment_from_value(attachment) {
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
                    Err(error) => eprintln!("Yuanbao attachment skipped: {error:#}"),
                }
            }
        }
        Ok(out)
    }

    fn download_attachment(&self, url: &str, attachment: &Value) -> Result<InboundAttachmentInput> {
        let mut request = self.client.get(url);
        if let Some(token) = self.token.as_deref() {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .context("Yuanbao attachment download failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("Yuanbao attachment download failed with status {status}");
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
            .context("Yuanbao attachment body unreadable")?;
        if self.max_download_bytes > 0 && bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "Yuanbao attachment exceeds max_download_bytes ({})",
                self.max_download_bytes
            );
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: first_str(attachment, &["filename", "name", "file_name", "fileName"])
                .map(str::to_string)
                .or_else(|| Some("yuanbao-attachment.bin".to_string())),
            mime,
        })
    }

    fn verify_webhook(&self, request: &ChannelHttpRequest) -> bool {
        let Some(secret) = self.webhook_secret.as_deref() else {
            return true;
        };
        let candidate = request
            .header("x-duckagent-gateway-secret")
            .or_else(|| request.header("x-yuanbao-secret"))
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
            .expect("yuanbao seen message ids mutex poisoned");
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
            .ok_or_else(|| anyhow!("yuanbao channel requires bridge API URL"))?;
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
        let response = request.send().context("Yuanbao bridge POST failed")?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            bail!("Yuanbao bridge POST failed with status {status}: {text}");
        }
        Ok(())
    }
}

impl ChannelAdapter for YuanbaoAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        if self.direct_websocket {
            self.start_direct_websocket(inbound)?;
        }
        Ok(())
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        if !self.direct_websocket
            && request.method == "POST"
            && matches!(
                request.path.as_str(),
                "/yuanbao/events" | "/yuanbao/webhook"
            )
        {
            return self.handle_yuanbao_event(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        if self.direct_websocket {
            if !message.media_paths.is_empty() {
                bail!(
                    "Yuanbao direct_websocket text delivery is configured; media upload requires yuanbao_bridge transport"
                );
            }
            return self.send_direct_text(&route.key.conversation_id, message.text);
        }
        let conversation_id = route.key.conversation_id.as_str();
        let thread_id = route.key.thread_id.as_deref();
        let reply_to = message.reply_to.as_deref();
        for chunk in text_chunks(&message.text) {
            self.post_bridge(
                &self.send_endpoint,
                json!({
                    "channel": YUANBAO_CHANNEL,
                    "conversation_id": conversation_id,
                    "chat_type": yuanbao_route_chat_type(conversation_id),
                    "thread_id": thread_id,
                    "reply_to": reply_to,
                    "text": chunk,
                    "media_paths": [],
                    "elements": [{"type": "text", "text": chunk}],
                }),
            )?;
        }
        if !message.media_paths.is_empty() {
            self.post_bridge(
                &self.send_endpoint,
                json!({
                    "channel": YUANBAO_CHANNEL,
                    "conversation_id": conversation_id,
                    "chat_type": yuanbao_route_chat_type(conversation_id),
                    "thread_id": thread_id,
                    "reply_to": reply_to,
                    "text": "",
                    "media_paths": &message.media_paths,
                    "media_mode": "yuanbao_cos_or_proto_bridge",
                }),
            )?;
        }
        Ok(())
    }

    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()> {
        if self.direct_websocket {
            let _ = (route, event);
            return Ok(());
        }
        let Some(endpoint) = self.typing_endpoint.as_deref() else {
            return Ok(());
        };
        self.post_bridge(
            endpoint,
            json!({
                "channel": YUANBAO_CHANNEL,
                "conversation_id": route.key.conversation_id.as_str(),
                "chat_type": yuanbao_route_chat_type(&route.key.conversation_id),
                "thread_id": route.key.thread_id.as_deref(),
                "active": event.active,
                "reason": event.reason,
                "heartbeat": if event.active { "RUNNING" } else { "FINISH" },
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
        if self.direct_websocket {
            return self.send_direct_text(&route.key.conversation_id, approval_text);
        }
        self.post_bridge(
            &self.send_endpoint,
            json!({
                "channel": YUANBAO_CHANNEL,
                "conversation_id": route.key.conversation_id.as_str(),
                "chat_type": yuanbao_route_chat_type(&route.key.conversation_id),
                "thread_id": route.key.thread_id.as_deref(),
                "text": approval_text,
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
            media: !self.direct_websocket,
            typing: !self.direct_websocket,
            approval_prompt: true,
        }
    }
}

pub(in crate::gateway::channels) fn new_adapter(
    config: &GatewayChannelConfig,
    credentials: &GatewayCredentialEntry,
) -> Result<YuanbaoAdapter> {
    YuanbaoAdapter::new(config, credentials)
}

fn yuanbao_conversation_id(event: &Value) -> Option<String> {
    first_str(
        event,
        &[
            "conversation_id",
            "chat_id",
            "room_id",
            "peer_id",
            "to",
            "target",
        ],
    )
    .map(str::to_string)
    .or_else(|| first_str(event, &["group_code", "group_id"]).map(|value| format!("group:{value}")))
    .or_else(|| {
        first_str(
            event,
            &[
                "direct_account",
                "account",
                "user_account",
                "from_account",
                "sender_account",
            ],
        )
        .map(|value| format!("direct:{value}"))
    })
}

fn yuanbao_event_items(value: &Value) -> Vec<&Value> {
    for path in [
        "/events",
        "/messages",
        "/data/events",
        "/data/messages",
        "/payload/events",
        "/payload/messages",
    ] {
        if let Some(array) = value.pointer(path).and_then(Value::as_array) {
            return array.iter().collect();
        }
    }
    vec![value]
}

fn yuanbao_event_body(mut value: &Value) -> &Value {
    for _ in 0..4 {
        let Some(next) = value
            .get("data")
            .or_else(|| value.get("payload"))
            .or_else(|| value.get("event"))
            .or_else(|| value.get("message"))
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

fn yuanbao_event_type<'a>(event: &'a Value, body: &'a Value) -> Option<&'a str> {
    first_str(event, &["event_type", "type", "cmd", "command", "t"])
        .or_else(|| first_str(body, &["event_type", "type", "cmd", "command", "t"]))
}

fn yuanbao_is_message_event(event_type: Option<&str>, body: &Value) -> bool {
    if let Some(event_type) = event_type {
        let normalized = event_type.trim().to_ascii_uppercase();
        if matches!(
            normalized.as_str(),
            "READY" | "AUTH_BIND" | "HEARTBEAT" | "PING" | "PONG" | "ACK" | "CONNECT"
        ) {
            return false;
        }
        if matches!(
            normalized.as_str(),
            "T05"
                | "MESSAGE"
                | "MESSAGE_RECEIVE"
                | "MESSAGE_CREATE"
                | "GROUP_MESSAGE"
                | "DIRECT_MESSAGE"
        ) {
            return true;
        }
    }
    yuanbao_conversation_id(body).is_some()
        && (yuanbao_text(body).is_some()
            || !yuanbao_attachment_values(body).is_empty()
            || body["elements"].is_array()
            || body["msg_elements"].is_array()
            || body["msg_body"].is_array())
}

fn yuanbao_sender_id(event: &Value) -> Option<&str> {
    first_str(
        event,
        &[
            "sender_id",
            "user_id",
            "from",
            "from_account",
            "sender_account",
            "user_account",
            "member_account",
            "uin",
            "uid",
        ],
    )
    .or_else(|| event["sender"]["account"].as_str())
    .or_else(|| event["sender"]["id"].as_str())
    .or_else(|| event["from"]["account"].as_str())
}

fn yuanbao_message_id(event: &Value, body: &Value) -> Option<String> {
    first_str(
        body,
        &["message_id", "msg_id", "msg_seq", "event_id", "id", "seq"],
    )
    .or_else(|| {
        first_str(
            event,
            &["message_id", "msg_id", "msg_seq", "event_id", "id", "seq"],
        )
    })
    .map(str::to_string)
}

fn yuanbao_chat_type(event: &Value, conversation_id: &str) -> String {
    first_str(event, &["chat_type", "conversation_type", "type"])
        .map(|value| {
            if matches!(value, "group" | "GROUP" | "group_chat") {
                "group"
            } else {
                yuanbao_route_chat_type(conversation_id)
            }
        })
        .unwrap_or_else(|| yuanbao_route_chat_type(conversation_id))
        .to_string()
}

fn yuanbao_route_chat_type(conversation_id: &str) -> &'static str {
    if conversation_id.starts_with("group:") {
        "group"
    } else {
        "direct"
    }
}

fn yuanbao_mentions_bot(event: &Value, bot_accounts: &HashSet<String>) -> bool {
    for key in ["is_at_bot", "at_bot", "mentioned_bot", "mention_bot"] {
        if event[key].as_bool().unwrap_or(false) {
            return true;
        }
    }
    let text = yuanbao_text(event).unwrap_or_default();
    let lowered_text = text.to_ascii_lowercase();
    if text.trim_start().starts_with('@') {
        if bot_accounts.is_empty() {
            return true;
        }
        if bot_accounts
            .iter()
            .any(|account| lowered_text.contains(account))
        {
            return true;
        }
    }
    for element in event["elements"]
        .as_array()
        .or_else(|| event["msg_elements"].as_array())
        .or_else(|| event["msg_body"].as_array())
        .into_iter()
        .flatten()
    {
        let elem_type = first_str(
            element,
            &["type", "elem_type", "msg_type", "message_type", "name"],
        )
        .unwrap_or_default();
        let mentioned = first_str(
            element,
            &[
                "account",
                "user_account",
                "uin",
                "uid",
                "text",
                "content",
                "name",
            ],
        )
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
        let normalized = normalize_allow_entry(&mentioned);
        if matches!(elem_type, "at" | "mention" | "TIMMentionElem" | "TIMAtElem")
            && (bot_accounts.is_empty() || bot_accounts.contains(&normalized))
        {
            return true;
        }
    }
    false
}

fn yuanbao_text(value: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(text) = first_str(
        value,
        &["text", "message", "body", "content", "msg_content"],
    ) {
        if !text.trim().is_empty() {
            if let Some(parsed_text) = yuanbao_text_from_json_string(text) {
                parts.push(parsed_text);
            } else {
                parts.push(text.trim().to_string());
            }
        }
    }
    if let Some(json_content) = yuanbao_json_content(value) {
        if let Some(text) = first_str(&json_content, &["text", "content", "message"]) {
            if !text.trim().is_empty() {
                parts.push(text.trim().to_string());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn yuanbao_text_from_json_string(raw: &str) -> Option<String> {
    if !raw.trim_start().starts_with('{') {
        return None;
    }
    let value: Value = serde_json::from_str(raw).ok()?;
    first_str(&value, &["text", "content", "message"]).map(|text| text.trim().to_string())
}

fn yuanbao_json_content(value: &Value) -> Option<Value> {
    if value["msg_content"].is_object() {
        return Some(value["msg_content"].clone());
    }
    let raw = value["msg_content"].as_str()?;
    serde_json::from_str(raw).ok()
}

fn yuanbao_attachment_values(value: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    for key in ["attachments", "media", "files"] {
        if let Some(array) = value[key].as_array() {
            out.extend(array.iter());
        }
    }
    for key in ["attachment", "file", "image", "voice", "video"] {
        if value[key].is_object() {
            out.push(&value[key]);
        }
    }
    for key in ["elements", "msg_elements", "msg_body"] {
        if let Some(elements) = value[key].as_array() {
            for element in elements {
                for nested_key in ["attachments", "media", "files"] {
                    if let Some(array) = element[nested_key].as_array() {
                        out.extend(array.iter());
                    }
                }
                for nested_key in ["attachment", "file", "image", "voice", "video"] {
                    if element[nested_key].is_object() {
                        out.push(&element[nested_key]);
                    }
                }
            }
        }
    }
    out
}

fn attachment_from_value(value: &Value) -> Option<InboundAttachmentInput> {
    if let Some(path) = first_str(value, &["path", "file_path", "local_path"]) {
        return Some(InboundAttachmentInput {
            bytes: None,
            path: Some(path.to_string()),
            filename: first_str(value, &["filename", "name", "file_name", "fileName"])
                .map(str::to_string),
            mime: first_str(value, &["mime", "mime_type", "content_type", "contentType"])
                .map(str::to_string),
        });
    }
    if let Some(bytes) = first_str(value, &["bytes_base64", "base64"]) {
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(bytes) {
            return Some(InboundAttachmentInput {
                bytes: Some(decoded),
                path: None,
                filename: first_str(value, &["filename", "name", "file_name", "fileName"])
                    .map(str::to_string),
                mime: first_str(value, &["mime", "mime_type", "content_type", "contentType"])
                    .map(str::to_string),
            });
        }
    }
    None
}

fn strip_yuanbao_mention(text: &str, bot_accounts: &HashSet<String>) -> String {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('@') {
        return text.trim().to_string();
    }
    let Some((first, rest)) = trimmed.split_once(char::is_whitespace) else {
        return String::new();
    };
    let mentioned = normalize_allow_entry(
        first
            .trim_start_matches('@')
            .trim_end_matches(|character| character == ':' || character == ','),
    );
    if bot_accounts.is_empty() || bot_accounts.contains(&mentioned) {
        return rest.trim().to_string();
    }
    text.trim().to_string()
}

fn allowlist_contains(allowlist: &HashSet<String>, candidate: &str) -> bool {
    if allowlist.is_empty() || allowlist.contains("*") {
        return true;
    }
    let normalized = normalize_allow_entry(candidate);
    allowlist.contains(&normalized)
}

fn normalize_allow_entry(value: &str) -> String {
    let mut normalized = value.trim().to_ascii_lowercase();
    for prefix in [
        "yuanbao:", "direct:", "group:", "user:", "member:", "account:",
    ] {
        if let Some(rest) = normalized.strip_prefix(prefix) {
            normalized = rest.to_string();
            break;
        }
    }
    normalized
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

fn set_ws_read_timeout(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>) {
    let timeout = Some(DIRECT_WS_READ_TIMEOUT);
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

fn authenticate_yuanbao_ws(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    token: &YuanbaoSignToken,
) -> Result<String> {
    let msg_id = uuid::Uuid::now_v7().to_string();
    socket
        .send(Message::Binary(encode_auth_bind(token, &msg_id)))
        .context("failed to send Yuanbao auth-bind")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if Instant::now() >= deadline {
            bail!("Yuanbao auth-bind timed out");
        }
        match socket.read() {
            Ok(Message::Binary(bytes)) => {
                let message = decode_conn_msg(bytes.as_ref())?;
                if message.head.cmd_type == 1 && message.head.cmd == "auth-bind" {
                    return decode_auth_bind_connect_id(&message.data);
                }
            }
            Ok(Message::Ping(bytes)) => {
                socket
                    .send(Message::Pong(bytes))
                    .context("failed to send Yuanbao auth pong")?;
            }
            Ok(Message::Close(frame)) => bail!("Yuanbao WebSocket closed during auth: {frame:?}"),
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => return Err(error).context("Yuanbao WebSocket auth read failed"),
        }
    }
}

fn yuanbao_text_body(text: &str) -> Vec<Value> {
    vec![json!({
        "msg_type": "TIMTextElem",
        "msg_content": {"text": text},
    })]
}

fn yuanbao_beijing_timestamp() -> String {
    let offset = chrono::FixedOffset::east_opt(8 * 3600).expect("valid Beijing UTC offset");
    chrono::Utc::now()
        .with_timezone(&offset)
        .format("%Y-%m-%dT%H:%M:%S+08:00")
        .to_string()
}

fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    const BLOCK_SIZE: usize = 64;
    let mut key_block = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let digest = Sha256::digest(key);
        key_block[..digest.len()].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK_SIZE];
    let mut opad = [0x5cu8; BLOCK_SIZE];
    for index in 0..BLOCK_SIZE {
        ipad[index] ^= key_block[index];
        opad[index] ^= key_block[index];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    hex_lower(&outer.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn encode_auth_bind(token: &YuanbaoSignToken, msg_id: &str) -> Vec<u8> {
    let auth = [
        encode_field(1, 2, encode_string(&token.bot_id)),
        encode_field(2, 2, encode_string(&token.source)),
        encode_field(3, 2, encode_string(&token.token)),
    ]
    .concat();
    let device = [
        encode_field(1, 2, encode_string(env!("CARGO_PKG_VERSION"))),
        encode_field(2, 2, encode_string(std::env::consts::OS)),
        encode_field(10, 2, encode_string(YUANBAO_INSTANCE_ID)),
        encode_field(24, 2, encode_string(env!("CARGO_PKG_VERSION"))),
    ]
    .concat();
    let mut body = [
        encode_field(1, 2, encode_string("ybBot")),
        encode_field(2, 2, encode_message(auth)),
        encode_field(3, 2, encode_message(device)),
    ]
    .concat();
    if !token.route_env.is_empty() {
        body.extend(encode_field(5, 2, encode_string(&token.route_env)));
    }
    encode_conn_msg_full(0, "auth-bind", msg_id, "conn_access", body, false)
}

fn encode_ping(msg_id: &str) -> Vec<u8> {
    encode_conn_msg_full(0, "ping", msg_id, "conn_access", Vec::new(), false)
}

fn encode_push_ack(head: &YuanbaoConnHead) -> Vec<u8> {
    encode_conn_msg_full(3, &head.cmd, &head.msg_id, &head.module, Vec::new(), false)
}

fn encode_send_c2c_message(
    to_account: &str,
    from_account: &str,
    msg_id: &str,
    msg_body: Vec<Value>,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(encode_field(1, 2, encode_string(msg_id)));
    body.extend(encode_field(2, 2, encode_string(to_account)));
    if !from_account.is_empty() {
        body.extend(encode_field(3, 2, encode_string(from_account)));
    }
    for element in msg_body {
        body.extend(encode_field(
            5,
            2,
            encode_message(encode_msg_body_element(&element)),
        ));
    }
    encode_conn_msg_full(
        0,
        "send_c2c_message",
        msg_id,
        "yuanbao_openclaw_proxy",
        body,
        false,
    )
}

fn encode_send_group_message(
    group_code: &str,
    from_account: &str,
    msg_id: &str,
    msg_body: Vec<Value>,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(encode_field(1, 2, encode_string(msg_id)));
    body.extend(encode_field(2, 2, encode_string(group_code)));
    if !from_account.is_empty() {
        body.extend(encode_field(3, 2, encode_string(from_account)));
    }
    for element in msg_body {
        body.extend(encode_field(
            6,
            2,
            encode_message(encode_msg_body_element(&element)),
        ));
    }
    encode_conn_msg_full(
        0,
        "send_group_message",
        msg_id,
        "yuanbao_openclaw_proxy",
        body,
        false,
    )
}

fn encode_msg_body_element(element: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(msg_type) = first_str(element, &["msg_type", "type"]) {
        out.extend(encode_field(1, 2, encode_string(msg_type)));
    }
    if let Some(content) = element
        .get("msg_content")
        .or_else(|| element.get("content"))
    {
        out.extend(encode_field(
            2,
            2,
            encode_message(encode_msg_content(content)),
        ));
    }
    out
}

fn encode_msg_content(content: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    for (field, key) in [
        (1, "text"),
        (2, "uuid"),
        (4, "data"),
        (5, "desc"),
        (6, "ext"),
        (7, "sound"),
        (10, "url"),
        (12, "file_name"),
    ] {
        if let Some(value) = first_str(content, &[key]) {
            out.extend(encode_field(field, 2, encode_string(value)));
        }
    }
    out
}

fn encode_conn_msg_full(
    cmd_type: u64,
    cmd: &str,
    msg_id: &str,
    module: &str,
    data: Vec<u8>,
    need_ack: bool,
) -> Vec<u8> {
    let seq_no = next_seq_no();
    let mut head = Vec::new();
    if cmd_type != 0 {
        head.extend(encode_field(1, 0, encode_varint(cmd_type)));
    }
    if !cmd.is_empty() {
        head.extend(encode_field(2, 2, encode_string(cmd)));
    }
    if seq_no != 0 {
        head.extend(encode_field(3, 0, encode_varint(seq_no)));
    }
    if !msg_id.is_empty() {
        head.extend(encode_field(4, 2, encode_string(msg_id)));
    }
    if !module.is_empty() {
        head.extend(encode_field(5, 2, encode_string(module)));
    }
    if need_ack {
        head.extend(encode_field(6, 0, encode_varint(1)));
    }
    let mut out = encode_field(1, 2, encode_message(head));
    if !data.is_empty() {
        out.extend(encode_field(2, 2, encode_bytes(data)));
    }
    out
}

fn next_seq_no() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

fn decode_conn_msg(bytes: &[u8]) -> Result<YuanbaoConnMsg> {
    let fields = fields_to_map(parse_fields(bytes)?);
    let head_bytes = get_bytes(&fields, 1).unwrap_or_default();
    let data = get_bytes(&fields, 2).unwrap_or_default();
    let head = if head_bytes.is_empty() {
        YuanbaoConnHead::default()
    } else {
        decode_conn_head(&head_bytes)?
    };
    Ok(YuanbaoConnMsg { head, data })
}

fn decode_conn_head(bytes: &[u8]) -> Result<YuanbaoConnHead> {
    let fields = fields_to_map(parse_fields(bytes)?);
    Ok(YuanbaoConnHead {
        cmd_type: get_varint(&fields, 1).unwrap_or_default(),
        cmd: get_string(&fields, 2).unwrap_or_default(),
        seq_no: get_varint(&fields, 3).unwrap_or_default(),
        msg_id: get_string(&fields, 4).unwrap_or_default(),
        module: get_string(&fields, 5).unwrap_or_default(),
        need_ack: get_varint(&fields, 6).unwrap_or_default() != 0,
        status: get_varint(&fields, 10).unwrap_or_default(),
    })
}

fn decode_auth_bind_connect_id(bytes: &[u8]) -> Result<String> {
    let fields = fields_to_map(parse_fields(bytes)?);
    let code = get_varint(&fields, 1).unwrap_or_default();
    if code != 0 {
        bail!(
            "Yuanbao auth-bind failed: code={} message={}",
            code,
            get_string(&fields, 2).unwrap_or_default()
        );
    }
    get_string(&fields, 3).ok_or_else(|| anyhow!("Yuanbao auth-bind response missing connect id"))
}

fn decode_inbound_push(bytes: &[u8]) -> Result<Option<Value>> {
    let fields = fields_to_map(parse_fields(bytes)?);
    let mut msg_body = Vec::new();
    for element in get_repeated_bytes(&fields, 13) {
        msg_body.push(decode_msg_body_element(&element)?);
    }
    let mut value = json!({
        "callback_command": get_string(&fields, 1).unwrap_or_default(),
        "from_account": get_string(&fields, 2).unwrap_or_default(),
        "to_account": get_string(&fields, 3).unwrap_or_default(),
        "sender_nickname": get_string(&fields, 4).unwrap_or_default(),
        "group_id": get_string(&fields, 5).unwrap_or_default(),
        "group_code": get_string(&fields, 6).unwrap_or_default(),
        "group_name": get_string(&fields, 7).unwrap_or_default(),
        "msg_seq": get_varint(&fields, 8).unwrap_or_default(),
        "msg_random": get_varint(&fields, 9).unwrap_or_default(),
        "msg_time": get_varint(&fields, 10).unwrap_or_default(),
        "msg_key": get_string(&fields, 11).unwrap_or_default(),
        "msg_id": get_string(&fields, 12).unwrap_or_default(),
        "msg_body": msg_body,
        "cloud_custom_data": get_string(&fields, 14).unwrap_or_default(),
        "event_time": get_varint(&fields, 15).unwrap_or_default(),
        "bot_owner_id": get_string(&fields, 16).unwrap_or_default(),
        "claw_msg_type": get_varint(&fields, 18).unwrap_or_default(),
        "private_from_group_code": get_string(&fields, 19).unwrap_or_default(),
    });
    if let Some(trace_bytes) = get_bytes(&fields, 20) {
        let trace_fields = fields_to_map(parse_fields(&trace_bytes)?);
        if let Some(trace_id) = get_string(&trace_fields, 1) {
            value["trace_id"] = Value::String(trace_id);
        }
    }
    let empty_body = value["msg_body"]
        .as_array()
        .map_or(true, |items| items.is_empty());
    if empty_body && first_str(&value, &["from_account", "group_code", "msg_id"]).is_none() {
        return Ok(None);
    }
    Ok(Some(value))
}

fn decode_msg_body_element(bytes: &[u8]) -> Result<Value> {
    let fields = fields_to_map(parse_fields(bytes)?);
    let content = get_bytes(&fields, 2)
        .map(|bytes| decode_msg_content(&bytes))
        .transpose()?
        .unwrap_or_else(|| json!({}));
    Ok(json!({
        "msg_type": get_string(&fields, 1).unwrap_or_default(),
        "msg_content": content,
    }))
}

fn decode_msg_content(bytes: &[u8]) -> Result<Value> {
    let fields = fields_to_map(parse_fields(bytes)?);
    let mut map = serde_json::Map::new();
    for (field, key) in [
        (1, "text"),
        (2, "uuid"),
        (4, "data"),
        (5, "desc"),
        (6, "ext"),
        (7, "sound"),
        (10, "url"),
        (12, "file_name"),
    ] {
        if let Some(value) = get_string(&fields, field) {
            map.insert(key.to_string(), Value::String(value));
        }
    }
    for (field, key) in [(3, "image_format"), (9, "index"), (11, "file_size")] {
        if let Some(value) = get_varint(&fields, field) {
            if value != 0 {
                map.insert(key.to_string(), json!(value));
            }
        }
    }
    Ok(Value::Object(map))
}

fn encode_field(field_number: u64, wire_type: u8, value: Vec<u8>) -> Vec<u8> {
    let mut out = encode_varint((field_number << 3) | u64::from(wire_type));
    out.extend(value);
    out
}

fn encode_string(value: &str) -> Vec<u8> {
    encode_bytes(value.as_bytes().to_vec())
}

fn encode_bytes(value: Vec<u8>) -> Vec<u8> {
    let mut out = encode_varint(value.len() as u64);
    out.extend(value);
    out
}

fn encode_message(value: Vec<u8>) -> Vec<u8> {
    encode_bytes(value)
}

fn encode_varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
    out
}

fn decode_varint(bytes: &[u8], mut pos: usize) -> Result<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0u32;
    while pos < bytes.len() {
        let byte = bytes[pos];
        pos += 1;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok((result, pos));
        }
        shift += 7;
        if shift >= 64 {
            bail!("protobuf varint too long");
        }
    }
    bail!("protobuf varint truncated")
}

fn parse_fields(bytes: &[u8]) -> Result<Vec<(u64, u8, ProtoValue)>> {
    let mut fields = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let (tag, next) = decode_varint(bytes, pos)?;
        pos = next;
        let field_number = tag >> 3;
        let wire_type = (tag & 0x07) as u8;
        match wire_type {
            0 => {
                let (value, next) = decode_varint(bytes, pos)?;
                pos = next;
                fields.push((field_number, wire_type, ProtoValue::Varint(value)));
            }
            1 => {
                let end = pos + 8;
                if end > bytes.len() {
                    bail!("protobuf fixed64 field truncated");
                }
                fields.push((
                    field_number,
                    wire_type,
                    ProtoValue::Fixed64(bytes[pos..end].to_vec()),
                ));
                pos = end;
            }
            2 => {
                let (length, next) = decode_varint(bytes, pos)?;
                pos = next;
                let end = pos + length as usize;
                if end > bytes.len() {
                    bail!("protobuf length-delimited field truncated");
                }
                fields.push((
                    field_number,
                    wire_type,
                    ProtoValue::Len(bytes[pos..end].to_vec()),
                ));
                pos = end;
            }
            5 => {
                let end = pos + 4;
                if end > bytes.len() {
                    bail!("protobuf fixed32 field truncated");
                }
                fields.push((
                    field_number,
                    wire_type,
                    ProtoValue::Fixed32(bytes[pos..end].to_vec()),
                ));
                pos = end;
            }
            _ => bail!("unsupported protobuf wire type {wire_type}"),
        }
    }
    Ok(fields)
}

fn fields_to_map(
    fields: Vec<(u64, u8, ProtoValue)>,
) -> std::collections::BTreeMap<u64, Vec<ProtoValue>> {
    let mut map = std::collections::BTreeMap::new();
    for (field, _wire_type, value) in fields {
        map.entry(field).or_insert_with(Vec::new).push(value);
    }
    map
}

fn get_varint(map: &std::collections::BTreeMap<u64, Vec<ProtoValue>>, field: u64) -> Option<u64> {
    map.get(&field)?.iter().find_map(|value| match value {
        ProtoValue::Varint(value) => Some(*value),
        _ => None,
    })
}

fn get_string(
    map: &std::collections::BTreeMap<u64, Vec<ProtoValue>>,
    field: u64,
) -> Option<String> {
    get_bytes(map, field).and_then(|bytes| String::from_utf8(bytes).ok())
}

fn get_bytes(
    map: &std::collections::BTreeMap<u64, Vec<ProtoValue>>,
    field: u64,
) -> Option<Vec<u8>> {
    map.get(&field)?.iter().find_map(|value| match value {
        ProtoValue::Len(value) => Some(value.clone()),
        _ => None,
    })
}

fn get_repeated_bytes(
    map: &std::collections::BTreeMap<u64, Vec<ProtoValue>>,
    field: u64,
) -> Vec<Vec<u8>> {
    map.get(&field)
        .into_iter()
        .flatten()
        .filter_map(|value| match value {
            ProtoValue::Len(value) => Some(value.clone()),
            _ => None,
        })
        .collect()
}

fn text_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if current.len() + character.len_utf8() > YUANBAO_TEXT_LIMIT && !current.is_empty() {
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
    fn yuanbao_direct_conversation_is_prefixed() {
        assert_eq!(
            yuanbao_conversation_id(&json!({"direct_account": "alice"})).as_deref(),
            Some("direct:alice")
        );
    }

    #[test]
    fn yuanbao_group_conversation_is_prefixed() {
        assert_eq!(
            yuanbao_conversation_id(&json!({"group_code": "g1"})).as_deref(),
            Some("group:g1")
        );
    }

    #[test]
    fn yuanbao_group_policy_defaults_to_mention() -> Result<()> {
        let adapter = new_adapter(
            &GatewayChannelConfig::default(),
            &GatewayCredentialEntry {
                channel: "yuanbao".to_string(),
                ..Default::default()
            },
        )?;
        assert_eq!(adapter.group_policy, "mention");
        Ok(())
    }

    #[test]
    fn yuanbao_mentions_bot_accepts_at_flag() {
        assert!(yuanbao_mentions_bot(
            &json!({"is_at_bot": true}),
            &HashSet::new()
        ));
    }

    #[test]
    fn yuanbao_adapter_has_typing_and_media() -> Result<()> {
        let adapter = new_adapter(
            &GatewayChannelConfig::default(),
            &GatewayCredentialEntry {
                channel: "yuanbao".to_string(),
                ..Default::default()
            },
        )?;
        let capabilities = adapter.capabilities();
        assert!(capabilities.media);
        assert!(capabilities.typing);
        Ok(())
    }
}
